//! Writing captured media to MP4/MOV with AVAssetWriter, in-process.
//!
//! The recorder sends [`Media`] (encoded H.264 frames from VideoToolbox, AAC
//! packets per audio track) to one writer thread with exact timestamps:
//!
//! - **Record**: everything goes straight into an AVAssetWriter, which writes a
//!   fragmented MP4 — if the app dies mid-recording, everything up to the last
//!   fragment (≤ 1 s) still plays.
//! - **Replay buffer**: encoded media stays in a ring in memory, a little longer
//!   than the replay window. "Save clip" takes everything from the last keyframe
//!   at or before (now − replay window) and writes it to a file on another
//!   thread, so capture never pauses. Nothing touches the disk until you save.
//!
//! Track names and the mix tag are written into the file afterwards
//! (`mp4meta`), because AVAssetWriter can't put them where players look.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use objc2::rc::Retained;
use objc2::AllocAnyThread;
use objc2_av_foundation::{
    AVAssetWriter, AVAssetWriterInput, AVAssetWriterStatus, AVFileTypeMPEG4, AVFileTypeQuickTimeMovie,
    AVMediaTypeAudio, AVMediaTypeVideo, AVMetadataIdentifieriTunesMetadataUserComment, AVMutableMetadataItem,
};
use objc2_core_foundation::CFRetained;
use objc2_core_media::{CMFormatDescription, CMSampleBuffer, CMTime};
use objc2_foundation::{NSArray, NSString, NSURL};

use crate::aac::{AacPacket, packet_sample};
use crate::mixer::RATE;

/// Something to write: a video frame, or one AAC packet for audio track `track`.
pub(crate) enum Media {
    Video { sample: SendSample, pts: f64, key: bool },
    Audio { track: usize, packet: AacPacket },
}

/// A CMSampleBuffer moved between threads (retain counts are atomic; we never
/// touch one from two threads at once).
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

#[derive(Clone)]
pub(crate) struct SendFormat(pub CFRetained<CMFormatDescription>);
unsafe impl Send for SendFormat {}
unsafe impl Sync for SendFormat {}

pub(crate) enum Command {
    Media(Media),
    /// Replay buffer: write the last `seconds` to `out`, report the result.
    SaveClip { out: PathBuf, seconds: f64, done: Sender<Result<PathBuf>> },
}

/// The writer thread. Dropping the sender ends it; `join` finishes the file.
pub(crate) struct Writer {
    pub tx: Sender<Command>,
    thread: Option<JoinHandle<Result<()>>>,
}

impl Writer {
    /// Record straight to `file` (hidden partial path; the caller renames it).
    pub(crate) fn record(file: PathBuf, layout: Layout, fps: u32) -> Self {
        Self::spawn(move |rx| record_loop(rx, &file, &layout, fps))
    }

    /// Keep the last `window` seconds (plus slack) in memory until asked to save.
    pub(crate) fn replay(window: f64, layout: Layout, fps: u32) -> Self {
        Self::spawn(move |rx| replay_loop(rx, window, layout, fps))
    }

    fn spawn(f: impl FnOnce(Receiver<Command>) -> Result<()> + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel();
        Self { tx, thread: Some(thread::spawn(move || f(rx))) }
    }

    /// Wait for the writer to finish (after every sender is dropped).
    pub(crate) fn join(mut self) -> Result<()> {
        drop(self.tx);
        match self.thread.take().map(JoinHandle::join) {
            Some(Ok(r)) => r,
            Some(Err(_)) => bail!("the file writer crashed"),
            None => Ok(()),
        }
    }
}

// ---------------------------------------------------------------------------
// Record
// ---------------------------------------------------------------------------

fn record_loop(rx: Receiver<Command>, file: &Path, layout: &Layout, fps: u32) -> Result<()> {
    let mut file_writer: Option<FileWriter> = None;
    let mut first_error: Option<anyhow::Error> = None;
    while let Ok(cmd) = rx.recv() {
        let Command::Media(media) = cmd else { continue };
        if first_error.is_some() {
            continue; // keep draining so capture never blocks on us
        }
        // Start the file at the first keyframe, so it opens on a clean picture.
        if file_writer.is_none() {
            let Media::Video { pts, key: true, .. } = &media else { continue };
            match FileWriter::create(file, layout, *pts, fps, true) {
                Ok(w) => file_writer = Some(w),
                Err(e) => {
                    first_error = Some(e);
                    continue;
                }
            }
        }
        if let Err(e) = file_writer.as_mut().unwrap().append(&media) {
            first_error = Some(e);
        }
    }
    let Some(w) = file_writer else {
        return Err(first_error.unwrap_or_else(|| anyhow!("nothing was recorded")));
    };
    w.finish()?;
    finalize_metadata(file, layout)?;
    first_error.map_or(Ok(()), Err)
}

// ---------------------------------------------------------------------------
// Replay buffer
// ---------------------------------------------------------------------------

/// Media kept in memory, oldest first.
struct Ring {
    items: VecDeque<(f64, Media)>,
    /// How much to keep: the replay window plus room to start on a keyframe.
    keep: f64,
}

