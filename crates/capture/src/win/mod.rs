//! Native Windows capture backend: Windows Graphics Capture + Media Foundation +
//! WASAPI, all in-process — no external tools.
//!
//! - **Video**: WGC delivers the monitor, or one app's window, as BGRA textures
//!   on the GPU (`d3d`); a monitor can be captured with Desktop Duplication
//!   instead (`dupl`), which draws no yellow border on Windows 10. A pacer thread converts the newest one to NV12 at the
//!   output size with the GPU's video processor and hands it to a Media
//!   Foundation H.264 encoder (`h264`) at exactly `fps`, so the output is
//!   constant frame rate even when the screen is static and WGC sends nothing.
//!   Frames never leave the GPU unless the software encoder is used.
//! - **Desktop and app audio**: WASAPI process loopback (`loopback`). **Mics**
//!   use cpal.
//! - **Mixing** happens in Rust (`crate::mixer`), which also drives the live
//!   meters; each track is AAC-encoded with Media Foundation (`aac`).
//! - **Writing** (`crate::writer`): our own MP4 muxer (`file`), either straight
//!   to a fragmented MP4 (record) or from an in-memory replay ring when a clip
//!   is saved.
//!
//! Everything is timestamped on QueryPerformanceCounter, the clock WGC and
//! WASAPI use: the first video frame is t=0.

mod aac;
pub mod anticheat;
mod camera;
mod changes;
mod cursor;
mod d3d;
mod dupl;
pub(crate) mod export;
pub mod decode;
pub(crate) mod direct;
pub mod gpu;
mod h264;
pub mod hook;
mod loopback;
mod quad;
pub(crate) mod system;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

pub(crate) use loopback::SystemAudio;
pub(crate) use system::host_now;
pub use camera::{keep_open as keep_camera_open, list_cameras, open_camera_settings};
pub(crate) use camera::picture as camera_picture;
pub use system::{cursor_state, foreground_app_path, foreground_exe, list_apps, list_screens, list_windowed_apps};
pub(crate) use system::display_pixels;

use crate::mixer::{self, Clock, LiveAudio};
use crate::mp4mux::Spec;
use crate::output::{in_progress, timestamp};
use crate::sources::{AudioCapture, mix_inputs, track_layout};
use crate::writer::{self, Media, Writer};
use crate::{EncodeSettings, Mode, Recorder, VideoSource};
use aac::AacEncoder;
use crate::mp4file::Layout;

/// AAC bitrate per audio track.
const AUDIO_BITRATE: u32 = 192_000;

pub struct WinRecorder {
    session: Option<Session>,
    mode: Option<Mode>,
    current_file: Option<PathBuf>,
    replay_seconds: u32,
    output_dir: PathBuf,
    container_ext: String,
    live: Arc<LiveAudio>,
}

impl WinRecorder {
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

impl Recorder for WinRecorder {
    fn start(&mut self, mode: Mode, settings: &EncodeSettings) -> Result<()> {
        if self.session.is_some() {
            let _ = self.stop(None);
        }
        self.replay_seconds = settings.replay_seconds;
        self.output_dir = settings.output_dir.clone();
        // Our muxer writes MP4 and MOV; MKV isn't one of them.
        self.container_ext = if settings.container_ext == "mov" { "mov".into() } else { "mp4".into() };
        std::fs::create_dir_all(&settings.output_dir)?;

        let s = settings.clone();
        let target = match mode {
            Mode::Record => {
                let file = self.output_dir.join(format!("recording_{}.{}", timestamp(), self.container_ext));
                self.current_file = Some(file.clone());
                Target::Record(in_progress(&file))
            }
            Mode::ReplayBuffer => {
                self.current_file = None;
                Target::Replay(settings.replay_seconds as f64)
            }
        };
        self.session = Some(Session::start(&s, target, &self.live, self.container_ext == "mov")?);
        self.mode = Some(mode);
        Ok(())
    }

    fn save_clip(&mut self, dir: &Path) -> Result<crate::PendingClip> {
        let session = self.session.as_ref().filter(|_| self.mode == Some(Mode::ReplayBuffer));
        let session = session.context("replay buffer is not running")?;
        std::fs::create_dir_all(dir)?;
        writer::request_clip(&session.writer_tx, dir, &self.container_ext, self.replay_seconds as f64)
    }

    fn stop(&mut self, dir: Option<&Path>) -> Result<Option<PathBuf>> {
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
        let file = crate::output::destination(&file, dir);
        crate::output::finish_rename(&partial, &file).context("couldn't finish the recording")?;
        Ok(Some(file))
    }

    fn is_running(&self) -> bool {
        self.session.is_some()
    }

    fn set_replay_seconds(&mut self, seconds: u32) {
        self.replay_seconds = seconds;
        if let Some(session) = self.session.as_ref().filter(|_| self.mode == Some(Mode::ReplayBuffer)) {
            let _ = session.writer_tx.send(writer::Command::SetWindow(seconds as f64));
        }
    }

    fn update_video(&mut self, video: &VideoSource) -> bool {
        self.session.as_mut().is_some_and(|s| s.update_video(video))
    }

    fn set_mic(&mut self, id: &str, device: &str) -> bool {
        self.session.as_mut().and_then(|s| s.audio.as_mut()).is_some_and(|a| a.set_mic(id, device))
    }

    fn screenshot(&mut self, video: &VideoSource) -> Result<crate::PendingShot> {
        if let Some(sw) = self.session.as_ref().and_then(|s| s.switch.as_ref()) {
            return shot_of(&sw.latest);
        }
        // Nothing running: a capture of its own, for one picture.
        system::com_init();
        let gpu = d3d::Gpu::new()?;
        let (source, (w, h)) = PictureSource::plan(video)?;
        let picture = source.open(&gpu, w, h, 60, None)?;
        // A screen has its picture by now; games and apps take a moment to
        // find the one that's showing.
        let deadline = Instant::now() + Duration::from_secs(2);
        while (!picture.latest.has_frame() || picture.latest.waiting()) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let shot = shot_of(&picture.latest);
        picture.video.stop();
        shot
    }
}

/// A screenshot of what's up in `latest`, unless that's the away screen or
/// black: none of the games and apps recorded is showing.
fn shot_of(latest: &d3d::Latest) -> Result<crate::PendingShot> {
    if !latest.has_frame() {
        bail!("nothing has been captured yet. Try again in a moment");
    }
    if latest.waiting() {
        bail!("none of the games and apps you record is showing");
    }
    latest.snapshot()
}

impl Drop for WinRecorder {
    /// Dropping (app quit) must still finalise the file.
    fn drop(&mut self) {
        if self.session.is_some() {
            let _ = self.stop(None);
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
/// (WASAPI delivers in ~10 ms packets, sometimes late). Timestamps are exact, so
/// this only delays writing — it never shifts audio against video.
const MIX_LATENCY: f64 = 0.3;

/// Everything a running capture owns. Torn down in `finish` in an order that
/// lets every encoder flush into the writer before it closes the file.
struct Session {
    video: Option<Video>,
    /// The games and apps being followed, when recording apps.
    apps: Option<AppList>,
    /// What starting another kind of picture takes (whole screen, or games
    /// and apps), to switch while it runs.
    switch: Option<Switch>,
    pacer: Option<(Arc<AtomicBool>, JoinHandle<()>)>,
    audio: Option<AudioCapture>,
    mixer: Option<(Arc<AtomicBool>, JoinHandle<()>)>,
    /// AAC encoder threads, one per track; they end when the mixer drops its senders.
    encoders: Vec<JoinHandle<()>>,
    writer_tx: Sender<writer::Command>,
    writer: Option<Writer>,
}

impl Session {
    fn start(s: &EncodeSettings, target: Target, live: &Arc<LiveAudio>, mov: bool) -> Result<Self> {
        crate::capture_started();
        system::com_init();
        system::mf_startup()?;
        let gpu = d3d::Gpu::new()?;
        let (picture_source, (canvas_w, canvas_h)) = PictureSource::plan(&s.video)?;
        let (width, height) = s.size.of(canvas_w, canvas_h);
        let clock = Arc::new(Clock::default());

        let (titles, comment, has_rest) = track_layout(&s.sources);
        let aac: Vec<AacEncoder> = titles.iter().map(|_| AacEncoder::new(AUDIO_BITRATE)).collect::<Result<_>>()?;
        let layout = Layout {
            spec: Spec { width, height, fps: s.fps, audio_titles: titles, audio_bitrate: AUDIO_BITRATE, comment, mov, reorder: 0 },
        };
        let writer = match target {
            Target::Record(file) => Writer::record(file, layout, s.fps),
            Target::Replay(seconds) => Writer::replay(seconds, layout, s.fps),
        };
        let writer_tx = writer.tx.clone();

        let mut session =
            Self { video: None, apps: None, switch: None, pacer: None, audio: None, mixer: None, encoders: Vec::new(), writer_tx, writer: Some(writer) };
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

            let encoder = h264::Encoder::start(&gpu, width, height, s, session.writer_tx.clone())?;
            session.apps = picture_source.apps();
            // The screen is captured fast enough for every look to find a new
            // picture: the capture's rate limits are set from the looks per
            // second, not the frame rate (at 60 fps they'd hold it to 240).
            let looks_per_second = s.fps * s.blend_samples.max(1);
            let picture = picture_source.open(&gpu, canvas_w, canvas_h, looks_per_second, s.away_screen.clone())?;
            let latest = picture.latest.clone();
            session.video = Some(picture.video);
            session.switch = Some(Switch { gpu: gpu.clone(), latest: latest.clone(), fps: looks_per_second, away: s.away_screen.clone(), source: s.video.clone() });
            // A webcam that can't open doesn't stop the recording: its status
            // says why, and the picture goes on without it.
            let camera = s.webcam.as_ref().and_then(|w| match camera::CameraLayer::new(&gpu, w) {
                Ok(c) => Some(c),
                Err(e) => {
                    eprintln!("webcam: {e:#}");
                    None
                }
            });
            let overlay = camera.as_ref().zip(s.webcam.as_ref()).map(|(c, w)| (&c.latest, w.placement.clone()));
            // Each frame comes from the pictures shown during it, timed by
            // when each came up (see `FrameBlend`): with blending their
            // average, without it the one up at the frame's time. Taking the
            // newest picture on the clock instead repeated and skipped
            // pictures of a game at the recording's rate (a 300 fps game
            // recorded at 300: up to 22% of frames), as the clock and the
            // game's pictures drifted past each other.
            let looks = s.blend_samples.max(1);
            let shutter = crate::shutter::Shutter::new(looks, s.blend_shape);
            let blend = FrameBlend::new(&gpu, &latest, looks, shutter.frames_open())?;
            latest.set_wants_queue(true);
            let converter = d3d::Converter::new(&gpu, &blend.average, overlay.clone(), &encoder.pool.textures, true, width, height, s.fps, s.screen.clone())?;
            let preview = d3d::Previewer::new(&gpu, &latest, overlay, width, height, s.fps, s.screen.clone(), crate::preview::new_producer())?;
            session.pacer = Some(spawn_blend_pacer(s.fps, looks, shutter, latest, blend, converter, encoder, preview, camera, clock));
            Ok(())
        })();
        if let Err(e) = started {
            let _ = session.finish();
            return Err(e);
        }
        Ok(session)
    }

    /// Record something else without restarting: another list of games and
    /// apps, or the whole screen instead of them (or the other way round, or
    /// another display), into the same picture. False if it couldn't.
    fn update_video(&mut self, video: &VideoSource) -> bool {
        if let (Some(config), Some(list)) = (AppsConfig::of(video), self.apps.as_ref()) {
            *list.lock().unwrap() = config;
            if let Some(sw) = &mut self.switch {
                sw.source = video.clone();
            }
            return true;
        }
        let Some(sw) = &mut self.switch else { return false };
        if sw.source == *video {
            return true;
        }
        let source = match PictureSource::plan(video) {
            Ok((source, _)) => source,
            Err(e) => {
                eprintln!("can't switch what's recorded: {e:#}");
                return false;
            }
        };
        // The old one stops first: they'd both fill the picture.
        if let Some(old) = self.video.take() {
            old.stop();
        }
        let apps = source.apps();
        match source.start(&sw.gpu, &sw.latest, sw.fps, sw.away.clone()) {
            Ok(v) => {
                self.video = Some(v);
                self.apps = apps;
                sw.source = video.clone();
                true
            }
            Err(e) => {
                eprintln!("can't switch what's recorded: {e:#}");
                // Back to what it was recording.
                if let Ok((old, _)) = PictureSource::plan(&sw.source) {
                    self.apps = old.apps();
                    self.video = old.start(&sw.gpu, &sw.latest, sw.fps, sw.away.clone()).ok();
                }
                false
            }
        }
    }

    /// Stop capturing and finish the file (record) or drop the ring (replay).
    fn finish(mut self) -> Result<()> {
        // Each step logged with its time: a step that never ends is a frozen
        // quit, and the log then says which.
        let t = Instant::now();
        let step = |what: &str| eprintln!("stopping capture: {what} ({} ms)", t.elapsed().as_millis());
        // The pacer flushes the video encoder on its way out.
        if let Some((stop, handle)) = self.pacer.take() {
            stop.store(true, Ordering::Relaxed);
            let _ = handle.join();
        }
        step("frame pacer stopped");
        if let Some(video) = self.video.take() {
            video.stop();
        }
        step("video capture stopped");
        if let Some(audio) = self.audio.take() {
            audio.stop();
        }
        step("audio capture stopped");
        // The mixer writes what's left and drops its senders; the AAC threads then
        // flush and exit.
        if let Some((stop, handle)) = self.mixer.take() {
            stop.store(true, Ordering::Relaxed);
            let _ = handle.join();
        }
        step("mixer stopped");
        for h in self.encoders.drain(..) {
            let _ = h.join();
        }
        step("audio encoders stopped");
        drop(self.writer_tx);
        let r = self.writer.take().map_or(Ok(()), Writer::join);
        step("file writer done");
        r
    }
}

/// Where a session's picture comes from: a display, or an app's window.
enum PictureSource {
    /// A display (and which), when to switch to the game capture hook, and
    /// how to capture it.
    Screen(windows::Graphics::Capture::GraphicsCaptureItem, windows::Win32::Graphics::Gdi::HMONITOR, crate::GameHook, crate::ScreenCapture),
    /// The apps to follow, changeable while capturing.
    Apps(AppList),
}

/// The games and apps a capture follows, and what to show while you're in
/// something else.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct AppsConfig {
    ids: Vec<String>,
    away_when_unfocused: bool,
    hook: crate::GameHook,
}

impl AppsConfig {
    fn of(source: &VideoSource) -> Option<Self> {
        match source {
            VideoSource::Apps { ids, away_when_unfocused, hook } => {
                Some(Self { ids: ids.clone(), away_when_unfocused: *away_when_unfocused, hook: hook.clone() })
            }
            VideoSource::Screen { .. } => None,
        }
    }
}

/// Shared with the thread following the apps, so it can change while it runs.
type AppList = Arc<std::sync::Mutex<AppsConfig>>;

/// What starting another kind of picture into a running capture takes.
struct Switch {
    gpu: d3d::Gpu,
    latest: Arc<d3d::Latest>,
    fps: u32,
    away: Option<Arc<crate::StillImage>>,
    /// What it's recording now.
    source: VideoSource,
}

/// A running capture and the texture it fills.
struct Picture {
    video: Video,
    latest: Arc<d3d::Latest>,
}

impl PictureSource {
    /// The source, and the frame size: the display's — for apps, the main
    /// display's — so it can't change mid-file, whatever the windows do.
    fn plan(source: &VideoSource) -> Result<(Self, (u32, u32))> {
        Ok(match source {
            VideoSource::Screen { id, hook, method, .. } => {
                let monitor = system::find_monitor(id);
                let item = d3d::monitor_item(monitor)?;
                let size = item.Size()?;
                (PictureSource::Screen(item, monitor, hook.clone(), *method), (size.Width.max(2) as u32, size.Height.max(2) as u32))
            }
            VideoSource::Apps { .. } => {
                let (_, w, h) = system::primary_monitor();
                let config = AppsConfig::of(source).unwrap_or_default();
                (PictureSource::Apps(Arc::new(std::sync::Mutex::new(config))), (w, h))
            }
        })
    }

