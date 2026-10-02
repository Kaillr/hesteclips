//! macOS file writing with AVAssetWriter, in-process: the `FileWriter` behind
//! `crate::writer`.
//!
//! Track names and the mix tag are written into the file afterwards
//! (`mp4meta`), because AVAssetWriter can't put them where players look.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use objc2::AllocAnyThread;
use objc2::rc::Retained;
use objc2_av_foundation::{
    AVAssetWriter, AVAssetWriterInput, AVAssetWriterStatus, AVFileTypeMPEG4, AVFileTypeQuickTimeMovie,
    AVMediaTypeAudio, AVMediaTypeVideo, AVMetadataIdentifieriTunesMetadataUserComment, AVMutableMetadataItem,
};
use objc2_core_foundation::CFRetained;
use objc2_core_media::{CMFormatDescription, CMSampleBuffer, CMTime};
use objc2_foundation::{NSArray, NSString, NSURL};

use crate::aac::packet_sample;
use crate::mixer::RATE;
use crate::writer::{AacPacket, Media};

/// An encoded video frame: a CMSampleBuffer from VideoToolbox.
pub(crate) type VideoFrame = SendSample;

/// A CMSampleBuffer moved between threads (retain counts are atomic; we never
/// touch one from two threads at once).
#[derive(Clone)]
pub(crate) struct SendSample(pub CFRetained<CMSampleBuffer>);
unsafe impl Send for SendSample {}

/// What the writer needs to create its outputs.
#[derive(Clone)]
pub(crate) struct Layout {
    pub video_format: SendFormat,
    /// One per audio track, in file order.
    pub audio_formats: Vec<SendFormat>,
    pub audio_titles: Vec<String>,
    /// Stored as the file's comment (the mix tag).
    pub comment: String,
}

impl Layout {
    pub(crate) fn audio_tracks(&self) -> usize {
        self.audio_formats.len()
    }
}

#[derive(Clone)]
pub(crate) struct SendFormat(pub CFRetained<CMFormatDescription>);
unsafe impl Send for SendFormat {}
unsafe impl Sync for SendFormat {}

pub(crate) struct FileWriter {
    writer: Retained<AVAssetWriter>,
    video: Retained<AVAssetWriterInput>,
    audio: Vec<Retained<AVAssetWriterInput>>,
    audio_formats: Vec<SendFormat>,
    /// Capture time of the file's first frame; written as t=0.
    start: f64,
    fps: u32,
    path: PathBuf,
    audio_titles: Vec<String>,
}

impl FileWriter {
    pub(crate) fn create(path: &Path, layout: &Layout, start: f64, fps: u32, live: bool) -> Result<Self> {
        let _ = std::fs::remove_file(path);
        let file_type = match path.extension().and_then(|e| e.to_str()) {
            Some("mov") => unsafe { AVFileTypeQuickTimeMovie },
            _ => unsafe { AVFileTypeMPEG4 },
        }
        .context("AVFoundation file type missing")?;
        unsafe {
            let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
            let writer = AVAssetWriter::initWithURL_fileType_error(AVAssetWriter::alloc(), &url, file_type)
                .map_err(|e| anyhow!("can't create {}: {}", path.display(), e.localizedDescription()))?;
            if live {
                // Fragments every second: a crash loses at most the last one.
                writer.setMovieFragmentInterval(CMTime::new(1, 1));
            } else {
                // Clips are short; index up front so they start instantly when streamed.
                writer.setShouldOptimizeForNetworkUse(true);
            }
            let video = passthrough_input(AVMediaTypeVideo.context("video media type")?, &layout.video_format.0);
            if !writer.canAddInput(&video) {
                bail!("can't add the video track");
            }
            writer.addInput(&video);
            let mut audio = Vec::new();
            for fmt in &layout.audio_formats {
                let input = passthrough_input(AVMediaTypeAudio.context("audio media type")?, &fmt.0);
                if !writer.canAddInput(&input) {
                    bail!("can't add an audio track");
                }
                writer.addInput(&input);
                audio.push(input);
            }
            if !layout.comment.is_empty() {
                writer.setMetadata(&NSArray::from_retained_slice(&[comment_item(&layout.comment)]));
            }
            if !writer.startWriting() {
                bail!("couldn't start writing: {}", writer_error(&writer));
            }
            writer.startSessionAtSourceTime(CMTime::new(0, 1));
            Ok(Self {
                writer,
                video,
                audio,
                audio_formats: layout.audio_formats.clone(),
                start,
                fps,
                path: path.to_path_buf(),
                audio_titles: layout.audio_titles.clone(),
            })
        }
    }

