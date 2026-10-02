//! Native macOS capture backend: ScreenCaptureKit + VideoToolbox + AVAssetWriter,
//! all in-process — no external tools.
//!
//! - **Video**: an `SCStream` delivers screen frames (already scaled to the output
//!   size, NV12). A pacer thread feeds the newest frame to a VideoToolbox H.264
//!   encoder at exactly `fps`, so the output is constant frame rate even when the
//!   screen is static and SCK sends nothing.
//! - **Desktop and app audio**: one audio-only `SCStream` per source
//!   ([`SystemAudio`]), filtered to that app (or to everything minus the apps
//!   added separately). **Mics** use cpal.
//! - **Mixing** happens in Rust (`crate::mixer`), which also drives the live meters;
//!   each track is AAC-encoded with Apple's encoder (`crate::aac`).
//! - **Writing** (`crate::writer`): AVAssetWriter, either straight to a fragmented
//!   MP4 (record) or from an in-memory replay ring when a clip is saved.
//!
//! Everything is timestamped on the host clock: the first video frame is t=0.

use std::ffi::c_void;
use std::path::PathBuf;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send};
use objc2_core_audio_types::{
    AudioBuffer, AudioBufferList, AudioStreamBasicDescription, kAudioFormatFlagIsNonInterleaved,
};
use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_graphics::{
    CGDisplayCopyDisplayMode, CGDisplayMode, CGMainDisplayID, kCGDisplayStreamYCbCrMatrix_ITU_R_709_2,
};
use objc2_core_media::{
    CMAudioFormatDescriptionGetStreamBasicDescription, CMBlockBuffer, CMClock, CMSampleBuffer, CMTime,
    kCMSampleAttachmentKey_NotSync,
    kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment, kCMTimeInvalid,
    kCMVideoCodecType_H264,
};
use objc2_core_video::{
    CVPixelBuffer, kCVImageBufferColorPrimaries_ITU_R_709_2,
    kCVImageBufferTransferFunction_ITU_R_709_2, kCVImageBufferYCbCrMatrix_ITU_R_709_2,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCRunningApplication, SCShareableContent, SCStream,
    SCStreamConfiguration, SCStreamOutput, SCStreamOutputType,
};
use objc2_video_toolbox::{
    VTCompressionSession, VTEncodeInfoFlags, VTSessionSetProperty,
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_ColorPrimaries, kVTCompressionPropertyKey_ExpectedFrameRate,
    kVTCompressionPropertyKey_MaxKeyFrameInterval,
    kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration, kVTCompressionPropertyKey_ProfileLevel,
    kVTCompressionPropertyKey_RealTime, kVTCompressionPropertyKey_TransferFunction,
    kVTCompressionPropertyKey_YCbCrMatrix, kVTProfileLevel_H264_High_AutoLevel,
    kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder,
    kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
};

use crate::aac::AacEncoder;
use crate::mixer::{self, Channel, Clock, LiveAudio, SourceFeed, SourceStatus};
use crate::output::{in_progress, timestamp};
use crate::sources::{AudioCapture, AudioSource, SourceKind, mix_inputs, track_layout};
use crate::avwriter::{SendFormat, SendSample};
use crate::writer::{self, Layout, Media, Writer};
use crate::{EncodeSettings, Mode, Recorder};

use crate::AUDIO_BITRATE;

pub struct SckRecorder {
    session: Option<Session>,
    mode: Option<Mode>,
    current_file: Option<PathBuf>,
    replay_seconds: u32,
    output_dir: PathBuf,
    container_ext: String,
    live: Arc<LiveAudio>,
}

impl SckRecorder {
    /// `live` receives the meters and supplies each source's volume.
    pub fn new(live: Arc<LiveAudio>) -> Self {
        Self {
            session: None,
            mode: None,
            current_file: None,
            replay_seconds: 60,
            output_dir: PathBuf::from("."),
            container_ext: "mp4".to_string(),
            live,
        }
    }
}

impl Recorder for SckRecorder {
    fn start(&mut self, mode: Mode, settings: &EncodeSettings) -> Result<()> {
        if self.session.is_some() {
            let _ = self.stop();
        }
        self.replay_seconds = settings.replay_seconds;
        self.output_dir = settings.output_dir.clone();
        // AVAssetWriter writes MP4 and MOV; MKV isn't one of them.
        self.container_ext = if settings.container_ext == "mov" { "mov".into() } else { "mp4".into() };
        std::fs::create_dir_all(&settings.output_dir)?;

        let mut s = settings.clone();
        let target = match mode {
            Mode::Record => {
                let file = self.output_dir.join(format!("recording_{}.{}", timestamp(), self.container_ext));
                self.current_file = Some(file.clone());
                Target::Record(in_progress(&file))
            }
            Mode::ReplayBuffer => {
                // 1 s keyframes: a saved clip starts at most 1 s before the window.
                s.keyframe_interval_secs = 1;
                self.current_file = None;
                Target::Replay(settings.replay_seconds as f64)
            }
        };
        self.session = Some(Session::start(&s, target, &self.live)?);
        self.mode = Some(mode);
        Ok(())
    }