    fn open(self, gpu: &d3d::Gpu, width: u32, height: u32, fps: u32, away: Option<Arc<crate::StillImage>>) -> Result<Picture> {
        // What's captured is copied 1:1 before it's scaled into the frame, so the
        // copy must hold all of it. A display is its own size; an app's window
        // can be on any display — on a portrait one, taller than the frame — so
        // it gets room for a window on the biggest of them. A picture bigger
        // still (a display whose mode grew since) is shrunk to fit it.
        // Room for a window whatever it records now: it can switch to games
        // and apps while it runs.
        let (copy_w, copy_h) = {
            let (w, h) = system::largest_display_box();
            (w.max(width), h.max(height))
        };
        let latest = d3d::Latest::new(gpu, copy_w, copy_h)?;
        let video = self.start(gpu, &latest, fps, away)?;
        Ok(Picture { video, latest })
    }

    /// Start recording into `latest`.
    fn start(self, gpu: &d3d::Gpu, latest: &Arc<d3d::Latest>, fps: u32, away: Option<Arc<crate::StillImage>>) -> Result<Video> {
        Ok(match self {
            PictureSource::Apps(list) => {
                // The away screen (black with it off) until one of the
                // apps' windows shows up.
                let on = list.lock().unwrap().away_when_unfocused;
                match away.as_ref().filter(|_| on) {
                    Some(image) => latest.show_still(image)?,
                    None => latest.clear()?,
                }
                Video::follow_apps(gpu.clone(), latest.clone(), list, fps, away)
            }
            PictureSource::Screen(item, monitor, hook, method) => {
                let by = NEXT_FOLLOWER.fetch_add(1, Ordering::Relaxed);
                let capture = DisplayCapture::start(gpu, &item, monitor, latest, fps, method, by)?;
                // Wait for the first frame so a capture that can't see the screen
                // fails here, visibly, instead of producing an empty file.
                let deadline = Instant::now() + Duration::from_secs(3);
                while !latest.has_frame() {
                    if Instant::now() >= deadline {
                        bail!("the screen isn't delivering frames");
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Video::follow_screen(gpu.clone(), latest.clone(), item, monitor, capture, fps, hook, method, by)
            }
        })
    }

    /// The app list, when following apps.
    fn apps(&self) -> Option<AppList> {
        match self {
            PictureSource::Apps(list) => Some(list.clone()),
            PictureSource::Screen(..) => None,
        }
    }
}

/// A capture made only for the preview (`crate::preview::VideoPreview`). Runs
/// entirely on its own thread: starting doesn't wait for the capture to open,
/// and dropping doesn't wait for it to close.
pub(crate) struct PreviewCapture {
    stop: Arc<AtomicBool>,
    error: Arc<std::sync::Mutex<Option<String>>>,
    /// The app list being followed, when previewing apps.
    apps: Option<AppList>,
}

impl PreviewCapture {
    pub(crate) fn start(
        source: VideoSource,
        size: crate::VideoSize,
        fps: u32,
        away: Option<Arc<crate::StillImage>>,
        webcam: Option<crate::webcam::Webcam>,
        screen: crate::SharedScreen,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let error = Arc::new(std::sync::Mutex::new(None));
        let (stop2, error2) = (stop.clone(), error.clone());
        let generation = crate::preview::new_producer();
        let apps = AppsConfig::of(&source).map(|c| Arc::new(std::sync::Mutex::new(c)));
        let apps2 = apps.clone();
        thread::spawn(move || {
            system::com_init();
            let opened = (|| -> Result<(Picture, d3d::Previewer, Option<camera::CameraLayer>)> {
                let gpu = d3d::Gpu::new()?;
                let (mut source, (w, h)) = PictureSource::plan(&source)?;
                if let (PictureSource::Apps(list), Some(shared)) = (&mut source, apps2) {
                    *list = shared; // so `update` reaches the running follower
                }
                let picture = source.open(&gpu, w, h, fps, away)?;
                let (width, height) = size.of(w, h);
                let cam = webcam.as_ref().and_then(|wc| camera::CameraLayer::new(&gpu, wc).ok());
                let overlay = cam.as_ref().zip(webcam.as_ref()).map(|(c, wc)| (&c.latest, wc.placement.clone()));
                let preview = d3d::Previewer::new(&gpu, &picture.latest, overlay, width, height, fps, screen, generation)?;
                Ok((picture, preview, cam))
            })();
            let (picture, preview, mut cam) = match opened {
                Ok(p) => p,
                Err(e) => {
                    *error2.lock().unwrap() = Some(format!("{e:#}"));
                    crate::preview::clear(generation);
                    return;
                }
            };
            // Paced like the recording would be.
            let every = 1.0 / fps.max(1) as f64;
            let t0 = host_now();
            let mut n = 0u64;
            while !stop2.load(Ordering::Relaxed) {
                if let Some(c) = cam.as_mut() {
                    c.pull();
                }
                if let Err(e) = preview.submit() {
                    eprintln!("preview: {e:#}");
                }
                n += 1;
                let wait = t0 + n as f64 * every - host_now();
                if wait > 0.0 {
                    thread::sleep(Duration::from_secs_f64(wait));
                } else if wait < -0.5 {
                    n = ((host_now() - t0) / every) as u64; // fell far behind: skip ahead
                }
            }
            drop(preview);
            drop(cam);
            picture.video.stop();
        });
        Self { stop, error, apps }
    }

    pub(crate) fn update(&self, source: &VideoSource) -> bool {
        match (AppsConfig::of(source), &self.apps) {
            (Some(config), Some(list)) => {
                *list.lock().unwrap() = config;
                true
            }
            _ => false,
        }
    }

    pub(crate) fn error(&self) -> Option<String> {
        self.error.lock().unwrap().clone()
    }
}

impl Drop for PreviewCapture {
    /// Tell the thread to stop; it closes the capture by itself.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// One app window's capture: Windows' own (also watched for freezing, when
/// the hook may take over), or the game capture hook.
enum WindowCapture {
    Wgc(d3d::Capture, bool),
    Hook(hook::HookCapture),
}

impl WindowCapture {
    fn is_closed(&self) -> bool {
        match self {
            WindowCapture::Wgc(c, _) => c.is_closed(),
            WindowCapture::Hook(h) => h.ended().is_some(),
        }
    }
}

/// Exclusive fullscreen: a fullscreen game in focus is drawing (on the
/// graphics card, `gpu`) while its pictures haven't reached the capture for
/// this long (see [`Sight`]). Windows' capture gets a frame for every frame a
/// window presents, the same picture again too (measured: a still window
/// drawn 180 times a second, 180 frames a second), and Desktop Duplication a
/// new picture, so a game on a still screen isn't mistaken for this; one that
/// stops drawing isn't busy on the graphics card.
const FROZEN: Duration = Duration::from_secs(2);

/// Percent of the graphics card's 3D engine that counts as drawing. A guess:
/// a window that does nothing but clear itself every frame measured 1%, one
/// that doesn't draw 0.
const DRAWING: f64 = 0.3;

/// How busy a fullscreen game keeps the graphics card drawing, in percent
/// (`gpu`, sampled every call while it's watched), if that can be told.
fn drawing(gpu: &mut Option<gpu::GpuUse>, window: windows::Win32::Foundation::HWND) -> Option<f64> {
    Some(gpu.as_mut()?.percent(system::window_pid(window)))
}

/// What tells how busy games keep the graphics card (see [`drawing`]); said
/// in the log (once) where Windows can't: without it, no fullscreen game is
/// switched to the game capture hook for going unseen.
fn gpu_meter() -> Option<gpu::GpuUse> {
    let found = gpu::GpuUse::new();
    if found.is_none() {
        static SAID: std::sync::Once = std::sync::Once::new();
        SAID.call_once(|| eprintln!("can't tell how busy games keep the graphics card (Windows' GPU counters): a fullscreen game Windows can't see won't switch to the game capture hook"));
    }
    found
}

/// How long after a game comes up, or its window moves, to look at it: a
/// window just gone fullscreen can still be shown by the compositor for its
/// first frames (measured: 0.2 s), and the look would end there. A guess.
const FIRST_LOOK: Duration = Duration::from_secs(1);

/// How long after a look that saw the game to look again: at first, then
/// twice as long each time, up to the second.
const LOOK_AGAIN: (Duration, Duration) = (Duration::from_secs(5), Duration::from_secs(60));

/// Whether a fullscreen game in focus that Windows' capture records reaches
/// it. Windows' capture can't tell by itself: it hands over a frame each time
/// the cursor moves too, even over a game it can't see (its last picture
/// again, the cursor moved on it), and a game being played moves one of its
/// own (Minecraft 1.12 and before an invisible one, put back in the middle
/// every frame), which brings it frames even with the cursor left out of it.
/// That kept a frozen Minecraft's recording from ever switching to the hook
/// (issue #11). Desktop Duplication tells ([`dupl::Probe`]), so one looks:
/// when the game comes up, when its window moves or changes size (going
/// exclusive fullscreen), and now and then after (see [`FIRST_LOOK`],
/// [`LOOK_AGAIN`]), till a new picture shows the game seen. Not one kept
/// open: that costs the compositor about as much as Windows' capture does
/// (measured with a game at 540 fps: 15% more of a core on top of Windows'
/// capture, 37% alone, however seldom it's asked); a look keeps one open a
/// fifth of a second, and makes a frame or two come a refresh late. Where
/// the display can't be duplicated, Windows' capture's own frames tell, as
/// they can.
#[derive(Default)]
struct Sight {
    /// The game's window and when it came up there (another, or moved, is
    /// looked at soon).
    window: Option<(WindowAt, Instant)>,
    /// The look going on.
    probe: Option<dupl::Probe>,
    /// The display can't be duplicated.
    cannot: bool,
    /// When to look again (none: at once), and how long after that.
    next: Option<Instant>,
    wait: Duration,
}

/// A window, and where it is on the desktop (see [`system::window_place`]).
type WindowAt = (usize, (i32, i32, i32, i32));

impl Sight {
    /// How long the game in `window` hasn't reached the capture, since it
    /// came up: told by looks with a Desktop Duplication of the display
    /// (`probe`: Windows' capture records it), else by how long the capture's
    /// had nothing new (`own`: Desktop Duplication's own pictures, or Windows'
    /// capture's frames where the display can't be duplicated). None: it was
    /// seen lately, and isn't looked at now.
    fn unseen(&mut self, gpu: &d3d::Gpu, window: WindowAt, probe: Option<windows::Win32::Graphics::Gdi::HMONITOR>, own: Duration) -> Option<Duration> {
        let now = Instant::now();
        if self.window.is_none_or(|(w, _)| w != window) {
            *self = Sight { window: Some((window, now)), next: Some(now + FIRST_LOOK), ..Default::default() };
        }
        let up = self.window.map_or(Duration::ZERO, |(_, at)| now - at);
        let Some(monitor) = probe.filter(|_| !self.cannot) else {
            return Some(own.min(up));
        };
        if self.probe.is_none() {
            if self.next.is_some_and(|next| now < next) {
                return None;
            }
            match dupl::Probe::start(gpu, monitor) {
                Ok(probe) => self.probe = Some(probe),
                Err(why) => {
                    // Once: it's the same each time a game comes up.
                    static SAID: std::sync::Once = std::sync::Once::new();
                    SAID.call_once(|| eprintln!("can't look at the display with Desktop Duplication ({why}): a fullscreen game Windows' capture can't see is told by its frames alone, which moving the cursor brings too"));
                    self.cannot = true;
                    return Some(own.min(up));
                }
            }
        }
        let (unseen, seen) = self.probe.as_mut()?.since_last_picture();
        if seen {
            self.look_later();
            return None;
        }
        Some(unseen)
    }

    /// The look's over (the game's seen, or doesn't draw: nothing to see):
    /// the next, later.
    fn look_later(&mut self) {
        self.probe = None;
        self.wait = (self.wait * 2).clamp(LOOK_AGAIN.0, LOOK_AGAIN.1);
        self.next = Some(Instant::now() + self.wait);
    }
}

/// A display's capture: Windows' own, or Desktop Duplication (see `dupl`).
enum DisplayCapture {
    Wgc(d3d::Capture),
    Dupl(dupl::DuplCapture),
    /// Windows' capture standing in for Desktop Duplication that couldn't
    /// start or stopped, until it can (see [`Video::follow_screen`]).
    StandIn(d3d::Capture),
}

/// How long Windows' capture stands in for Desktop Duplication before it's
/// tried again: at first, then twice as long each time it still can't, up to
/// the second.
const RETRY_DUPLICATION: (Duration, Duration) = (Duration::from_secs(5), Duration::from_secs(300));

impl DisplayCapture {
    /// Capture the display the way `choice` says (see [`dupl::choose`]).
    /// Desktop Duplication that can't start has Windows' capture stand in.
    fn start(
        gpu: &d3d::Gpu,
        item: &windows::Graphics::Capture::GraphicsCaptureItem,
        monitor: windows::Win32::Graphics::Gdi::HMONITOR,
        latest: &Arc<d3d::Latest>,
        fps: u32,
        choice: crate::ScreenCapture,
        by: u64,
    ) -> Result<Self> {
        let (method, why) = dupl::choose(choice, gpu, monitor);
        if method == dupl::Method::Wgc {
            eprintln!("recording the display with Windows' capture ({why})");
            return Self::wgc(gpu, item, latest, fps, by, false);
        }
        match Self::duplicate(gpu, monitor, latest, fps, by) {
            Ok(c) => {
                eprintln!("recording the display with Desktop Duplication ({why})");
                Ok(c)
            }
            Err(e) => {
                eprintln!("can't record the display with Desktop Duplication ({e:#}); using Windows' capture until it can");
                Self::wgc(gpu, item, latest, fps, by, true)
            }
        }
    }

    fn duplicate(gpu: &d3d::Gpu, monitor: windows::Win32::Graphics::Gdi::HMONITOR, latest: &Arc<d3d::Latest>, fps: u32, by: u64) -> Result<Self> {
        let c = dupl::DuplCapture::start(gpu, monitor, latest, fps)?;
        set_in_use(by, crate::ScreenCaptureInUse { duplication: true, fell_back: false });
        Ok(DisplayCapture::Dupl(c))
    }

    /// Windows' capture, standing in for Desktop Duplication or not.
    fn wgc(gpu: &d3d::Gpu, item: &windows::Graphics::Capture::GraphicsCaptureItem, latest: &Arc<d3d::Latest>, fps: u32, by: u64, stand_in: bool) -> Result<Self> {
        let c = d3d::Capture::start(gpu, item, latest, None, fps)?;
        set_in_use(by, crate::ScreenCaptureInUse { duplication: false, fell_back: stand_in });
        Ok(if stand_in { DisplayCapture::StandIn(c) } else { DisplayCapture::Wgc(c) })
    }
}

/// How the display is being captured (see [`crate::screen_capture_in_use`]),
/// and by which [`Video::follow_screen`] (its number): a preview's closing
/// as a recording's starts mustn't take the recording's away.
static IN_USE: std::sync::Mutex<Option<(u64, crate::ScreenCaptureInUse)>> = std::sync::Mutex::new(None);
static NEXT_FOLLOWER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn set_in_use(by: u64, now: crate::ScreenCaptureInUse) {
    *IN_USE.lock().unwrap() = Some((by, now));
}

/// `by` stopped: it no longer says how the display is captured, if it did.
fn clear_in_use(by: u64) {
    let mut in_use = IN_USE.lock().unwrap();
    if in_use.is_some_and(|(b, _)| b == by) {
        *in_use = None;
    }
}

pub(crate) fn screen_capture_in_use() -> Option<crate::ScreenCaptureInUse> {
    IN_USE.lock().unwrap().map(|(_, u)| u)
}

pub(crate) fn screen_capture_auto_duplicates() -> bool {
    dupl::border_always_shown()
}

/// Keeps the picture coming: one capture of a display, or the windows of a set
/// of apps, following focus between them and each as it closes and reopens.
enum Video {
    /// A display, or (while one's fullscreen on it and needs it) a game
    /// through the hook; or the apps followed. Both on their own thread.
    Screen { stop: Arc<AtomicBool>, thread: JoinHandle<()> },
    App { stop: Arc<AtomicBool>, thread: JoinHandle<()> },
}

impl Video {
    /// How often to look at which app is in focus.
    const RESCAN: Duration = Duration::from_millis(100);

    fn follow_apps(gpu: d3d::Gpu, latest: Arc<d3d::Latest>, list: AppList, fps: u32, away: Option<Arc<crate::StillImage>>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let thread = thread::spawn(move || {
            system::com_init();
            let mut config = AppsConfig::default();
            // The listed app last in focus: recorded until another one is.
            let mut active: Option<String> = None;
            let mut current: Option<(windows::Win32::Foundation::HWND, String, WindowCapture)> = None;
            // Windows the hook failed on (it couldn't load, or stopped): Windows'
            // capture for them instead.
            let mut hook_failed: std::collections::HashSet<usize> = Default::default();
            // Apps Windows' capture froze on: hooked from then on.
            let mut frozen: std::collections::HashSet<String> = Default::default();
            let mut sight = Sight::default();
            let mut gpu_use = gpu_meter();
            // A window that couldn't be captured, so it's reported once.
            let mut failed = None;
            // The away screen is up (so it's uploaded once, not every tick).
            let mut away_up = false;
            // Nothing listed is open and the away screen is off: the last
            // picture (or black) stays.
            let mut held = false;
            let show_away = |away_up: &mut bool| {
                if !*away_up {
                    let _ = match &away {
                        Some(image) => latest.show_still(image),
                        None => latest.clear(),
                    };
                    latest.set_app(None);
                    *away_up = true;
                }
            };
            while !stop2.load(Ordering::Relaxed) {
                // The list may have changed: forget apps that left it.
                let now = list.lock().unwrap().clone();
                if now != config {
                    config = now;
                    let still = |a: &String| config.ids.iter().any(|x| x.eq_ignore_ascii_case(a));
                    if active.as_ref().is_some_and(|a| !still(a)) {
                        active = None;
                    }
                }
                let listed = |exe: &str| config.ids.iter().find(|a| a.eq_ignore_ascii_case(exe)).cloned();
                let foreground = system::foreground_app();
                let focused = foreground.as_ref().and_then(|f| Some((f.hwnd, listed(&f.exe)?, f.main)));
                if let Some((_, app, _)) = &focused {
                    active = Some(app.clone());
                }
                // The hook stopped on a window that's still open: Windows'
                // capture for it instead.
                if let Some((h, app, WindowCapture::Hook(hooked))) = &current
                    && let Some(why) = hooked.ended()
                {
                    if system::window_alive(*h) {
                        eprintln!("{app}: the game capture hook stopped ({why}); recording it with Windows' capture");
                        hook_failed.insert(h.0 as usize);
                    }
                    current = None;
                }
                // Windows' capture of a fullscreen game in focus gets nothing
                // from it: it can't see the game (exclusive fullscreen, see
                // `Sight`). The hook can.
                let mut unseen = None;
                match &current {
                    Some((h, app, WindowCapture::Wgc(capture, true)))
                        if focused.as_ref().is_some_and(|(f, _, _)| f == h) && !system::window_hidden(*h) && system::covers_display(*h) =>
                    {
                        if let Some(percent) = drawing(&mut gpu_use, *h)
                            && sight
                                .unseen(&gpu, (h.0 as usize, system::window_place(*h)), Some(system::window_monitor(*h)), capture.since_last_frame())
                                .is_some_and(|u| u > FROZEN)
                        {
                            if percent > DRAWING {
                                unseen = Some((app.clone(), percent));
                            } else {
                                // Nothing to see: it doesn't draw.
                                sight.look_later();
                            }
                        }
                    }
                    _ => sight = Sight::default(),
                }
                if let Some((app, percent)) = unseen {
                    eprintln!("{app}: drawing ({percent:.1}% of the graphics card) but Windows' capture has had nothing from it for {FROZEN:?}: exclusive fullscreen; switching to the game capture hook");
                    frozen.insert(app.to_lowercase());
                    current = None;
                }
                // Keep the window being recorded while it's still the active
                // app's, open, and shown (or focused): switching costs a moment,
                // and flipping between two of an app's windows would flicker.
                let keep = current.as_ref().is_some_and(|(h, app, capture)| {
                    Some(app) == active.as_ref()
                        && !capture.is_closed()
                        && system::window_alive(*h)
                        && (focused.as_ref().is_none_or(|(f, _, main)| !main || f == h) && !system::window_minimized(*h)
                            || focused.as_ref().is_some_and(|(f, _, _)| f == h))
                });
                if !keep {
                    // What to record now: the focused window of a listed app,
                    // else the active app's main window, else the first listed
                    // app that's open.
                    let target = focused
                        .as_ref()
                        .filter(|(_, _, main)| *main)
                        .map(|(h, app, _)| (*h, app.clone()))
                        .or_else(|| active.as_ref().and_then(|a| Some((system::find_app_window(a)?, a.clone()))))
                        .or_else(|| config.ids.iter().find_map(|a| Some((system::find_app_window(a)?, a.clone()))));
                    let same = current.as_ref().zip(target.as_ref()).is_some_and(|((h, ..), (t, _))| h == t);
                    if !same {
                        current = None;
                        match target.filter(|(h, _)| failed != Some(*h)) {
                            // Nothing listed is open.
                            None if config.away_when_unfocused => show_away(&mut away_up),
                            None if held => {}
                            None => {
                                let _ = latest.hold();
                                latest.set_app(None);
                                away_up = false;
                                held = true;
                            }
                            Some((h, app)) => {
                                let (mut plan, mut why) = hook::plan(&app, h, &config.hook);
                                if plan == hook::Plan::Watch && frozen.contains(&app.to_lowercase()) {
                                    plan = hook::Plan::Hook;
                                }
                                if plan == hook::Plan::Hook && hook_failed.contains(&(h.0 as usize)) {
                                    plan = hook::Plan::Wgc;
                                    why = "the hook failed on this window".into();
                                }
                                eprintln!("recording {app} with {} ({why})", if plan == hook::Plan::Hook { "the game capture hook" } else { "Windows' capture" });
                                let started = match plan {
                                    hook::Plan::Hook => hook::HookCapture::start(&gpu, h, &latest, fps).map(WindowCapture::Hook),
                                    p => d3d::window_item(h)
                                        .and_then(|item| d3d::Capture::start(&gpu, &item, &latest, Some(h), fps))
                                        .map(|c| WindowCapture::Wgc(c, p == hook::Plan::Watch)),
                                };
                                match started {
                                    Ok(capture) => {
                                        latest.set_app(Some(app.clone()));
                                        active = Some(app.clone());
                                        current = Some((h, app, capture));
                                        away_up = false;
                                        held = false;
                                    }
                                    Err(e) => {
                                        eprintln!("can't record {app}'s window: {e:#}");
                                        failed = Some(h);
                                    }
                                }
                            }
                        }
                    }
                }
                // The setting changed while nothing listed is open.
                if current.is_none() && config.away_when_unfocused && held {
                    held = false;
                    show_away(&mut away_up);
                } else if current.is_none() && !config.away_when_unfocused && away_up {
                    let _ = latest.hold();
                    away_up = false;
                    held = true;
                }
                // Tabbed out: the recorded window isn't showing anymore
                // (minimized, as games are when you alt-tab, or hidden). Just
                // clicking elsewhere doesn't count: a window still on screen is
                // still recorded. With the away screen off, the clip keeps its
                // last picture.
                if let Some((h, app, _)) = &current {
                    if system::window_hidden(*h) {
                        if config.away_when_unfocused {
                            show_away(&mut away_up);
                        } else if away_up && latest.restore() {
                            // Turned off while tabbed out: its last picture again.
                            latest.set_app(Some(app.clone()));
                            away_up = false;
                        }
                    } else if away_up {
                        // Showing again: its next frame replaces the away screen.
                        latest.set_app(Some(app.clone()));
                        away_up = false;
                    }
                }
                let until = Instant::now() + Self::RESCAN;
                while Instant::now() < until && !stop2.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(20));
                }
            }
        });
        Video::App { stop, thread }
    }

    /// Record a display: Windows' capture of it (or Desktop Duplication, see
    /// [`DisplayCapture`]), except while a game that needs the game capture
    /// hook (see [`hook::plan`]) is fullscreen and in focus on it. The
    /// display's capture is stopped meanwhile: while Windows' runs, some games
    /// lose their cursor. Desktop Duplication that couldn't start or stopped
    /// is tried again now and then (see [`RETRY_DUPLICATION`]), never given
    /// up for good: what stopped it (a hitch as a game went fullscreen,
    /// another app duplicating the display) may pass.
    #[allow(clippy::too_many_arguments)]
    fn follow_screen(
        gpu: d3d::Gpu,
        latest: Arc<d3d::Latest>,
        item: windows::Graphics::Capture::GraphicsCaptureItem,
        monitor: windows::Win32::Graphics::Gdi::HMONITOR,
        first: DisplayCapture,
        fps: u32,
        settings: crate::GameHook,
        method: crate::ScreenCapture,
        by: u64,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let (item, monitor, first) = (d3d::Shared(item), d3d::Shared(monitor), d3d::Shared(first));
        let thread = thread::spawn(move || {
            // Whole wrappers (they make these Send), not their fields.
            let (item, monitor, first) = (item, monitor, first);
            let (item, monitor) = (item.0, monitor.0);
            system::com_init();
            let mut display = Some(first.0);
            // While Windows' capture stands in for Desktop Duplication: when
            // to try it again, and how long to wait after that if it can't.
            let mut retry = (Instant::now() + RETRY_DUPLICATION.0, RETRY_DUPLICATION.0);
            let mut hooked: Option<(windows::Win32::Foundation::HWND, String, hook::HookCapture)> = None;
            // How to capture each game window, worked out once.
            let mut plans: std::collections::HashMap<usize, hook::Plan> = Default::default();
            let mut hook_failed: std::collections::HashSet<usize> = Default::default();
            let mut frozen: std::collections::HashSet<String> = Default::default();
            let mut sight = Sight::default();
            let mut gpu_use = gpu_meter();
            while !stop2.load(Ordering::Relaxed) {
                // A fullscreen window in focus on this display.
                let game = system::foreground_app()
                    .filter(|f| f.main && !system::window_hidden(f.hwnd) && system::fullscreen_on(f.hwnd, monitor))
                    .map(|f| (f.hwnd, f.exe));
                let plan = game.as_ref().map(|(h, exe)| {
                    *plans.entry(h.0 as usize).or_insert_with(|| {
                        let (plan, why) = hook::plan(exe, *h, &settings);
                        eprintln!("{exe} fullscreen: {} ({why})", if plan == hook::Plan::Hook { "the game capture hook" } else { "the display's capture" });
                        plan
                    })
                });
                // The hook stopped on a game that's still open: the display
                // for it from now on.
                if let Some((h, exe, capture)) = &hooked
                    && let Some(why) = capture.ended()
                {
                    if system::window_alive(*h) {
                        eprintln!("{exe}: the game capture hook stopped ({why}); recording the display");
                        hook_failed.insert(h.0 as usize);
                    }
                    hooked = None;
                }
                let want = game.clone().filter(|(h, exe)| {
                    let hook = plan == Some(hook::Plan::Hook) || plan == Some(hook::Plan::Watch) && frozen.contains(&exe.to_lowercase());
                    hook && !hook_failed.contains(&(h.0 as usize))
                });
                match &want {
                    Some((h, exe)) if hooked.as_ref().is_none_or(|(hh, ..)| hh != h) => {
                        hooked = None;
                        display = None;
                        eprintln!("recording {exe} with the game capture hook while it's fullscreen");
                        match hook::HookCapture::start(&gpu, *h, &latest, fps) {
                            Ok(capture) => hooked = Some((*h, exe.clone(), capture)),
                            Err(e) => {
                                eprintln!("{exe}: can't start the game capture hook: {e:#}");
                                hook_failed.insert(h.0 as usize);
                            }
                        }
                    }
                    None if hooked.is_some() => hooked = None,
                    _ => {}
                }
                // Desktop Duplication stopped (it couldn't start again after
                // the display changed): Windows' capture stands in.
                if let Some(DisplayCapture::Dupl(c)) = &display
                    && let Some(why) = c.ended()
                {
                    eprintln!("desktop duplication stopped ({why}); recording the display with Windows' capture until it can start again");
                    display = DisplayCapture::wgc(&gpu, &item, &latest, fps, by, true).map_err(|e| eprintln!("can't record the display: {e:#}")).ok();
                    retry.0 = Instant::now() + retry.1;
                }
                if hooked.is_none() && display.is_none() {
                    match DisplayCapture::start(&gpu, &item, monitor, &latest, fps, method, by) {
                        Ok(c) => {
                            if matches!(c, DisplayCapture::StandIn(_)) {
                                retry.0 = Instant::now() + retry.1;
                            }
                            display = Some(c);
                        }
                        Err(e) => eprintln!("can't record the display: {e:#}"),
                    }
                }
                // Desktop Duplication tried again, Windows' capture standing in
                // until it starts (they draw the same picture meanwhile).
                if let Some(DisplayCapture::StandIn(_)) = &display
                    && Instant::now() >= retry.0
                {
                    match DisplayCapture::duplicate(&gpu, monitor, &latest, fps, by) {
                        Ok(c) => {
                            eprintln!("recording the display with Desktop Duplication again");
                            display = Some(c);
                            retry.1 = RETRY_DUPLICATION.0;
                        }
                        Err(e) => {
                            eprintln!("desktop duplication still can't start ({e:#})");
                            retry.1 = (retry.1 * 2).min(RETRY_DUPLICATION.1);
                        }
                    }
                    retry.0 = Instant::now() + retry.1;
                }
                // The display's capture gets nothing new while a fullscreen
                // game is in focus: it can't see it (exclusive fullscreen, or
                // a game that goes to the display past the compositor, issue
                // #11). Desktop Duplication tells; for Windows' capture, one
                // that looks now and then (see `Sight`).
                match (&display, &game, plan) {
                    (Some(capture), Some((h, exe)), Some(hook::Plan::Watch)) if !frozen.contains(&exe.to_lowercase()) => {
                        if let Some(percent) = drawing(&mut gpu_use, *h) {
                            let (own, probe) = match capture {
                                DisplayCapture::Dupl(c) => (c.since_last_picture(), None),
                                DisplayCapture::Wgc(c) | DisplayCapture::StandIn(c) => (c.since_last_frame(), Some(monitor)),
                            };
                            if sight.unseen(&gpu, (h.0 as usize, system::window_place(*h)), probe, own).is_some_and(|u| u > FROZEN) {
                                if percent > DRAWING {
                                    eprintln!("{exe}: drawing ({percent:.1}% of the graphics card) but the display's capture has had nothing from it for {FROZEN:?}: exclusive fullscreen; switching to the game capture hook");
                                    frozen.insert(exe.to_lowercase());
                                } else {
                                    // Nothing to see: it doesn't draw.
                                    sight.look_later();
                                }
                            }
                        }
                    }
                    _ => sight = Sight::default(),
                }
                let until = Instant::now() + Self::RESCAN;
                while Instant::now() < until && !stop2.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(20));
                }
            }
            clear_in_use(by);
        });
        Video::Screen { stop, thread }
    }