    pub(crate) fn append(&mut self, media: &Media) -> Result<()> {
        let (input, sample) = match media {
            Media::Video { frame, pts, .. } => {
                // Exact CFR timestamps relative to the file start.
                let n = ((pts - self.start) * self.fps as f64).round() as i64;
                (&self.video, retime_video(&frame.0, n, self.fps)?)
            }
            Media::Audio { track, packet } => {
                let Some(input) = self.audio.get(*track) else { return Ok(()) };
                let start_frame = (self.start * RATE as f64).round() as i64;
                if packet.frame < start_frame {
                    return Ok(());
                }
                let shifted = AacPacket { data: packet.data.clone(), frame: packet.frame - start_frame };
                let fmt = &self.audio_formats[*track].0;
                (input, packet_sample(&shifted, fmt).context("couldn't wrap an audio packet")?)
            }
        };
        unsafe {
            // Real-time inputs interleave themselves; this only waits when the disk
            // briefly can't keep up.
            let mut waited = 0;
            while !input.isReadyForMoreMediaData() {
                if self.writer.status() != AVAssetWriterStatus::Writing {
                    bail!("writing failed: {}", writer_error(&self.writer));
                }
                thread::sleep(Duration::from_millis(1));
                waited += 1;
                if waited > 5000 {
                    bail!("the file writer stopped accepting data");
                }
            }
            if !input.appendSampleBuffer(&sample) {
                bail!("writing failed: {}", writer_error(&self.writer));
            }
        }
        Ok(())
    }

    /// Finish the file and name its tracks the way players read them.
    pub(crate) fn finish(self) -> Result<()> {
        unsafe {
            self.video.markAsFinished();
            for a in &self.audio {
                a.markAsFinished();
            }
            let (tx, rx) = mpsc::channel();
            let block = block2::RcBlock::new(move || {
                let _ = tx.send(());
            });
            self.writer.finishWritingWithCompletionHandler(&block);
            let _ = rx.recv_timeout(Duration::from_secs(30));
            if self.writer.status() != AVAssetWriterStatus::Completed {
                bail!("couldn't finish the file: {}", writer_error(&self.writer));
            }
        }
        crate::mp4meta::name_tracks(&self.path, "Video", &self.audio_titles)
    }
}

fn passthrough_input(kind: &NSString, format: &CMFormatDescription) -> Retained<AVAssetWriterInput> {
    unsafe {
        let input = AVAssetWriterInput::initWithMediaType_outputSettings_sourceFormatHint(
            AVAssetWriterInput::alloc(),
            kind,
            None,
            Some(format),
        );
        // Lets the writer interleave tracks as data arrives instead of waiting
        // for every track to catch up (which deadlocks a single feeding thread).
        input.setExpectsMediaDataInRealTime(true);
        input
    }
}

fn comment_item(text: &str) -> Retained<objc2_av_foundation::AVMetadataItem> {
    unsafe {
        let item = AVMutableMetadataItem::new();
        item.setIdentifier(AVMetadataIdentifieriTunesMetadataUserComment);
        item.setValue(Some(&NSString::from_str(text)));
        item.setExtendedLanguageTag(Some(&NSString::from_str("und")));
        Retained::into_super(item)
    }
}

fn writer_error(w: &AVAssetWriter) -> String {
    unsafe { w.error() }.map_or_else(|| "unknown error".into(), |e| e.localizedDescription().to_string())
}

/// A copy of a video sample with presentation/decode time `frame / fps`.
fn retime_video(sample: &CMSampleBuffer, frame: i64, fps: u32) -> Result<CFRetained<CMSampleBuffer>> {
    use objc2_core_media::CMSampleTimingInfo;
    let t = unsafe { CMTime::new(frame, fps as i32) };
    let timing = CMSampleTimingInfo { duration: unsafe { CMTime::new(1, fps as i32) }, presentationTimeStamp: t, decodeTimeStamp: t };
    let mut out: *mut CMSampleBuffer = std::ptr::null_mut();
    let status = unsafe {
        CMSampleBuffer::create_copy_with_new_timing(None, sample, 1, &timing, std::ptr::NonNull::from(&mut out))
    };
    std::ptr::NonNull::new(out)
        .filter(|_| status == 0)
        .map(|p| unsafe { CFRetained::from_raw(p) })
        .context("couldn't retime a video frame")
}