    fn save_clip(&mut self) -> Result<crate::PendingClip> {
        let session = self.session.as_ref().filter(|_| self.mode == Some(Mode::ReplayBuffer));
        let session = session.context("replay buffer is not running")?;
        writer::request_clip(&session.writer_tx, &self.output_dir, &self.container_ext, self.replay_seconds as f64)
    }

    fn stop(&mut self) -> Result<Option<PathBuf>> {
        let finished = self.session.take().map(Session::finish);
        let file = self.current_file.take();
        let was_record = self.mode == Some(Mode::Record);
        self.mode = None;
        if !was_record {
            return Ok(None);
        }
        let Some(file) = file else { return Ok(None) };
        let partial = in_progress(&file);
        if let Some(Err(e)) = finished {
            // A fragmented MP4 is still playable up to the failure: keep what we got.
            if !partial.exists() {
                return Err(e.context("recording failed"));
            }
            eprintln!("recording ended with an error: {e:#}");
        }
        if !partial.exists() {
            bail!("recording failed — nothing was written");
        }
        std::fs::rename(&partial, &file).context("couldn't finish the recording")?;
        Ok(Some(file))
    }

    fn is_running(&self) -> bool {
        self.session.is_some()
    }
}

impl Drop for SckRecorder {
    /// Dropping (app quit) must still finalise the file.
    fn drop(&mut self) {
        if self.session.is_some() {
            let _ = self.stop();
        }
    }
}

/// Where a session's media goes.
enum Target {
    /// Straight to this (hidden, partial) file.
    Record(PathBuf),
    /// An in-memory ring of this many seconds.
    Replay(f64),
}

// ---------------------------------------------------------------------------
// Session: one running capture
// ---------------------------------------------------------------------------

/// How far behind real time the mixer runs, so every source's audio has arrived
/// (SCK and CoreAudio deliver in ~10–20 ms blocks, sometimes late). Timestamps
/// are exact, so this only delays writing — it never shifts audio against video.
const MIX_LATENCY: f64 = 0.3;

/// Everything a running capture owns. Torn down in `finish` in an order that
/// lets every encoder flush into the writer before it closes the file.
struct Session {
    /// The screen stream and its callback object (kept alive while it may call in).
    video: Option<(Retained<SCStream>, Retained<StreamOutput>)>,
    pacer: Option<(Arc<AtomicBool>, JoinHandle<()>)>,
    audio: Option<AudioCapture>,
    mixer: Option<(Arc<AtomicBool>, JoinHandle<()>)>,
    /// AAC encoder threads, one per track; they end when the mixer drops its senders.
    encoders: Vec<JoinHandle<()>>,
    writer_tx: Sender<writer::Command>,
    writer: Option<Writer>,
}