impl Ring {
    fn push(&mut self, media: Media) {
        let t = media_time(&media);
        self.items.push_back((t, media));
        // Drop whole keyframe intervals from the front once the rest still covers
        // `keep` — the ring must always start on a keyframe.
        loop {
            let newest = self.items.back().map_or(t, |(t, _)| *t);
            let next_key = self.items.iter().skip(1).find_map(|(t, m)| matches!(m, Media::Video { key: true, .. }).then_some(*t));
            match next_key {
                Some(k) if newest - k >= self.keep => {
                    while self.items.front().is_some_and(|(t, _)| *t < k) {
                        self.items.pop_front();
                    }
                }
                _ => break,
            }
        }
    }

    /// Everything from the last keyframe at or before `end - seconds` up to `end`,
    /// where `end` is the newest moment every track has reached (audio runs a
    /// little behind video, so cutting at the newest frame would leave the clip's
    /// last fraction of a second silent). Shares the sample buffers.
    fn snapshot(&self, seconds: f64, audio_tracks: usize) -> Vec<(f64, Media)> {
        let newest_video = self.items.iter().rev().find_map(|(t, m)| matches!(m, Media::Video { .. }).then_some(*t));
        let Some(newest_video) = newest_video else { return Vec::new() };
        let newest_audio = (0..audio_tracks)
            .map(|k| {
                self.items.iter().rev().find_map(|(t, m)| match m {
                    Media::Audio { track, .. } if *track == k => Some(*t),
                    _ => None,
                })
            })
            .collect::<Option<Vec<f64>>>()
            .and_then(|v| v.into_iter().reduce(f64::min));
        let newest = match (audio_tracks, newest_audio) {
            (0, _) => newest_video,
            (_, Some(a)) => newest_video.min(a),
            (_, None) => return Vec::new(),
        };
        let want = newest - seconds;
        let start = self
            .items
            .iter()
            .rev()
            .find(|(t, m)| matches!(m, Media::Video { key: true, .. }) && *t <= want + 1e-6)
            .map(|(t, _)| *t)
            .or_else(|| self.items.iter().find_map(|(t, m)| matches!(m, Media::Video { key: true, .. }).then_some(*t)));
        let Some(start) = start else { return Vec::new() };
        self.items
            .iter()
            .filter(|(t, _)| *t >= start - 1e-6 && *t <= newest + 1e-6)
            .map(|(t, m)| (*t, m.share()))
            .collect()
    }
}

fn replay_loop(rx: Receiver<Command>, window: f64, layout: Layout, fps: u32) -> Result<()> {
    let mut ring = Ring { items: VecDeque::new(), keep: window + 2.0 };
    let mut saves: Vec<JoinHandle<()>> = Vec::new();
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Command::Media(m) => ring.push(m),
            Command::SaveClip { out, seconds, done } => {
                let items = ring.snapshot(seconds, layout.audio_formats.len());
                let layout = layout.clone();
                // Writing happens off this thread so the ring keeps filling.
                saves.push(thread::spawn(move || {
                    let _ = done.send(write_clip(&out, &items, &layout, fps));
                }));
                saves.retain(|h| !h.is_finished());
            }
        }
    }
    // Let pending saves finish before the app exits.
    for h in saves {
        let _ = h.join();
    }
    Ok(())
}

fn write_clip(out: &Path, items: &[(f64, Media)], layout: &Layout, fps: u32) -> Result<PathBuf> {
    let Some((start, _)) = items.iter().find(|(_, m)| matches!(m, Media::Video { key: true, .. })) else {
        bail!("no buffered footage yet — give it a second");
    };
    let mut w = FileWriter::create(out, layout, *start, fps, false)?;
    for (_, m) in items {
        w.append(m)?;
    }
    w.finish()?;
    finalize_metadata(out, layout)?;
    Ok(out.to_path_buf())
}

// ---------------------------------------------------------------------------
// AVAssetWriter
// ---------------------------------------------------------------------------

struct FileWriter {
    writer: Retained<AVAssetWriter>,
    video: Retained<AVAssetWriterInput>,
    audio: Vec<Retained<AVAssetWriterInput>>,
    audio_formats: Vec<SendFormat>,
    /// Capture time of the file's first frame; written as t=0.
    start: f64,
    fps: u32,
}

impl FileWriter {
    fn create(path: &Path, layout: &Layout, start: f64, fps: u32, live: bool) -> Result<Self> {
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
            Ok(Self { writer, video, audio, audio_formats: layout.audio_formats.clone(), start, fps })
        }
    }

    fn append(&mut self, media: &Media) -> Result<()> {
        let (input, sample) = match media {
            Media::Video { sample, pts, .. } => {
                // Exact CFR timestamps relative to the file start.
                let frame = ((pts - self.start) * self.fps as f64).round() as i64;
                (&self.video, retime_video(&sample.0, frame, self.fps)?)
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

    fn finish(self) -> Result<()> {
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
        Ok(())
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

/// Name the tracks the way players read them.
fn finalize_metadata(path: &Path, layout: &Layout) -> Result<()> {
    crate::mp4meta::name_tracks(path, "Video", &layout.audio_titles)
}

fn media_time(m: &Media) -> f64 {
    match m {
        Media::Video { pts, .. } => *pts,
        Media::Audio { packet, .. } => packet.frame as f64 / RATE as f64,
    }
}

impl Media {
    /// Another handle to the same media (sample buffers are shared, AAC bytes copied).
    fn share(&self) -> Media {
        match self {
            Media::Video { sample, pts, key } => Media::Video { sample: SendSample(sample.0.clone()), pts: *pts, key: *key },
            Media::Audio { track, packet } => {
                Media::Audio { track: *track, packet: AacPacket { data: packet.data.clone(), frame: packet.frame } }
            }
        }
    }
}