    fn stop(self) {
        let (Video::App { stop, thread } | Video::Screen { stop, thread }) = self;
        stop.store(true, Ordering::Relaxed);
        let _ = thread.join();
    }
}

/// AAC-encode one track's PCM and pass the packets to the writer.
fn spawn_aac(track: usize, mut enc: AacEncoder, rx: mpsc::Receiver<Vec<f32>>, out: Sender<writer::Command>) -> JoinHandle<()> {
    thread::spawn(move || {
        system::com_init();
        while let Ok(pcm) = rx.recv() {
            for packet in enc.push(&pcm) {
                if out.send(writer::Command::Media(Media::Audio { track, packet })).is_err() {
                    return;
                }
            }
        }
        // The mixer is done: what the encoder still holds is the last ~50 ms
        // of the recording.
        for packet in enc.finish() {
            let _ = out.send(writer::Command::Media(Media::Audio { track, packet }));
        }
    })
}

// ---------------------------------------------------------------------------
// Pacer: constant frame rate
// ---------------------------------------------------------------------------

/// Live frame blending: each frame is the average of a fixed number of
/// looks at the game, evenly spaced over the frame's time (the blend rate
/// over the frame rate: 300 fps blending at 60 fps is 5), each weighed the
/// same, so every frame shows the same number of steps of motion. Or,
/// blended softly, of the looks from the middle of the frame before to the
/// middle of the one after, the nearer the frame's middle the more (see
/// [`crate::BlendShape`]): two frames are made at once.
///
/// A look takes the picture that was up at its time. When the game runs as
/// fast as the blend rate, each look should get a different picture, but a
/// picture's time is only known to a millisecond or so (when the capture
/// noticed it): with looks right next to the game's pictures, that would decide
/// by chance which picture a look gets, one twice and the next not at all. So
/// the looks are kept halfway between the game's pictures (locked to its pace).
/// When the game runs faster, the looks pick evenly spaced ones of its
/// pictures; slower, some picture is seen twice (as it was up that long).
///
/// The pictures are averaged as light, not as pixel values (which are
/// gamma-encoded): a screen showing them in turn gives off the average of
/// their light. Averaging the values would give a bright thing moving over a
/// dark background ghosts far too faint (a fifth of its value is about 3% of
/// its light, not 20%), and a dark thing over a bright one ghosts too strong.
///
/// Looks are taken `BLEND_LATE` after their time, so that pictures noticed
/// late still count.
struct FrameBlend {
    quad: quad::Quad,
    gpu: d3d::Gpu,
    /// The picture that's up: a copy of the capture's newest (for one it puts
    /// up without queueing, a still), or one it handed over whole
    /// (`Queued`); and its valid size.
    own: Arc<d3d::Latest>,
    own_view: windows::Win32::Graphics::Direct3D11::ID3D11ShaderResourceView,
    held_view: windows::Win32::Graphics::Direct3D11::ID3D11ShaderResourceView,
    /// Keeps a handed-over picture's texture from going away.
    held_keep: Option<windows::Win32::Graphics::Direct3D11::ID3D11Texture2D>,
    held_size: (u32, u32),
    /// The averages being made, as light (16-bit float, so faint ghosts and
    /// dark colours keep their precision): one per frame open at once, by
    /// its number (even and odd, blended softly). None with one look a
    /// frame: the picture is copied as it is.
    light: Vec<(windows::Win32::Graphics::Direct3D11::ID3D11RenderTargetView, windows::Win32::Graphics::Direct3D11::ID3D11ShaderResourceView)>,
    /// The finished frame, back in sRGB: the converter reads it.
    average: Arc<d3d::Latest>,
    target: windows::Win32::Graphics::Direct3D11::ID3D11RenderTargetView,
    /// Per average: the looks' weight in it so far.
    weight: [f32; 2],
    /// Per average: the weight of looks at the picture that's up not added
    /// yet: added in one draw when another comes up or the frame is done (a
    /// game slower than the blend rate is seen several times in a row; one
    /// draw each was a full-size pass per look).
    pending: [f32; 2],
}