impl Session {
    fn start(s: &EncodeSettings, target: Target, live: &Arc<LiveAudio>) -> Result<Self> {
        let content = shareable_content()?;
        let screen_id = match &s.video {
            crate::VideoSource::Screen { id } => id.as_str(),
            crate::VideoSource::Apps { .. } => bail!("recording games and apps isn't available on macOS yet"),
        };
        let display = pick_display(&content, screen_id).context("no display to capture")?;
        let (width, height) = output_size(&display, s.target_height);
        let clock = Arc::new(Clock::default());

        // The writer needs every track's format up front: the video format comes
        // from a throwaway encode, the audio ones from the AAC encoders.
        let (titles, comment, has_rest) = track_layout(&s.sources);
        let aac: Vec<AacEncoder> = titles.iter().map(|_| AacEncoder::new(AUDIO_BITRATE)).collect::<Result<_>>()?;
        let layout = Layout {
            video_format: SendFormat(probe_video_format(width, height, s)?),
            audio_formats: aac.iter().map(|e| SendFormat(retain_format(e.format()))).collect(),
            audio_titles: titles.clone(),
            comment,
        };
        let writer = match target {
            Target::Record(file) => Writer::record(file, layout, s.fps),
            Target::Replay(seconds) => Writer::replay(seconds, layout, s.fps),
        };
        let writer_tx = writer.tx.clone();

        let mut session =
            Self { video: None, pacer: None, audio: None, mixer: None, encoders: Vec::new(), writer_tx, writer: Some(writer) };
        let started = (|| -> Result<()> {
            // One PCM channel + AAC thread per track, in layout order: the mix,
            // each own track, the rest.
            let mut pcm_senders = Vec::new();
            for (track, enc) in aac.into_iter().enumerate() {
                let (tx, rx) = mpsc::channel::<Vec<f32>>();
                pcm_senders.push(tx);
                session.encoders.push(spawn_aac(track, enc, rx, session.writer_tx.clone()));
            }
            let mut tracks = pcm_senders.into_iter();
            let mix_track = tracks.next();
            let audio = AudioCapture::start(&s.sources, live, clock.clone())?;
            let inputs = mix_inputs(&s.sources, &audio.feeds, live, |_| tracks.next());
            let rest_track = if has_rest { tracks.next() } else { None };
            session.audio = Some(audio);
            let stop = Arc::new(AtomicBool::new(false));
            let mixer = mixer::spawn_mixer(
                inputs,
                mix_track,
                rest_track,
                live.clone(),
                clock.clone(),
                host_now,
                MIX_LATENCY,
                stop.clone(),
            );
            session.mixer = Some((stop, mixer));

            let encoder = Encoder::new(width, height, s, session.writer_tx.clone())?;
            let shared = Arc::new(Shared::default());
            let output = StreamOutput::new(shared.clone());
            let stream = make_video_stream(&display, width, height, s.fps, &output)?;
            start_capture(&stream)?;
            session.video = Some((stream, output));

            // Wait for the first frame so a capture that can't see the screen fails
            // here, visibly, instead of producing an empty file.
            let deadline = Instant::now() + Duration::from_secs(3);
            while shared.latest.lock().unwrap().is_none() {
                if Instant::now() >= deadline {
                    bail!("the screen isn't delivering frames — check Screen Recording permission");
                }
                thread::sleep(Duration::from_millis(10));
            }
            session.pacer = Some(spawn_pacer(s.fps, shared, encoder, clock));
            Ok(())
        })();
        if let Err(e) = started {
            let _ = session.finish();
            return Err(e);
        }
        Ok(session)
    }

    /// Stop capturing and finish the file (record) or drop the ring (replay).
    fn finish(mut self) -> Result<()> {
        if let Some((stream, _)) = &self.video {
            stop_capture(stream);
        }
        // The pacer flushes the video encoder on its way out.
        if let Some((stop, handle)) = self.pacer.take() {
            stop.store(true, Ordering::Relaxed);
            let _ = handle.join();
        }
        if let Some(audio) = self.audio.take() {
            audio.stop();
        }
        // The mixer writes what's left and drops its senders; the AAC threads then
        // flush and exit.
        if let Some((stop, handle)) = self.mixer.take() {
            stop.store(true, Ordering::Relaxed);
            let _ = handle.join();
        }
        for h in self.encoders.drain(..) {
            let _ = h.join();
        }
        drop(self.writer_tx);
        self.writer.take().map_or(Ok(()), Writer::join)
    }
}

/// AAC-encode one track's PCM and pass the packets to the writer.
fn spawn_aac(track: usize, mut enc: AacEncoder, rx: mpsc::Receiver<Vec<f32>>, out: Sender<writer::Command>) -> JoinHandle<()> {
    thread::spawn(move || {
        while let Ok(pcm) = rx.recv() {
            for packet in enc.push(&pcm) {
                if out.send(writer::Command::Media(Media::Audio { track, packet })).is_err() {
                    return;
                }
            }
        }
    })
}

fn retain_format(f: &objc2_core_media::CMFormatDescription) -> CFRetained<objc2_core_media::CMFormatDescription> {
    unsafe { CFRetained::retain(NonNull::from(f)) }
}

/// Current host time in seconds — the clock SCK and CoreAudio timestamps use.
pub(crate) fn host_now() -> f64 {
    unsafe { CMClock::host_time_clock().time().seconds() }
}

// ---------------------------------------------------------------------------
// ScreenCaptureKit
// ---------------------------------------------------------------------------

/// Wrapper to move ObjC/CF objects that are thread-safe in practice (retain counts
/// are atomic; we never mutate them concurrently) across threads.
struct SendBox<T>(T);
unsafe impl<T> Send for SendBox<T> {}

