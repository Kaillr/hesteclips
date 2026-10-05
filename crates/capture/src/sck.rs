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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
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
    SCContentFilter, SCDisplay, SCRunningApplication, SCStream,
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
use crate::mac::video::{self, Frames, Picture, pick_display, shareable_content};
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
        crate::output::finish_rename(&partial, &file).context("couldn't finish the recording")?;
        Ok(Some(file))
    }

    fn is_running(&self) -> bool {
        self.session.is_some()
    }

    fn update_video(&mut self, video: &crate::VideoSource) -> bool {
        self.session.as_ref().and_then(|s| s.video.as_ref()).is_some_and(|p| p.update(video))
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
    /// The screen or app capture (`mac::video`).
    video: Option<Picture>,
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
        let (content, display, (width, height)) = video::plan(&s.video, s.target_height)?;
        // The away screen, converted once.
        let away = match &s.away_screen {
            Some(img) if matches!(s.video, crate::VideoSource::Apps { .. }) => Some(Arc::new(video::still_buffer(Some(img), width, height)?)),
            _ => None,
        };
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
            let picture = Picture::start(&s.video, &content, &display, (width, height), s.fps, away)?;
            // A webcam that can't open doesn't stop the recording: its status
            // says why, and the picture goes on without it.
            let frames = Frames::new(picture.latest.clone(), s.webcam.as_ref(), width, height, crate::preview::new_producer())?;
            session.video = Some(picture);
            session.pacer = Some(spawn_pacer(s.fps, frames, encoder, clock));
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
        // The pacer flushes the video encoder on its way out.
        if let Some((stop, handle)) = self.pacer.take() {
            stop.store(true, Ordering::Relaxed);
            let _ = handle.join();
        }
        if let Some(picture) = self.video.take() {
            picture.stop();
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

/// What an audio stream's callbacks feed: a source's audio.
struct Shared {
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
            if kind == SCStreamOutputType::Audio {
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
                    let output = StreamOutput::new(Arc::new(Shared { audio: Some(tap.feed.clone()) }));
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
///
/// When the encoder can't keep up (a big frame at a high rate is more than
/// the hardware does: ~60 fps at 3600×2338 on an M4 Pro), slots are skipped
/// rather than queued: the pacer always encodes the slot that's due now, and
/// a skipped slot shows the previous picture a little longer. Video never
/// falls behind real time, so it stays in sync and nothing piles up.
fn spawn_pacer(fps: u32, mut frames: Frames, encoder: Encoder, clock: Arc<Clock>) -> (Arc<AtomicBool>, JoinHandle<()>) {
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let encoder = SendBox(encoder);
    let handle = thread::spawn(move || {
        let encoder = encoder;
        let fps_f = fps as f64;
        let t0 = host_now();
        clock.set(t0);
        let mut next: i64 = 0;
        let mut skipped = 0u64;
        let mut note = |n: i64| {
            let before = skipped;
            skipped += n as u64;
            // Once per doubling, so a long session doesn't flood the log.
            if before.checked_ilog2() != skipped.checked_ilog2() {
                eprintln!("video encoder is behind: {skipped} frames skipped so far");
            }
        };
        while !stop2.load(Ordering::Relaxed) {
            let due = ((host_now() - t0) * fps_f) as i64;
            if next <= due {
                // Missed slots (the encoder or the system stalled) are skipped.
                if due > next {
                    note(due - next);
                }
                if let Some(frame) = frames.next() {
                    if !encoder.0.encode(&frame.0, due, fps) {
                        note(1);
                    }
                }
                next = due + 1;
            }
            let wait = t0 + next as f64 / fps_f - host_now();
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

/// Frames in the encoder before the pacer skips one instead of waiting.
/// VideoToolbox's own limit is 8: past it `encode_frame` blocks until a
/// frame is done, which would hold the pacer up.
const QUEUE: usize = 6;

pub(crate) struct Encoder {
    pub(crate) session: CFRetained<VTCompressionSession>,
    /// Owned by the encoder callback; freed in `finish` after the last callback.
    ctx: *mut EncodedSink,
}

/// Where encoded frames go.
pub(crate) struct EncodedSink {
    pub(crate) tx: Option<Sender<writer::Command>>,
    /// Catches the format description of the first frame (`probe_video_format`).
    pub(crate) probe: Option<Sender<CFRetained<objc2_core_media::CMFormatDescription>>>,
    /// Frames handed to the encoder and not called back yet.
    in_flight: AtomicUsize,
}

impl Encoder {
    pub(crate) fn new(width: usize, height: usize, s: &EncodeSettings, tx: Sender<writer::Command>) -> Result<Self> {
        Self::with_sink(width, height, s, EncodedSink { tx: Some(tx), probe: None, in_flight: AtomicUsize::new(0) })
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

    /// Encode `frame` as frame number `n` (presented at n/fps). False if the
    /// encoder is too far behind to take it.
    pub(crate) fn encode(&self, frame: &CVPixelBuffer, n: i64, fps: u32) -> bool {
        let sink = unsafe { &*self.ctx };
        if sink.in_flight.load(Ordering::Relaxed) >= QUEUE {
            return false;
        }
        sink.in_flight.fetch_add(1, Ordering::Relaxed);
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
                // No callback comes for a frame refused outright.
                sink.in_flight.fetch_sub(1, Ordering::Relaxed);
                eprintln!("VideoToolbox encode error {status}");
            }
        }
        true
    }

    /// Frames handed in and not called back yet.
    #[allow(dead_code)]
    pub(crate) fn in_flight(&self) -> usize {
        unsafe { &*self.ctx }.in_flight.load(Ordering::Relaxed)
    }

    /// Flush pending frames, then release the sink.
    pub(crate) fn finish(self) {
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
    let Some(sink) = (unsafe { (ctx as *const EncodedSink).as_ref() }) else { return };
    // Called once per frame handed in, encoded or dropped.
    sink.in_flight.fetch_sub(1, Ordering::Relaxed);
    let Some(sample) = (unsafe { sample.as_ref() }).filter(|_| status == 0) else { return };
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
pub(crate) fn probe_video_format(
    width: usize,
    height: usize,
    s: &EncodeSettings,
) -> Result<CFRetained<objc2_core_media::CMFormatDescription>> {
    let (tx, rx) = mpsc::channel();
    let enc = Encoder::with_sink(width, height, s, EncodedSink { tx: None, probe: Some(tx), in_flight: AtomicUsize::new(0) })?;
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

/// For `examples/vt_limits`: can the hardware encoder open at this size?
pub(crate) fn probe_encoder(width: usize, height: usize) -> String {
    let s = EncodeSettings {
        output_dir: std::env::temp_dir(), container_ext: "mp4".into(), fps: 60, video_bitrate_kbps: 20000, target_height: None,
        keyframe_interval_secs: 2, use_hardware: true, replay_seconds: 5, video: crate::VideoSource::Screen { id: String::new() },
        away_screen: None, webcam: None, sources: Vec::new(),
    };
    match probe_video_format(width, height, &s) {
        Ok(_) => "ok".into(),
        Err(e) => format!("{e:#}"),
    }
}