impl FrameBlend {
    /// `looks` a frame (one is timing alone: each frame the picture that was
    /// up at its time), `open` frames made at once.
    fn new(gpu: &d3d::Gpu, latest: &Arc<d3d::Latest>, looks: u32, open: usize) -> Result<Self> {
        let average = d3d::Latest::new(gpu, latest.width, latest.height)?;
        let own = d3d::Latest::new(gpu, latest.width, latest.height)?;
        let own_view = quad::shader_view(gpu, &own.texture)?;
        use windows::Win32::Graphics::Direct3D11::{D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_USAGE_DEFAULT};
        let bind = (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32;
        let light = (0..if looks > 1 { open.clamp(1, 2) } else { 0 })
            .map(|_| {
                let t = gpu.texture(latest.width, latest.height, windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R16G16B16A16_FLOAT, bind, D3D11_USAGE_DEFAULT, 0)?;
                Ok((quad::target_view(gpu, &t)?, quad::shader_view(gpu, &t)?))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            light,
            quad: quad::Quad::new(gpu)?,
            gpu: gpu.clone(),
            held_view: own_view.clone(),
            own_view,
            own,
            held_keep: None,
            held_size: (latest.width, latest.height),
            target: quad::target_view(gpu, &average.texture)?,
            average,
            weight: [0.0; 2],
            pending: [0.0; 2],
        })
    }

    /// Which average frame `k` is made in.
    fn slot(&self, k: i64) -> usize {
        k.rem_euclid(self.light.len().max(1) as i64) as usize
    }

    /// The capture's newest picture is the one that's up now (copied).
    fn hold(&mut self, latest: &d3d::Latest) {
        self.flush();
        unsafe { self.gpu.context.CopyResource(&self.own.texture, &latest.texture) };
        self.held_view = self.own_view.clone();
        self.held_keep = None;
        self.held_size = latest.content();
    }

    /// The same, for a picture the capture handed over whole.
    fn hold_queued(&mut self, q: d3d::Queued) {
        self.flush();
        self.held_view = q.view;
        self.held_keep = Some(q.texture);
        self.held_size = q.size;
    }

    /// Add a look at the picture that's up to frame `k`'s average, by `weight`.
    fn add(&mut self, k: i64, weight: f32) {
        let i = self.slot(k);
        self.pending[i] += weight;
    }

    /// No looks in frame `k` so far (no picture has come up yet).
    fn is_empty(&self, k: i64) -> bool {
        let i = self.slot(k);
        self.weight[i] + self.pending[i] == 0.0
    }

    /// Frame `k` is skipped: its looks are let go of unmixed, and its
    /// average starts empty for the frame after next (the picture that's up
    /// stays up).
    fn drop_frame(&mut self, k: i64) {
        let i = self.slot(k);
        self.weight[i] = 0.0;
        self.pending[i] = 0.0;
    }

    /// Every frame being made is skipped.
    fn drop_all(&mut self) {
        self.weight = [0.0; 2];
        self.pending = [0.0; 2];
    }

    /// Mix the looks at the picture that's up into the averages, at once.
    fn flush(&mut self) {
        for i in 0..2 {
            let weight = std::mem::take(&mut self.pending[i]);
            if weight == 0.0 {
                continue;
            }
            self.weight[i] += weight;
            // Mixing in by its share of the total so far keeps it the average.
            if let Some((target, _)) = self.light.get(i) {
                self.quad.average_light(&self.gpu, target, (self.own.width, self.own.height), &self.held_view, weight / self.weight[i]);
            }
        }
    }

    /// Frame `k` is done: its average is ready for the converter, and starts
    /// empty for the frame after next. Not for an empty one (see `is_empty`).
    fn finish(&mut self, k: i64) {
        self.flush();
        let i = self.slot(k);
        match self.light.get(i) {
            Some((_, view)) => self.quad.from_light(&self.gpu, &self.target, (self.own.width, self.own.height), view),
            // One look: the picture itself.
            None => unsafe { self.gpu.context.CopyResource(&self.average.texture, self.held_keep.as_ref().unwrap_or(&self.own.texture)) },
        }
        self.average.mark_frame(self.held_size);
        self.weight[i] = 0.0;
    }
}

/// How long after a frame's time it's made: the game capture knows a picture
/// is new a few milliseconds after it came up (it compares it with the last),
/// and it must still get into its frame; Desktop Duplication asks for new
/// pictures every half of it while the screen is still. A guess.
const BLEND_LATE: f64 = 0.012;
/// How much of the way the looks move toward halfway between the game's
/// pictures, on each picture. A guess: slow enough that the noise in
/// pictures' times averages out (a few hundred milliseconds), quick enough to
/// follow a game that drifts.
const PHASE_GAIN: f64 = 0.03;
/// How far behind its frames the pacer may fall (seconds) before it skips
/// the ones it missed instead of making each. Made in a burst, they'd mostly
/// be skipped anyway (more than the encoder takes at once) and mostly repeat
/// one picture (the capture keeps only its last few, `QUEUE_MAX`), and
/// blending them is more work for a graphics card that's already behind.
/// Without this, a pacer short of time fell behind for good, and quietly:
/// measured with each frame made to take 20 ms at 60 fps, it was 1.9 s behind
/// after 10 s and had skipped none (the file's video falls behind its sound).
/// A quarter second is 15 frames at 60 fps, about as many as the encoder has
/// (16).
const MAX_BEHIND: f64 = 0.25;

struct BlendPacer {
    t0: f64,
    /// Seconds between looks (one over the blend rate), and looks per frame.
    step: f64,
    looks: i64,
    /// Look `m` is at `t0 + phase + (m + 0.5) * step`; frame `k`'s own time
    /// is looks `k * looks` to `(k + 1) * looks - 1`.
    phase: f64,
    /// Which frames each look counts toward.
    shutter: crate::shutter::Shutter,
    /// The next look to take.
    next: i64,
    /// The last frame done (made, skipped or empty): later looks don't count
    /// toward it.
    closed: i64,
    /// The picture that's up (a number for each, to count different ones),
    /// and the one the last look took.
    held: Option<u64>,
    pictures: u64,
    last_taken: Option<u64>,
    /// Different pictures in the frame's own time so far.
    different: u32,
    blend: FrameBlend,
    converter: d3d::Converter,
    encoder: h264::Encoder,
    preview: d3d::Previewer,
    camera: Option<camera::CameraLayer>,
    dropped: u64,
    stats: BlendStats,
    diag: EncodeDiag,
}

/// Where frames wait, logged for every 5 s in which frames were skipped
/// (every 5 s with `HASTECLIP_ENCODE_LOG=1`), so a log from a PC that can't
/// keep up says what it's short of: how late the pacer closed frames and how
/// long closing took (CPU), how long the GPU took to finish a frame's work
/// after it was submitted, how long the encoder held frames, the fewest free
/// encoder frames, and why frames were skipped.
struct EncodeDiag {
    verbose: bool,
    since: Instant,
    frames: u32,
    late_max: f64,
    close_max: Duration,
    /// Queries ending each frame's GPU work, oldest first, with when.
    pending: std::collections::VecDeque<(windows::Win32::Graphics::Direct3D11::ID3D11Query, Instant)>,
    spare: Vec<windows::Win32::Graphics::Direct3D11::ID3D11Query>,
    gpu_ms: Vec<f64>,
    free_min: usize,
    pool_empty: u32,
    queue_full: u32,
    caught_up: u32,
}

impl EncodeDiag {
    fn new() -> Self {
        Self {
            verbose: std::env::var_os("HASTECLIP_ENCODE_LOG").is_some(),
            since: Instant::now(),
            frames: 0,
            late_max: 0.0,
            close_max: Duration::ZERO,
            pending: Default::default(),
            spare: Vec::new(),
            gpu_ms: Vec::new(),
            free_min: usize::MAX,
            pool_empty: 0,
            queue_full: 0,
            caught_up: 0,
        }
    }