/// What a stream's callbacks feed: the newest screen frame (for the pacer) and/or
/// a source's audio.
#[derive(Default)]
struct Shared {
    latest: Mutex<Option<SendBox<CFRetained<CVPixelBuffer>>>>,
    audio: Option<Arc<SourceFeed>>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSObject))]
    #[ivars = Arc<Shared>]
    struct StreamOutput;

    unsafe impl NSObjectProtocol for StreamOutput {}

    unsafe impl SCStreamOutput for StreamOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output(
            &self,
            _stream: &SCStream,
            sample: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            let shared = self.ivars();
            if kind == SCStreamOutputType::Screen {
                // Idle/blank status frames carry no image; keep the previous one.
                if let Some(image) = unsafe { sample.image_buffer() } {
                    *shared.latest.lock().unwrap() = Some(SendBox(image));
                }
            } else if kind == SCStreamOutputType::Audio {
                if let Some((feed, (pcm, channels))) = shared.audio.as_ref().zip(interleaved_pcm(sample)) {
                    let start = unsafe { sample.presentation_time_stamp().seconds() };
                    feed.push(start, &pcm, channels);
                }
            }
        }
    }
);

impl StreamOutput {
    fn new(shared: Arc<Shared>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(shared);
        unsafe { msg_send![super(this), init] }
    }
}

/// SCK delivers float32, usually non-interleaved; flatten to interleaved f32.
/// Returns the samples and the channel count.
fn interleaved_pcm(sample: &CMSampleBuffer) -> Option<(Vec<f32>, usize)> {
    unsafe {
        let desc = sample.format_description()?;
        let asbd: &AudioStreamBasicDescription = &*CMAudioFormatDescriptionGetStreamBasicDescription(&desc);
        let planar = asbd.mFormatFlags & kAudioFormatFlagIsNonInterleaved != 0;
        let flags = kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment;
        // CoreMedia wants exactly the size it asks for (a larger buffer fails with
        // -12737), so query it first. u64 storage keeps the list aligned.
        let mut need = 0usize;
        sample.audio_buffer_list_with_retained_block_buffer(&mut need, ptr::null_mut(), 0, None, None, flags, ptr::null_mut());
        if need < std::mem::size_of::<AudioBufferList>() {
            return None;
        }
        let mut storage = vec![0u64; need.div_ceil(8)];
        let list = storage.as_mut_ptr() as *mut AudioBufferList;
        let mut block: *mut CMBlockBuffer = ptr::null_mut();
        let status = sample.audio_buffer_list_with_retained_block_buffer(ptr::null_mut(), list, need, None, None, flags, &mut block);
        if status != 0 {
            return None;
        }
        // Releases the block buffer (which owns the sample memory) when done.
        let _block = NonNull::new(block).map(|b| CFRetained::from_raw(b));
        let count = (*list).mNumberBuffers as usize;
        let buffers = std::slice::from_raw_parts((&raw const (*list).mBuffers).cast::<AudioBuffer>(), count);
        let plane = |b: &AudioBuffer| std::slice::from_raw_parts(b.mData as *const f32, b.mDataByteSize as usize / 4);
        if !planar || buffers.len() == 1 {
            let channels = asbd.mChannelsPerFrame.max(1) as usize;
            return buffers.first().map(|b| (plane(b).to_vec(), channels));
        }
        let planes: Vec<&[f32]> = buffers.iter().map(plane).collect();
        let frames = planes.iter().map(|p| p.len()).min().unwrap_or(0);
        let mut out = Vec::with_capacity(frames * planes.len());
        for i in 0..frames {
            for p in &planes {
                out.push(p[i]);
            }
        }
        Some((out, planes.len()))
    }
}

fn shareable_content() -> Result<Retained<SCShareableContent>> {
    let (tx, rx) = mpsc::channel::<Result<SendBox<Retained<SCShareableContent>>, String>>();
    let handler = RcBlock::new(move |content: *mut SCShareableContent, err: *mut NSError| {
        let result = match unsafe { Retained::retain(content) } {
            Some(c) => Ok(SendBox(c)),
            None => Err(unsafe { err.as_ref() }
                .map(|e| e.localizedDescription().to_string())
                .unwrap_or_else(|| "unknown error".into())),
        };
        let _ = tx.send(result);
    });
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&handler) };
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(c)) => Ok(c.0),
        Ok(Err(e)) => bail!("can't list screens to capture ({e}) — check Screen Recording permission"),
        Err(_) => bail!("ScreenCaptureKit didn't respond"),
    }
}

/// The display with this id, else the main display, else any display.
fn pick_display(content: &SCShareableContent, id: &str) -> Option<Retained<SCDisplay>> {
    let displays = unsafe { content.displays() };
    let by_id = |want: u32| displays.iter().find(|d| unsafe { d.displayID() } == want);
    id.parse().ok().and_then(by_id).or_else(|| by_id(CGMainDisplayID())).or_else(|| displays.iter().next())
}

/// Output size: the display's native pixels, downscaled to `target_height` if
/// that's smaller. Even dimensions, as H.264 4:2:0 requires.
fn output_size(display: &SCDisplay, target_height: Option<u32>) -> (usize, usize) {
    let id = unsafe { display.displayID() };
    let mode = CGDisplayCopyDisplayMode(id);
    let (mut w, mut h) = (CGDisplayMode::pixel_width(mode.as_deref()), CGDisplayMode::pixel_height(mode.as_deref()));
    if w == 0 || h == 0 {
        (w, h) = unsafe { (display.width() as usize, display.height() as usize) };
    }
    if let Some(t) = target_height.map(|t| t as usize).filter(|&t| t < h) {
        w = (w * t + h / 2) / h;
        h = t;
    }
    (w & !1, h & !1)
}

fn make_video_stream(
    display: &SCDisplay,
    width: usize,
    height: usize,
    fps: u32,
    output: &StreamOutput,
) -> Result<Retained<SCStream>> {
    unsafe {
        let filter = SCContentFilter::initWithDisplay_excludingWindows(
            SCContentFilter::alloc(),
            display,
            &NSArray::new(),
        );
        let config = SCStreamConfiguration::new();
        config.setWidth(width);
        config.setHeight(height);
        config.setMinimumFrameInterval(CMTime::new(1, fps as i32));
        config.setPixelFormat(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange);
        config.setColorMatrix(kCGDisplayStreamYCbCrMatrix_ITU_R_709_2);
        config.setShowsCursor(true);
        // Room for the frame we hold + frames in the encoder without starving SCK.
        config.setQueueDepth(8);
        let stream = SCStream::initWithFilter_configuration_delegate(SCStream::alloc(), &filter, &config, None);
        let queue = DispatchQueue::new("hesteclips.sck.video", None);
        stream
            .addStreamOutput_type_sampleHandlerQueue_error(ProtocolObject::from_ref(output), SCStreamOutputType::Screen, Some(&queue))
            .map_err(|e| anyhow::anyhow!("can't capture the screen: {}", e.localizedDescription()))?;
        Ok(stream)
    }
}

/// An audio-only stream: everything the Mac plays (`include == false`, minus
/// `apps`) or just `apps` (`include == true`).
fn make_audio_stream(
    display: &SCDisplay,
    apps: &[Retained<SCRunningApplication>],
    include: bool,
    output: &StreamOutput,
) -> Result<Retained<SCStream>> {
    unsafe {
        let filter = audio_filter(display, apps, include);
        let config = SCStreamConfiguration::new();
        // SCK always captures video too; make it as cheap as possible.
        config.setWidth(2);
        config.setHeight(2);
        config.setMinimumFrameInterval(CMTime::new(1, 1));
        config.setQueueDepth(3);
        config.setCapturesAudio(true);
        config.setSampleRate(mixer::RATE as isize);
        config.setChannelCount(2);
        // Don't record our own clip previews. Note this excludes by *responsible*
        // app: under `cargo run`, sounds played from the same terminal are muted too.
        config.setExcludesCurrentProcessAudio(true);
        let stream = SCStream::initWithFilter_configuration_delegate(SCStream::alloc(), &filter, &config, None);
        let queue = DispatchQueue::new("hesteclips.sck.audio", None);
        stream
            .addStreamOutput_type_sampleHandlerQueue_error(ProtocolObject::from_ref(output), SCStreamOutputType::Audio, Some(&queue))
            .map_err(|e| anyhow::anyhow!("can't capture audio: {}", e.localizedDescription()))?;
        Ok(stream)
    }
}

fn audio_filter(display: &SCDisplay, apps: &[Retained<SCRunningApplication>], include: bool) -> Retained<SCContentFilter> {
    let apps = NSArray::from_retained_slice(apps);
    let none = NSArray::new();
    unsafe {
        if include {
            SCContentFilter::initWithDisplay_includingApplications_exceptingWindows(SCContentFilter::alloc(), display, &apps, &none)
        } else {
            SCContentFilter::initWithDisplay_excludingApplications_exceptingWindows(SCContentFilter::alloc(), display, &apps, &none)
        }
    }
}

/// Run an SCStream start/stop call and wait for its completion handler.
fn await_completion(call: impl FnOnce(&block2::DynBlock<dyn Fn(*mut NSError)>)) -> Result<(), String> {
    let (tx, rx) = mpsc::channel::<Option<String>>();
    let handler = RcBlock::new(move |err: *mut NSError| {
        let _ = tx.send(unsafe { err.as_ref() }.map(|e| e.localizedDescription().to_string()));
    });
    call(&handler);
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(None) => Ok(()),
        Ok(Some(e)) => Err(e),
        Err(_) => Err("timed out".into()),
    }
}