    fn frame(&mut self, gpu: &d3d::Gpu, late: f64, close: Duration) {
        use windows::Win32::Graphics::Direct3D11::{D3D11_ASYNC_GETDATA_DONOTFLUSH, D3D11_QUERY_DESC, D3D11_QUERY_EVENT};
        self.frames += 1;
        self.late_max = self.late_max.max(late);
        self.close_max = self.close_max.max(close);
        unsafe {
            while let Some((q, at)) = self.pending.front() {
                let mut done = 0i32;
                let r = gpu.context.GetData(q, Some(&mut done as *mut i32 as *mut _), 4, D3D11_ASYNC_GETDATA_DONOTFLUSH.0 as u32);
                if r.is_err() || done == 0 {
                    break;
                }
                self.gpu_ms.push(at.elapsed().as_secs_f64() * 1000.0);
                let (q, _) = self.pending.pop_front().unwrap();
                self.spare.push(q);
            }
            let q = self.spare.pop().or_else(|| {
                let mut q = None;
                gpu.device.CreateQuery(&D3D11_QUERY_DESC { Query: D3D11_QUERY_EVENT, MiscFlags: 0 }, Some(&mut q)).ok()?;
                q
            });
            if let Some(q) = q {
                gpu.context.End(&q);
                self.pending.push_back((q, Instant::now()));
            }
        }
        if self.since.elapsed() < Duration::from_secs(5) {
            return;
        }
        use std::sync::atomic::Ordering::Relaxed;
        let held_max = h264::HELD_MAX_US.swap(0, Relaxed) as f64 / 1000.0;
        let held_n = h264::HELD_N.swap(0, Relaxed).max(1);
        let held_avg = h264::HELD_SUM_US.swap(0, Relaxed) as f64 / 1000.0 / held_n as f64;
        if self.verbose || self.pool_empty + self.queue_full + self.caught_up > 0 {
            self.gpu_ms.sort_by(f64::total_cmp);
            let pct = |p: f64| self.gpu_ms.get(((self.gpu_ms.len() as f64 - 1.0) * p).round() as usize).copied().unwrap_or(f64::NAN);
            eprintln!(
                "encode: {} frames; closed up to {:.1} ms late, closing took up to {:.1} ms; GPU done after {:.1}/{:.1}/{:.1} ms (median/99%/max), {} unfinished; encoder held frames {:.1} ms avg, {:.1} max; fewest free frames {}; skipped {} (no free frame) + {} (queue full) + {} (caught up)",
                self.frames,
                self.late_max * 1000.0,
                self.close_max.as_secs_f64() * 1000.0,
                pct(0.5),
                pct(0.99),
                pct(1.0),
                self.pending.len(),
                held_avg,
                held_max,
                self.free_min,
                self.pool_empty,
                self.queue_full,
                self.caught_up,
            );
        }
        let pending = std::mem::take(&mut self.pending);
        let spare = std::mem::take(&mut self.spare);
        *self = Self::new();
        self.pending = pending;
        self.spare = spare;
    }
}

impl BlendPacer {
    fn look_time(&self, m: i64) -> f64 {
        self.t0 + self.phase + (m as f64 + 0.5) * self.step
    }