fn start_capture(stream: &SCStream) -> Result<()> {
    await_completion(|h| unsafe { stream.startCaptureWithCompletionHandler(Some(h)) })
        .map_err(|e| anyhow::anyhow!("couldn't start screen capture: {e}"))
}

fn stop_capture(stream: &SCStream) {
    let _ = await_completion(|h| unsafe { stream.stopCaptureWithCompletionHandler(Some(h)) });
}

// ---------------------------------------------------------------------------
// Desktop + app audio
// ---------------------------------------------------------------------------

/// Captures desktop and app sources, one audio-only stream each. A thread keeps
/// each stream's filter in step with what's running: an app source starts by
/// itself when its app opens (and follows it across relaunches), and a desktop
/// source that excludes app sources keeps excluding them.
pub(crate) struct SystemAudio {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

/// One desktop/app source and its stream, owned by the `SystemAudio` thread.
struct Tap {
    source: AudioSource,
    feed: Arc<SourceFeed>,
    channel: Arc<Channel>,
    stream: Option<(Retained<SCStream>, Retained<StreamOutput>)>,
    /// Pids the current filter was built from, to notice changes.
    pids: Option<Vec<i32>>,
}

impl SystemAudio {
    /// How often to look for apps that opened, quit or relaunched.
    const RESCAN: Duration = Duration::from_secs(1);

    pub(crate) fn start(sources: Vec<(AudioSource, Arc<SourceFeed>, Arc<Channel>)>) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = thread::spawn(move || {
            let mut taps: Vec<Tap> = sources
                .into_iter()
                .map(|(source, feed, channel)| Tap { source, feed, channel, stream: None, pids: None })
                .collect();
            let mut first = true;
            while !stop2.load(Ordering::Relaxed) {
                sync_taps(&mut taps);
                if std::mem::take(&mut first) {
                    let _ = ready_tx.send(());
                }
                let until = Instant::now() + Self::RESCAN;
                while Instant::now() < until && !stop2.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(50));
                }
            }
            for tap in &taps {
                if let Some((stream, _)) = &tap.stream {
                    stop_capture(stream);
                }
                tap.channel.set_status(SourceStatus::Off);
            }
        });
        // Give the first scan a moment, so capture starts with the audio running.
        let _ = ready_rx.recv_timeout(Duration::from_secs(5));
        Ok(Self { stop, thread: Some(thread) })
    }

    pub(crate) fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Whether a running app belongs to an app source: the app itself or one of its
/// helpers (`com.hnc.Discord` → `com.hnc.Discord.helper`), whose processes are
/// often the ones actually playing the sound.
fn belongs_to(app_bundle: &str, source_bundle: &str) -> bool {
    app_bundle == source_bundle
        || app_bundle.strip_prefix(source_bundle).is_some_and(|rest| rest.starts_with('.'))
}

/// Bring every tap's stream in line with the apps running right now.
fn sync_taps(taps: &mut [Tap]) {
    let Ok(content) = shareable_content() else {
        for tap in taps.iter() {
            if tap.stream.is_none() {
                tap.channel.set_status(SourceStatus::Unavailable);
            }
        }
        return;
    };
    let Some(display) = pick_display(&content, "") else { return };
    let running: Vec<(String, Retained<SCRunningApplication>)> = unsafe { content.applications() }
        .iter()
        .map(|app| (unsafe { app.bundleIdentifier() }.to_string(), app))
        .collect();
    let app_sources: Vec<String> = taps
        .iter()
        .filter_map(|t| match &t.source.kind {
            SourceKind::App { bundle_id } => Some(bundle_id.clone()),
            _ => None,
        })
        .collect();

    for tap in taps.iter_mut() {
        let (include, wanted): (bool, Vec<&str>) = match &tap.source.kind {
            SourceKind::App { bundle_id } => (true, vec![bundle_id.as_str()]),
            SourceKind::Desktop { exclude_app_sources: true } => (false, app_sources.iter().map(String::as_str).collect()),
            _ => (false, Vec::new()),
        };
        let apps: Vec<Retained<SCRunningApplication>> = running
            .iter()
            .filter(|(bundle, _)| wanted.iter().any(|w| belongs_to(bundle, w)))
            .map(|(_, app)| app.clone())
            .collect();
        let mut pids: Vec<i32> = apps.iter().map(|a| unsafe { a.processID() }).collect();
        pids.sort_unstable();

        if include && apps.is_empty() {
            // Nothing to listen to until the app opens. A stream left over from a
            // previous run of the app just goes quiet.
            tap.channel.set_status(SourceStatus::WaitingForApp);
            continue;
        }
        if tap.pids.as_ref() != Some(&pids) {
            match &tap.stream {
                Some((stream, _)) => {
                    let filter = audio_filter(&display, &apps, include);
                    let _ = await_completion(|h| unsafe { stream.updateContentFilter_completionHandler(&filter, Some(h)) });
                }
                None => {
                    let output = StreamOutput::new(Arc::new(Shared { latest: Mutex::new(None), audio: Some(tap.feed.clone()) }));
                    match make_audio_stream(&display, &apps, include, &output).and_then(|st| start_capture(&st).map(|_| st)) {
                        Ok(stream) => tap.stream = Some((stream, output)),
                        Err(e) => {
                            eprintln!("audio source \"{}\": {e}", tap.source.name);
                            tap.channel.set_status(SourceStatus::Unavailable);
                            continue;
                        }
                    }
                }
            }
            tap.pids = Some(pids);
        }
        tap.channel.set_status(SourceStatus::Live);
    }
}

// ---------------------------------------------------------------------------
// Pacer: constant frame rate
// ---------------------------------------------------------------------------

/// Encode the newest screen frame every 1/fps on the host clock. SCK only sends
/// frames when the screen changes; repeating the last one keeps the output CFR.
/// Frame N is stamped N/fps seconds after t0, the same clock the audio uses.
fn spawn_pacer(fps: u32, shared: Arc<Shared>, encoder: Encoder, clock: Arc<Clock>) -> (Arc<AtomicBool>, JoinHandle<()>) {
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let encoder = SendBox(encoder);
    let handle = thread::spawn(move || {
        let encoder = encoder;
        let fps_f = fps as f64;
        let t0 = host_now();
        clock.set(t0);
        let mut next: i64 = 0;
        while !stop2.load(Ordering::Relaxed) {
            let now = host_now();
            let due = ((now - t0) * fps_f) as i64;
            // After a stall, catch up gradually rather than in one huge burst.
            let mut burst = 0;
            while next <= due && burst < fps * 2 {
                let frame = shared.latest.lock().unwrap().as_ref().map(|f| f.0.clone());
                if let Some(frame) = frame {
                    encoder.0.encode(&frame, next, fps);
                }
                next += 1;
                burst += 1;
            }
            let wake = t0 + next as f64 / fps_f;
            let wait = wake - host_now();
            if wait > 0.0 {
                thread::sleep(Duration::from_secs_f64(wait));
            }
        }
        encoder.0.finish();
    });
    (stop, handle)
}

// ---------------------------------------------------------------------------
// VideoToolbox H.264 encoder
// ---------------------------------------------------------------------------

struct Encoder {
    session: CFRetained<VTCompressionSession>,
    /// Owned by the encoder callback; freed in `finish` after the last callback.
    ctx: *mut EncodedSink,
}

/// Where encoded frames go.
struct EncodedSink {
    tx: Option<Sender<writer::Command>>,
    /// Catches the format description of the first frame (`probe_video_format`).
    probe: Option<Sender<CFRetained<objc2_core_media::CMFormatDescription>>>,
}

impl Encoder {
    fn new(width: usize, height: usize, s: &EncodeSettings, tx: Sender<writer::Command>) -> Result<Self> {
        Self::with_sink(width, height, s, EncodedSink { tx: Some(tx), probe: None })
    }