    /// When the next frame to make is done: its last look.
    fn frame_end(&self) -> f64 {
        self.look_time(self.shutter.last_look(self.closed + 1))
    }

    /// Fallen more than `MAX_BEHIND` behind `t` (what looks are taken up
    /// to): the frames whose time is past are skipped at once, the one being
    /// made with them, and it goes on with the frame `t` is in.
    fn catch_up(&mut self, t: f64) {
        let behind = t - self.frame_end();
        if behind < MAX_BEHIND {
            return;
        }
        // The first look not due yet, and its frame.
        let due = ((t - self.t0 - self.phase) / self.step - 0.5).ceil() as i64;
        let (frame, _) = self.shutter.split(due);
        let missed = frame - (self.closed + 1);
        self.next = self.shutter.first_look(frame);
        self.closed = frame - 1;
        self.different = 0;
        self.blend.drop_all();
        // Before the first picture, no frames would have been made.
        if self.held.is_some() {
            skipped(&mut self.dropped, missed as u64);
            self.stats.caught_up(missed as u32, behind);
            self.diag.caught_up += missed as u32;
        }
    }

    /// Take every look before `t`: each sees the picture that's up.
    fn look_until(&mut self, t: f64) {
        let mut burst = 0;
        while self.look_time(self.next) < t && burst < 10_000 {
            self.look();
            burst += 1;
        }
    }

    fn look(&mut self) {
        let (first, weights) = self.shutter.split(self.next);
        if let Some(id) = self.held {
            for (k, weight) in [(first, weights[0]), (first + 1, weights[1])] {
                if k > self.closed && weight > 0.0 {
                    self.blend.add(k, weight);
                }
            }
            if self.last_taken != Some(id) {
                self.different += 1;
                self.last_taken = Some(id);
            }
        }
        self.next += 1;
        if self.next % self.looks == 0 {
            let different = std::mem::take(&mut self.different);
            if self.held.is_some() {
                self.stats.frame(different, self.looks as u32);
            }
        }
        // The frames no later look counts toward are done.
        let (first, _) = self.shutter.split(self.next);
        while self.closed < first - 1 {
            self.close(self.closed + 1);
        }
    }

    /// Frame `k` is done: encode it.
    fn close(&mut self, k: i64) {
        self.closed = k;
        let started = Instant::now();
        let late = host_now() - self.look_time(self.shutter.last_look(k));
        if self.blend.is_empty(k) {
            return;
        }
        if let Some(c) = self.camera.as_mut() {
            c.pull();
        }
        self.diag.free_min = self.diag.free_min.min(self.encoder.pool.free_count());
        match self.encoder.pool.take() {
            Some(slot) => {
                self.blend.finish(k);
                match self.converter.convert(slot) {
                    Ok(()) => {
                        if self.encoder.encode(slot, k) {
                            encoded();
                        } else {
                            skipped(&mut self.dropped, 1);
                            self.diag.queue_full += 1;
                        }
                    }
                    Err(e) => {
                        self.encoder.pool.give(slot);
                        eprintln!("{e:#}");
                    }
                }
            }
            // No frame free for the encoder: skipped, and not blended (work
            // a graphics card that's behind can't spare).
            None => {
                self.blend.drop_frame(k);
                skipped(&mut self.dropped, 1);
                self.diag.pool_empty += 1;
            }
        }
        if let Err(e) = self.preview.submit() {
            eprintln!("preview: {e:#}");
        }
        self.diag.frame(&self.blend.gpu, late, started.elapsed());
    }

    /// A new picture came up at `at`: the looks before it saw the one before.
    /// Then the looks move toward halfway between pictures.
    fn arrive(&mut self, at: f64, picture: Option<d3d::Queued>, latest: &d3d::Latest) {
        crate::count_picture();
        if at < self.look_time(self.next - 1) {
            self.stats.late += 1;
            crate::count_missed(1);
        }
        self.look_until(at);
        // Pictures belong on whole steps of the looks (halfway between two).
        let steps = (at - self.t0 - self.phase) / self.step;
        self.phase += (steps - steps.round()) * self.step * PHASE_GAIN;
        // A whole step either way is the same lock, one look over: a game a
        // little off the blend rate gets a picture seen twice, or one missed,
        // now and then, as it must; every frame still has its looks.
        if self.phase > self.step / 2.0 {
            self.phase -= self.step;
        } else if self.phase < -self.step / 2.0 {
            self.phase += self.step;
        }
        match picture {
            Some(q) => self.blend.hold_queued(q),
            None => self.blend.hold(latest),
        }
        self.pictures += 1;
        self.held = Some(self.pictures);
        self.stats.pictures += 1;
    }
}

/// What the blending saw, printed every few seconds (to the log): how many
/// pictures the game showed, how many different ones each frame got (all its
/// looks, when the game runs at least at the blend rate), how many came
/// too late for their frame, and the frames skipped to catch up (and how
/// far behind it was at most).
struct BlendStats {
    since: Instant,
    pictures: u32,
    frames: u32,
    full: u32,
    fewest: u32,
    most: u32,
    late: u32,
    caught_up: u32,
    behind: f64,
}

impl BlendStats {
    fn new() -> Self {
        Self { since: Instant::now(), pictures: 0, frames: 0, full: 0, fewest: u32::MAX, most: 0, late: 0, caught_up: 0, behind: 0.0 }
    }