    fn with_sink(width: usize, height: usize, s: &EncodeSettings, sink: EncodedSink) -> Result<Self> {
        unsafe {
            let spec_key = if s.use_hardware {
                kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder
            } else {
                kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder
            };
            let spec = CFDictionary::<CFString, CFType>::from_slices(&[spec_key], &[CFBoolean::new(s.use_hardware)]);
            let ctx = Box::into_raw(Box::new(sink));
            let mut out: *mut VTCompressionSession = ptr::null_mut();
            let status = VTCompressionSession::create(
                None,
                width as i32,
                height as i32,
                kCMVideoCodecType_H264,
                Some(spec.as_opaque()),
                None,
                None,
                Some(on_encoded),
                ctx.cast(),
                NonNull::from(&mut out),
            );
            let Some(session) = NonNull::new(out).filter(|_| status == 0) else {
                drop(Box::from_raw(ctx));
                bail!("couldn't create the H.264 encoder (VideoToolbox error {status})");
            };
            let session = CFRetained::from_raw(session);

            let kf = s.keyframe_interval_secs.max(1);
            let set = |key: &CFString, value: &CFType| VTSessionSetProperty(&session, key, Some(value));
            set(kVTCompressionPropertyKey_RealTime, CFBoolean::new(true));
            set(kVTCompressionPropertyKey_ProfileLevel, kVTProfileLevel_H264_High_AutoLevel);
            // No B-frames: frames come out in order, so decode time = presentation time.
            set(kVTCompressionPropertyKey_AllowFrameReordering, CFBoolean::new(false));
            set(kVTCompressionPropertyKey_AverageBitRate, &CFNumber::new_i64(s.video_bitrate_kbps as i64 * 1000));
            set(kVTCompressionPropertyKey_ExpectedFrameRate, &CFNumber::new_i32(s.fps as i32));
            set(kVTCompressionPropertyKey_MaxKeyFrameInterval, &CFNumber::new_i32((s.fps * kf) as i32));
            set(kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration, &CFNumber::new_f64(kf as f64));
            set(kVTCompressionPropertyKey_ColorPrimaries, kCVImageBufferColorPrimaries_ITU_R_709_2);
            set(kVTCompressionPropertyKey_TransferFunction, kCVImageBufferTransferFunction_ITU_R_709_2);
            set(kVTCompressionPropertyKey_YCbCrMatrix, kCVImageBufferYCbCrMatrix_ITU_R_709_2);
            Ok(Self { session, ctx })
        }
    }

    /// Encode `frame` as frame number `n` (presented at n/fps).
    fn encode(&self, frame: &CVPixelBuffer, n: i64, fps: u32) {
        unsafe {
            let status = self.session.encode_frame(
                frame,
                CMTime::new(n, fps as i32),
                CMTime::new(1, fps as i32),
                None,
                ptr::null_mut(),
                ptr::null_mut(),
            );
            if status != 0 {
                eprintln!("VideoToolbox encode error {status}");
            }
        }
    }

    /// Flush pending frames, then release the sink.
    fn finish(self) {
        unsafe {
            self.session.complete_frames(kCMTimeInvalid);
            self.session.invalidate();
            drop(Box::from_raw(self.ctx));
        }
    }
}

/// VideoToolbox output callback: hand the encoded frame (AVCC, as AVAssetWriter
/// wants it) to the writer, timestamped in seconds since t0.
unsafe extern "C-unwind" fn on_encoded(
    ctx: *mut c_void,
    _frame_ctx: *mut c_void,
    status: i32,
    _flags: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    let (Some(sink), Some(sample)) = (unsafe { (ctx as *const EncodedSink).as_ref() }, unsafe { sample.as_ref() }) else {
        return;
    };
    if status != 0 {
        return;
    }
    if let Some(probe) = &sink.probe {
        if let Some(fmt) = unsafe { sample.format_description() } {
            let _ = probe.send(fmt);
        }
        return;
    }
    let pts = unsafe { sample.presentation_time_stamp().seconds() };
    let key = is_keyframe(sample);
    let sample = unsafe { CFRetained::retain(NonNull::from(sample)) };
    if let Some(tx) = &sink.tx {
        let _ = tx.send(writer::Command::Media(Media::Video { frame: SendSample(sample), pts, key }));
    }
}

/// The H.264 format description our encoder settings produce, from encoding one
/// blank frame — the writer must know each track's format before the first frame.
fn probe_video_format(
    width: usize,
    height: usize,
    s: &EncodeSettings,
) -> Result<CFRetained<objc2_core_media::CMFormatDescription>> {
    let (tx, rx) = mpsc::channel();
    let enc = Encoder::with_sink(width, height, s, EncodedSink { tx: None, probe: Some(tx) })?;
    let mut pb: *mut CVPixelBuffer = ptr::null_mut();
    let status = unsafe {
        objc2_core_video::CVPixelBufferCreate(
            None,
            width,
            height,
            kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
            None,
            NonNull::from(&mut pb),
        )
    };
    let pb = NonNull::new(pb).filter(|_| status == 0).context("couldn't create a probe frame")?;
    let pb = unsafe { CFRetained::from_raw(pb) };
    enc.encode(&pb, 0, s.fps);
    enc.finish();
    rx.recv_timeout(Duration::from_secs(5)).context("the H.264 encoder produced nothing")
}

fn is_keyframe(sample: &CMSampleBuffer) -> bool {
    unsafe {
        let Some(attachments) = sample.sample_attachments_array(false) else { return true };
        if attachments.count() == 0 {
            return true;
        }
        let dict = attachments.value_at_index(0) as *const CFDictionary;
        let not_sync = kCMSampleAttachmentKey_NotSync as *const CFString as *const c_void;
        dict.as_ref().is_none_or(|d| !d.contains_ptr_key(not_sync))
    }
}