    fn caught_up(&mut self, frames: u32, behind: f64) {
        self.caught_up += frames;
        self.behind = self.behind.max(behind);
    }

    fn frame(&mut self, different: u32, looks: u32) {
        self.frames += 1;
        self.full += (different == looks) as u32;
        self.fewest = self.fewest.min(different);
        self.most = self.most.max(different);
        if self.since.elapsed() >= Duration::from_secs(5) {
            eprintln!(
                "{}: the game showed {:.0} pictures/s; {looks} looks per frame, {}-{} different pictures in each, {:.0}% of frames with {looks}; {} pictures too late, over {} frames{}",
                if looks == 1 { "frame timing" } else { "frame blending" },
                self.pictures as f64 / self.since.elapsed().as_secs_f64(),
                self.fewest,
                self.most,
                self.full as f64 * 100.0 / self.frames as f64,
                self.late,
                self.frames,
                match self.caught_up {
                    0 => String::new(),
                    n => format!("; fell up to {:.2} s behind, skipped {n} frames to catch up", self.behind),
                }
            );
            *self = Self::new();
        }
    }
}

/// Like [`spawn_pacer`], for frame blending (see [`FrameBlend`]).
#[allow(clippy::too_many_arguments)]
fn spawn_blend_pacer(
    fps: u32,
    looks: u32,
    shutter: crate::shutter::Shutter,
    latest: Arc<d3d::Latest>,
    blend: FrameBlend,
    converter: d3d::Converter,
    encoder: h264::Encoder,
    preview: d3d::Previewer,
    camera: Option<camera::CameraLayer>,
    clock: Arc<Clock>,
) -> (Arc<AtomicBool>, JoinHandle<()>) {
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let handle = thread::spawn(move || {
        system::multimedia_thread("Capture");
        crate::blending(fps, looks);
        let t0 = host_now();
        clock.set(t0);
        let mut p = BlendPacer {
            t0,
            step: 1.0 / (fps.max(1) * looks.max(1)) as f64,
            looks: looks.max(1) as i64,
            phase: 0.0,
            shutter,
            next: 0,
            closed: -1,
            held: None,
            pictures: 0,
            last_taken: None,
            different: 0,
            blend,
            converter,
            encoder,
            preview,
            camera,
            dropped: 0,
            stats: BlendStats::new(),
            diag: EncodeDiag::new(),
        };
        if latest.has_frame() {
            p.blend.hold(&latest);
            p.held = Some(0);
        }
        let mut others = latest.others();
        while !stop2.load(Ordering::Relaxed) {
            p.catch_up(host_now() - BLEND_LATE);
            let (seen, _) = latest.arrivals();
            let wait = p.frame_end() + BLEND_LATE - host_now();
            if wait > 0.0 && latest.queue_len() == 0 {
                latest.wait_for_frame(seen, Duration::from_secs_f64(wait.min(0.05)));
            }
            for q in latest.drain_queue() {
                p.arrive(q.at, Some(q), &latest);
            }
            // Put up some other way (a still): take it from the capture.
            let now_others = latest.others();
            if now_others != others {
                others = now_others;
                p.arrive(host_now(), None, &latest);
            }
            // Looks whose time is past (and late pictures' too) see what's up.
            p.look_until(host_now() - BLEND_LATE);
        }
        p.encoder.finish();
    });
    (stop, handle)
}

/// A frame went to the encoder: counted for the UI (the share skipped).
fn encoded() {
    crate::count_encoded();
}

/// The encoder can't keep up (no free frame for it, or its queue is full),
/// or the pacer fell far behind: `n` frames are skipped, and the file shows
/// the one before them longer. Counted for the UI, and logged each time the
/// total passes a power of two.
fn skipped(dropped: &mut u64, n: u64) {
    let before = *dropped;
    *dropped += n;
    crate::count_skipped(n);
    if n > 0 && (before == 0 || dropped.ilog2() > before.ilog2()) {
        eprintln!("video is behind: {dropped} frames skipped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Graphics::Direct3D11::*;
    use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;

    /// The picture's one colour, as grey (0–1).
    fn fill(gpu: &d3d::Gpu, latest: &d3d::Latest, grey: f32) {
        let target = quad::target_view(gpu, &latest.texture).unwrap();
        unsafe { gpu.context.ClearRenderTargetView(&target, &[grey, grey, grey, 1.0]) };
    }

    /// The first pixel's green, 0–255.
    fn read(gpu: &d3d::Gpu, latest: &d3d::Latest) -> u8 {
        let staging = gpu.texture(latest.width, latest.height, DXGI_FORMAT_B8G8R8A8_UNORM, 0, D3D11_USAGE_STAGING, D3D11_CPU_ACCESS_READ.0 as u32).unwrap();
        unsafe {
            gpu.context.CopyResource(&staging, &latest.texture);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            gpu.context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).unwrap();
            let g = *(mapped.pData as *const u8).add(1);
            gpu.context.Unmap(&staging, 0);
            g
        }
    }

    /// Blended softly, two frames are made at once, each from its own looks
    /// by its own weights: white then black, a quarter and three quarters
    /// of the light.
    #[test]
    fn two_frames_at_once() {
        let Ok(gpu) = d3d::Gpu::new() else {
            eprintln!("no graphics card: skipped");
            return;
        };
        let latest = d3d::Latest::new(&gpu, 8, 8).unwrap();
        let mut blend = FrameBlend::new(&gpu, &latest, 4, 2).unwrap();
        fill(&gpu, &latest, 1.0);
        blend.hold(&latest);
        blend.add(0, 0.25);
        blend.add(1, 0.75);
        fill(&gpu, &latest, 0.0);
        blend.hold(&latest);
        blend.add(0, 0.75);
        blend.add(1, 0.25);
        blend.finish(0);
        let first = read(&gpu, &blend.average);
        blend.finish(1);
        let second = read(&gpu, &blend.average);
        // sRGB of 0.25 light is 0.537 (137), of 0.75 is 0.881 (225).
        assert!(first.abs_diff(137) <= 1, "{first}");
        assert!(second.abs_diff(225) <= 1, "{second}");
        assert!(blend.is_empty(2) && blend.is_empty(3), "both start empty again");
    }

    /// A fullscreen game that's seen is looked at again 5 s on, then twice as
    /// long each time, up to a minute.
    #[test]
    fn looks_again_later_and_later() {
        let mut sight = Sight::default();
        let waits: Vec<u64> = (0..6)
            .map(|_| {
                sight.look_later();
                sight.wait.as_secs()
            })
            .collect();
        assert_eq!(waits, [5, 10, 20, 40, 60, 60]);
    }

    /// With nothing running, a screenshot of the main screen is taken by a
    /// capture of its own: the screen's size, opaque, not all black.
    /// `HASTECLIP_SHOT=<file.png>` keeps it to look at.
    #[test]
    #[ignore = "captures the screen: cargo test -p capture screenshot_of_the_screen -- --ignored"]
    fn screenshot_of_the_screen() {
        let screen = system::list_screens().into_iter().next().expect("a screen");
        let video = VideoSource::Screen { id: screen.id.clone(), hook: crate::GameHook { auto: false, allowed: Vec::new() }, method: Default::default() };
        let mut rec = WinRecorder::new(Arc::new(LiveAudio::default()));
        let t = Instant::now();
        let img = rec.screenshot(&video).unwrap().finish().unwrap();
        eprintln!("{}×{} in {} ms", img.width, img.height, t.elapsed().as_millis());
        let (w, h) = system::display_pixels(Some(&screen.id)).expect("its size");
        assert_eq!((img.width, img.height), (w, h));
        assert_eq!(img.bgra.len(), (w * h * 4) as usize);
        assert!(img.bgra.chunks_exact(4).all(|p| p[3] == 255));
        assert!(img.bgra.chunks_exact(4).any(|p| p[0] > 16 || p[1] > 16 || p[2] > 16), "all black");
        if let Some(out) = std::env::var_os("HASTECLIP_SHOT") {
            let rgb: Vec<u8> = img.bgra.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0]]).collect();
            let header = format!("P6\n{w} {h}\n255\n");
            std::fs::write(out, [header.as_bytes(), &rgb].concat()).unwrap();
        }
    }

    /// While clipping at 720p in 1:1, a screenshot is still all of the screen
    /// at its own size: the recording's shape and size aren't the picture's.
    #[test]
    #[ignore = "captures the screen: cargo test -p capture screenshot_while_clipping -- --ignored"]
    fn screenshot_while_clipping() {
        let screen = system::list_screens().into_iter().next().expect("a screen");
        let video = VideoSource::Screen { id: screen.id.clone(), hook: crate::GameHook { auto: false, allowed: Vec::new() }, method: Default::default() };
        let settings = EncodeSettings {
            output_dir: std::env::temp_dir().join("hc-shot-test"),
            container_ext: "mp4".into(),
            fps: 30,
            blend_samples: 1,
            blend_shape: crate::BlendShape::Even,
            video_bitrate_kbps: 4000,
            size: crate::VideoSize { short_side: Some(720), shape: Some(crate::Shape { w: 1, h: 1, framing: crate::Framing::Fill }), exact: None },
            use_hardware: true,
            replay_seconds: 5,
            video: video.clone(),
            away_screen: None,
            webcam: None,
            screen: Arc::new(std::sync::Mutex::new(crate::ScreenPlacement::whole(crate::Framing::Fill))),
            sources: Vec::new(),
        };
        let mut rec = WinRecorder::new(Arc::new(LiveAudio::default()));
        rec.start(Mode::ReplayBuffer, &settings).unwrap();
        std::thread::sleep(Duration::from_millis(500));
        let t = Instant::now();
        let img = rec.screenshot(&video).unwrap().finish().unwrap();
        eprintln!("{}×{} in {} ms", img.width, img.height, t.elapsed().as_millis());
        rec.stop(None).unwrap();
        let (w, h) = system::display_pixels(Some(&screen.id)).expect("its size");
        assert_eq!((img.width, img.height), (w, h));
        assert!(img.bgra.chunks_exact(4).any(|p| p[0] > 16 || p[1] > 16 || p[2] > 16), "all black");
    }

    /// A display's mode, changed for as long as this lives: not saved, so
    /// dropping it puts back the one in the registry.
    struct ModeChange(Vec<u16>);

    impl ModeChange {
        fn to(id: &str, w: u32, h: u32) -> Result<Self> {
            use windows::Win32::Graphics::Gdi::{CDS_TYPE, ChangeDisplaySettingsExW, DEVMODEW, DISP_CHANGE_SUCCESSFUL, DM_PELSHEIGHT, DM_PELSWIDTH};
            let name: Vec<u16> = id.encode_utf16().chain([0]).collect();
            let mut mode = DEVMODEW { dmSize: std::mem::size_of::<DEVMODEW>() as u16, ..Default::default() };
            mode.dmPelsWidth = w;
            mode.dmPelsHeight = h;
            mode.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT;
            let r = unsafe { ChangeDisplaySettingsExW(windows::core::PCWSTR(name.as_ptr()), Some(&mode), None, CDS_TYPE(0), None) };
            if r != DISP_CHANGE_SUCCESSFUL {
                bail!("couldn't change {id} to {w}×{h}: {r:?}");
            }
            Ok(Self(name))
        }
    }

    impl Drop for ModeChange {
        fn drop(&mut self) {
            use windows::Win32::Graphics::Gdi::{CDS_TYPE, ChangeDisplaySettingsExW};
            unsafe { ChangeDisplaySettingsExW(windows::core::PCWSTR(self.0.as_ptr()), None, None, CDS_TYPE(0), None) };
        }
    }

    /// A small square whose grey keeps changing, at `left`, `top` on the
    /// desktop, so its display's capture always has a new picture. Closed
    /// once `stop` is set.
    fn changing_square(left: i32, top: i32, stop: Arc<AtomicBool>) -> JoinHandle<()> {
        thread::spawn(move || unsafe {
            use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
            use windows::Win32::Graphics::Gdi::{CreateSolidBrush, DeleteObject, FillRect, GetDC, ReleaseDC};
            use windows::Win32::System::LibraryLoader::GetModuleHandleW;
            use windows::Win32::UI::WindowsAndMessaging::*;
            extern "system" fn proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
                unsafe { DefWindowProcW(h, m, w, l) }
            }
            // Where it goes is in the display's own pixels.
            windows::Win32::UI::HiDpi::SetThreadDpiAwarenessContext(windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let instance = GetModuleHandleW(None).unwrap();
            let class = WNDCLASSW { lpfnWndProc: Some(proc), hInstance: instance.into(), lpszClassName: windows::core::w!("hc_changing_square"), ..Default::default() };
            RegisterClassW(&class);
            // On top, but never in focus or on the taskbar.
            let hwnd = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                windows::core::w!("hc_changing_square"),
                windows::core::w!(""),
                WS_POPUP | WS_VISIBLE,
                left,
                top,
                96,
                96,
                None,
                None,
                Some(instance.into()),
                None,
            )
            .unwrap();
            let dc = GetDC(Some(hwnd));
            let mut n = 0u32;
            while !stop.load(Ordering::Relaxed) {
                let mut msg = MSG::default();
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    DispatchMessageW(&msg);
                }
                // A slow ramp of greys, up and down: nothing flashes.
                let g = 64 + (n % 128).abs_diff(64);
                let brush = CreateSolidBrush(COLORREF(g | g << 8 | g << 16));
                FillRect(dc, &RECT { left: 0, top: 0, right: 96, bottom: 96 }, brush);
                let _ = DeleteObject(brush.into());
                n += 1;
                thread::sleep(Duration::from_millis(16));
            }
            ReleaseDC(Some(hwnd), dc);
            let _ = DestroyWindow(hwnd);
        })
    }

    /// Issue #16: a display's mode changing while it's recorded (there from
    /// 1920×1080 to 1440×1080) froze the picture with Desktop Duplication
    /// until the app was started again: each duplication started again was
    /// lost at once. Pictures must keep coming, at the new size, and again
    /// once the mode is back, from Desktop Duplication itself (not Windows'
    /// capture standing in). `SCREEN=<id>`: a display whose mode may change
    /// for half a minute; `MODE=WxH`: a mode the system lists for it (turned,
    /// on a turned display); `METHOD=dupl|wgc`; `HOLD=<s>` in each mode (15).
    #[test]
    #[ignore = "changes a display's mode: SCREEN=\\\\.\\DISPLAY2 MODE=1080x1440 METHOD=dupl cargo test -p capture display_mode_change -- --ignored --nocapture"]
    fn display_mode_change() {
        let env = |k: &str| std::env::var(k).ok();
        let mode = env("MODE").and_then(|m| {
            let (w, h) = m.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        });
        let (Some(id), Some((w, h))) = (env("SCREEN"), mode) else {
            eprintln!("SCREEN and MODE not set: skipped");
            return;
        };
        // Never the main display instead.
        assert!(system::list_screens().iter().any(|s| s.id == id), "no display {id}");
        let hold = Duration::from_secs_f64(env("HOLD").and_then(|s| s.parse().ok()).unwrap_or(15.0));
        let duplication = env("METHOD").as_deref() != Some("wgc");
        let method = if duplication { crate::ScreenCapture::Duplication } else { crate::ScreenCapture::Wgc };
        let settings = EncodeSettings {
            output_dir: std::env::temp_dir().join("hc-mode-test"),
            container_ext: "mp4".into(),
            fps: 30,
            blend_samples: 1,
            blend_shape: crate::BlendShape::Even,
            video_bitrate_kbps: 2000,
            size: crate::VideoSize { short_side: Some(360), shape: None, exact: None },
            use_hardware: true,
            replay_seconds: 5,
            video: VideoSource::Screen { id: id.clone(), hook: crate::GameHook { auto: false, allowed: Vec::new() }, method },
            away_screen: None,
            webcam: None,
            screen: Arc::new(std::sync::Mutex::new(crate::ScreenPlacement::whole(crate::Framing::Fit))),
            sources: Vec::new(),
        };
        unsafe { windows::Win32::UI::HiDpi::SetThreadDpiAwarenessContext(windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        let mut info = windows::Win32::Graphics::Gdi::MONITORINFO { cbSize: std::mem::size_of::<windows::Win32::Graphics::Gdi::MONITORINFO>() as u32, ..Default::default() };
        unsafe { windows::Win32::Graphics::Gdi::GetMonitorInfoW(system::find_monitor(&id), &mut info) }.ok().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let square = changing_square(info.rcMonitor.left + 40, info.rcMonitor.top + 40, stop.clone());

        let mut rec = WinRecorder::new(Arc::new(LiveAudio::default()));
        rec.start(Mode::ReplayBuffer, &settings).unwrap();
        let latest = rec.session.as_ref().unwrap().switch.as_ref().unwrap().latest.clone();
        let t0 = Instant::now();
        let mut seen = latest.arrivals().0;
        // Pictures a second, over each half second for `for_`, and how the
        // display was captured at the end.
        let mut watch = |phase: &str, for_: Duration| {
            let mut rates = Vec::new();
            let end = Instant::now() + for_;
            while Instant::now() < end {
                thread::sleep(Duration::from_millis(500));
                let n = latest.arrivals().0;
                rates.push((n - seen) * 2);
                eprintln!("{:5.1} s {phase}: {:3} pictures/s, {:?}, {:?}", t0.elapsed().as_secs_f64(), (n - seen) * 2, latest.content(), crate::screen_capture_in_use());
                seen = n;
            }
            (rates, crate::screen_capture_in_use())
        };
        let (before, _) = watch("before", Duration::from_secs(3));
        let changed = ModeChange::to(&id, w, h).unwrap();
        let (during, during_in_use) = watch("changed", hold);
        let changed_content = latest.content();
        drop(changed);
        let (back, back_in_use) = watch("back", hold);
        rec.stop(None).unwrap();
        stop.store(true, Ordering::Relaxed);
        let _ = square.join();

        // The last 3 s of each.
        let rate = |v: &[u64]| v[v.len().saturating_sub(6)..].iter().sum::<u64>() as f64 / v.len().clamp(1, 6) as f64;
        eprintln!("pictures/s: before {:.0}, changed {:.0}, back {:.0}", rate(&before), rate(&during), rate(&back));
        assert!(rate(&during) > 5.0, "no pictures since the mode changed");
        assert!(rate(&back) > 5.0, "no pictures since the mode came back");
        assert_eq!(changed_content, (w, h), "the picture isn't the new mode's size");
        let own = Some(crate::ScreenCaptureInUse { duplication, fell_back: false });
        assert_eq!((during_in_use, back_in_use), (own, own), "not captured the way it was asked to be");
    }

    /// The pixel at `x`, `y`, as BGRA.
    fn pixel(gpu: &d3d::Gpu, latest: &d3d::Latest, x: u32, y: u32) -> [u8; 4] {
        let staging = gpu.texture(1, 1, DXGI_FORMAT_B8G8R8A8_UNORM, 0, D3D11_USAGE_STAGING, D3D11_CPU_ACCESS_READ.0 as u32).unwrap();
        let region = D3D11_BOX { left: x, top: y, front: 0, right: x + 1, bottom: y + 1, back: 1 };
        unsafe {
            gpu.context.CopySubresourceRegion(&staging, 0, 0, 0, 0, &latest.texture, 0, Some(&region));
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            gpu.context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).unwrap();
            let p = *(mapped.pData as *const [u8; 4]);
            gpu.context.Unmap(&staging, 0);
            p
        }
    }

    /// A display bigger than the picture it's captured into (its mode grew
    /// after the capture started) is shrunk to fit, keeping its shape: drawn
    /// where it fits and nowhere else, not squeezed into all of it (Desktop
    /// Duplication) or cut off (Windows' capture). Nothing of the screen is
    /// kept. `SCREEN=<id>` (the main display).
    #[test]
    #[ignore = "captures the screen: cargo test -p capture shrinks_a_display_too_big -- --ignored"]
    fn shrinks_a_display_too_big() {
        let id = std::env::var("SCREEN").unwrap_or_else(|_| system::list_screens().into_iter().next().expect("a screen").id);
        let monitor = system::find_monitor(&id);
        let (w, h) = system::display_pixels(Some(&id)).expect("its size");
        let gpu = d3d::Gpu::new().unwrap();
        // Half its size, with room left under it.
        let (tw, th) = (w / 2, h / 2 + 64);
        let fitted = quad::fit_within(w, h, tw, th);
        let magenta = [255, 0, 255, 255];
        for duplication in [true, false] {
            let latest = d3d::Latest::new(&gpu, tw, th).unwrap();
            let target = quad::target_view(&gpu, &latest.texture).unwrap();
            unsafe { gpu.context.ClearRenderTargetView(&target, &[1.0, 0.0, 1.0, 1.0]) };
            let method = if duplication { "Desktop Duplication" } else { "Windows' capture" };
            // Kept until its picture's been looked at.
            let (dupl, wgc) = if duplication {
                (Some(dupl::DuplCapture::start(&gpu, monitor, &latest, 30).unwrap()), None)
            } else {
                (None, Some(d3d::Capture::start(&gpu, &d3d::monitor_item(monitor).unwrap(), &latest, None, 30).unwrap()))
            };
            let deadline = Instant::now() + Duration::from_secs(3);
            while !latest.has_frame() {
                assert!(Instant::now() < deadline, "{method}: no picture");
                thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(latest.content(), fitted, "{method}");
            assert_ne!(pixel(&gpu, &latest, fitted.0 / 2, fitted.1 / 2), magenta, "{method}: nothing drawn where it fits");
            assert_eq!(pixel(&gpu, &latest, fitted.0 / 2, fitted.1 + 32), magenta, "{method}: drawn past where it fits");
            eprintln!("{method}: {w}×{h} into {tw}×{th}, as {}×{}", fitted.0, fitted.1);
            drop((dupl, wgc));
        }
    }
}
