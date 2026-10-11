//! Native Windows capture backend: Windows Graphics Capture + Media Foundation +
//! WASAPI, all in-process — no external tools.
//!
//! - **Video**: WGC delivers the monitor, or one app's window, as BGRA textures
//!   on the GPU (`d3d`). A pacer thread converts the newest one to NV12 at the
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
        match (AppsConfig::of(video), self.session.as_ref().and_then(|s| s.apps.as_ref())) {
            (Some(config), Some(list)) => {
                *list.lock().unwrap() = config;
                true
            }
            _ => false,
        }
    }

    fn set_mic(&mut self, id: &str, device: &str) -> bool {
        self.session.as_mut().and_then(|s| s.audio.as_mut()).is_some_and(|a| a.set_mic(id, device))
    }
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
        crate::FRAMES_SKIPPED.store(0, Ordering::Relaxed);
        system::com_init();
        system::mf_startup()?;
        let gpu = d3d::Gpu::new()?;
        let (picture_source, (canvas_w, canvas_h)) = PictureSource::plan(&s.video)?;
        let (width, height) = crate::output_size(canvas_w, canvas_h, s.target_height);
        let clock = Arc::new(Clock::default());

        let (titles, comment, has_rest) = track_layout(&s.sources);
        let aac: Vec<AacEncoder> = titles.iter().map(|_| AacEncoder::new(AUDIO_BITRATE)).collect::<Result<_>>()?;
        let layout = Layout {
            spec: Spec { width, height, fps: s.fps, audio_titles: titles, audio_bitrate: AUDIO_BITRATE, comment, mov },
        };
        let writer = match target {
            Target::Record(file) => Writer::record(file, layout, s.fps),
            Target::Replay(seconds) => Writer::replay(seconds, layout, s.fps),
        };
        let writer_tx = writer.tx.clone();

        let mut session =
            Self { video: None, apps: None, pacer: None, audio: None, mixer: None, encoders: Vec::new(), writer_tx, writer: Some(writer) };
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
            // With blending, frames come from an average of the pictures shown
            // during them, not the newest picture.
            let blend = if s.blend_samples > 1 { Some(FrameBlend::new(&gpu, &latest)?) } else { None };
            latest.set_wants_queue(blend.is_some());
            let converter_source = blend.as_ref().map_or(&latest, |b| &b.average);
            let converter = d3d::Converter::new(&gpu, converter_source, overlay.clone(), &encoder.pool.textures, true, width, height, s.fps)?;
            let preview = d3d::Previewer::new(&gpu, &latest, overlay, width, height, s.fps, crate::preview::new_producer())?;
            session.pacer = Some(match blend {
                Some(blend) => spawn_blend_pacer(s.fps, latest, blend, converter, encoder, preview, camera, clock),
                None => spawn_pacer(s.fps, latest, converter, encoder, preview, camera, clock),
            });
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
    /// A display (and which), and when to switch to the game capture hook.
    Screen(windows::Graphics::Capture::GraphicsCaptureItem, windows::Win32::Graphics::Gdi::HMONITOR, crate::GameHook),
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
            VideoSource::Screen { id, hook } => {
                let monitor = system::find_monitor(id);
                let item = d3d::monitor_item(monitor)?;
                let size = item.Size()?;
                (PictureSource::Screen(item, monitor, hook.clone()), (size.Width.max(2) as u32, size.Height.max(2) as u32))
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
        // it gets room for a window on the biggest of them.
        let (copy_w, copy_h) = match &self {
            PictureSource::Screen(..) => (width, height),
            PictureSource::Apps(_) => {
                let (w, h) = system::largest_display_box();
                (w.max(width), h.max(height))
            }
        };
        let latest = d3d::Latest::new(gpu, copy_w, copy_h)?;
        let video = match self {
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
            PictureSource::Screen(item, monitor, hook) => {
                let capture = d3d::Capture::start(gpu, &item, &latest, None, fps)?;
                // Wait for the first frame so a capture that can't see the screen
                // fails here, visibly, instead of producing an empty file.
                let deadline = Instant::now() + Duration::from_secs(3);
                while !latest.has_frame() {
                    if Instant::now() >= deadline {
                        bail!("the screen isn't delivering frames");
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Video::follow_screen(gpu.clone(), latest.clone(), item, monitor, capture, fps, hook)
            }
        };
        Ok(Picture { video, latest })
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
        target_height: Option<u32>,
        fps: u32,
        away: Option<Arc<crate::StillImage>>,
        webcam: Option<crate::webcam::Webcam>,
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
                let (width, height) = crate::output_size(w, h, target_height);
                let cam = webcam.as_ref().and_then(|wc| camera::CameraLayer::new(&gpu, wc).ok());
                let overlay = cam.as_ref().zip(webcam.as_ref()).map(|(c, wc)| (&c.latest, wc.placement.clone()));
                let preview = d3d::Previewer::new(&gpu, &picture.latest, overlay, width, height, fps, generation)?;
                Ok((picture, preview, cam))
            })();
            let (picture, preview, mut cam) = match opened {
                Ok(p) => p,
                Err(e) => {
                    *error2.lock().unwrap() = Some(format!("{e:#}"));
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
/// graphics card, `gpu`) while Windows' capture has had nothing from it for
/// this long. Windows' capture gets a frame for every frame a window
/// presents, the same picture again too (measured: a still window drawn 180
/// times a second, 180 frames a second), so a game on a still screen isn't
/// mistaken for this; one that stops drawing isn't busy on the graphics card.
const FROZEN: Duration = Duration::from_secs(2);

/// Percent of the graphics card's 3D engine that counts as drawing. A guess:
/// a window that does nothing but clear itself every frame measured 1%, one
/// that doesn't draw 0.
const DRAWING: f64 = 0.3;

/// Whether a fullscreen game's window that Windows' capture has had
/// nothing from for `FROZEN` is drawing all the same (`gpu`, sampled every
/// call while it's watched): exclusive fullscreen.
fn unseen_but_drawing(gpu: &mut Option<gpu::GpuUse>, window: windows::Win32::Foundation::HWND, since_last_frame: Duration) -> Option<f64> {
    let percent = gpu.as_mut()?.percent(system::window_pid(window));
    (since_last_frame > FROZEN && percent > DRAWING).then_some(percent)
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
            let mut gpu_use = gpu::GpuUse::new();
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
                // Windows' capture of a fullscreen game in focus sends nothing:
                // it can't see the game (exclusive fullscreen). The hook can.
                if let Some((h, app, WindowCapture::Wgc(capture, true))) = &current
                    && let Some(percent) = unseen_but_drawing(&mut gpu_use, *h, capture.since_last_frame())
                    && focused.as_ref().is_some_and(|(f, _, _)| f == h)
                    && !system::window_hidden(*h)
                    && system::covers_display(*h)
                {
                    eprintln!("{app}: drawing ({percent:.1}% of the graphics card) but Windows' capture has had nothing for {FROZEN:?}: exclusive fullscreen; switching to the game capture hook");
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

    /// Record a display: Windows' capture of it, except while a game that
    /// needs the game capture hook (see [`hook::plan`]) is fullscreen and in
    /// focus on it. Windows' capture of the display is stopped meanwhile: while
    /// it runs, some games lose their cursor.
    fn follow_screen(
        gpu: d3d::Gpu,
        latest: Arc<d3d::Latest>,
        item: windows::Graphics::Capture::GraphicsCaptureItem,
        monitor: windows::Win32::Graphics::Gdi::HMONITOR,
        first: d3d::Capture,
        fps: u32,
        settings: crate::GameHook,
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
            let mut hooked: Option<(windows::Win32::Foundation::HWND, String, hook::HookCapture)> = None;
            // How to capture each game window, worked out once.
            let mut plans: std::collections::HashMap<usize, hook::Plan> = Default::default();
            let mut hook_failed: std::collections::HashSet<usize> = Default::default();
            let mut frozen: std::collections::HashSet<String> = Default::default();
            let mut gpu_use = gpu::GpuUse::new();
            while !stop2.load(Ordering::Relaxed) {
                // A fullscreen window in focus on this display.
                let game = system::foreground_app()
                    .filter(|f| f.main && !system::window_hidden(f.hwnd) && system::fullscreen_on(f.hwnd, monitor))
                    .map(|f| (f.hwnd, f.exe));
                let plan = game.as_ref().map(|(h, exe)| {
                    *plans.entry(h.0 as usize).or_insert_with(|| {
                        let (plan, why) = hook::plan(exe, *h, &settings);
                        eprintln!("{exe} fullscreen: {} ({why})", if plan == hook::Plan::Hook { "the game capture hook" } else { "Windows' capture" });
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
                if hooked.is_none() && display.is_none() {
                    match d3d::Capture::start(&gpu, &item, &latest, None, fps) {
                        Ok(c) => display = Some(c),
                        Err(e) => eprintln!("can't record the display: {e:#}"),
                    }
                }
                // Windows' capture sends nothing new while a fullscreen game
                // is in focus: it can't see it (exclusive fullscreen).
                if let (Some(capture), Some((h, exe)), Some(hook::Plan::Watch)) = (&display, &game, plan)
                    && let Some(percent) = unseen_but_drawing(&mut gpu_use, *h, capture.since_last_frame())
                    && frozen.insert(exe.to_lowercase())
                {
                    eprintln!("{exe}: drawing ({percent:.1}% of the graphics card) but Windows' capture has had nothing for {FROZEN:?}: exclusive fullscreen; switching to the game capture hook");
                }
                let until = Instant::now() + Self::RESCAN;
                while Instant::now() < until && !stop2.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(20));
                }
            }
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
    })
}

// ---------------------------------------------------------------------------
// Pacer: constant frame rate
// ---------------------------------------------------------------------------

/// Live frame blending: each recorded frame is the average of every picture
/// the screen showed during its slot, each weighed by how long it was up
/// (so a steady 300 pictures a second give equal weights, and a stall just
/// holds one picture). Sampling the screen on a clock instead would catch some
/// pictures twice and miss others, differently each frame.
struct FrameBlend {
    quad: quad::Quad,
    gpu: d3d::Gpu,
    /// The picture that's currently up: a copy of the capture's (which it
    /// overwrites as soon as the next arrives), or one the capture handed
    /// over whole (`Queued`); and its valid size.
    own: Arc<d3d::Latest>,
    own_view: windows::Win32::Graphics::Direct3D11::ID3D11ShaderResourceView,
    held_view: windows::Win32::Graphics::Direct3D11::ID3D11ShaderResourceView,
    /// Keeps a handed-over picture's texture from going away.
    held_keep: Option<windows::Win32::Graphics::Direct3D11::ID3D11Texture2D>,
    held_size: (u32, u32),
    /// The average so far, drawn into; the converter reads it.
    average: Arc<d3d::Latest>,
    target: windows::Win32::Graphics::Direct3D11::ID3D11RenderTargetView,
    /// How much time the average holds so far, in seconds.
    weight: f64,
}

impl FrameBlend {
    fn new(gpu: &d3d::Gpu, latest: &Arc<d3d::Latest>) -> Result<Self> {
        let average = d3d::Latest::new(gpu, latest.width, latest.height)?;
        let own = d3d::Latest::new(gpu, latest.width, latest.height)?;
        let own_view = quad::shader_view(gpu, &own.texture)?;
        Ok(Self {
            quad: quad::Quad::new(gpu)?,
            gpu: gpu.clone(),
            held_view: own_view.clone(),
            own_view,
            own,
            held_keep: None,
            held_size: (latest.width, latest.height),
            target: quad::target_view(gpu, &average.texture)?,
            average,
            weight: 0.0,
        })
    }

    /// The screen just changed: keep the new picture as the one that's up.
    fn hold(&mut self, latest: &d3d::Latest) {
        unsafe { self.gpu.context.CopyResource(&self.own.texture, &latest.texture) };
        self.held_view = self.own_view.clone();
        self.held_keep = None;
        self.held_size = latest.content();
    }

    /// The same, for a picture the capture handed over whole.
    fn hold_queued(&mut self, q: d3d::Queued) {
        self.held_view = q.view;
        self.held_keep = Some(q.texture);
        self.held_size = q.size;
    }

    /// Add the picture that's up, for `seconds`, to the average.
    fn add(&mut self, seconds: f64) {
        if seconds <= 0.0 {
            return;
        }
        self.weight += seconds;
        // Mixing in by seconds / total so far keeps it the weighted average.
        self.quad.average(&self.gpu, &self.target, (self.own.width, self.own.height), &self.held_view, (seconds / self.weight) as f32);
    }

    /// The frame is done: its average is ready for the converter; the next
    /// one starts empty.
    fn finish(&mut self) {
        self.average.mark_frame(self.held_size);
        self.weight = 0.0;
    }
}

/// What the blending saw, printed every few seconds (to the log): how many
/// pictures arrived, how evenly, and how many went into each frame (by the
/// time they came up), and how many came too late for their frame, to tell a
/// capture that delivers unevenly from a blend that's wrong.
struct BlendStats {
    t0: f64,
    fps: f64,
    since: Instant,
    pictures: u32,
    last: f64,
    gap: (f64, f64),
    frame: i64,
    in_frame: u32,
    per_frame: (u32, u32),
    frames: u32,
    missed: u32,
    late: u32,
}

impl BlendStats {
    fn new(t0: f64, fps: u32) -> Self {
        Self { t0, fps: fps as f64, since: Instant::now(), pictures: 0, last: 0.0, gap: (f64::MAX, 0.0), frame: -1, in_frame: 0, per_frame: (u32::MAX, 0), frames: 0, missed: 0, late: 0 }
    }

    fn arrival(&mut self, at: f64) {
        if self.last > 0.0 {
            let g = (at - self.last) * 1000.0;
            self.gap = (self.gap.0.min(g), self.gap.1.max(g));
        }
        self.last = at;
        self.pictures += 1;
        let frame = ((at - self.t0) * self.fps).floor() as i64;
        if frame > self.frame {
            if self.frame >= 0 {
                self.per_frame = (self.per_frame.0.min(self.in_frame), self.per_frame.1.max(self.in_frame));
                self.frames += 1;
                // Frames between got nothing.
                let skipped = (frame - self.frame - 1) as u32;
                self.missed += skipped;
                self.frames += skipped;
                if skipped > 0 {
                    self.per_frame.0 = 0;
                }
            }
            self.frame = frame;
            self.in_frame = 0;
        }
        self.in_frame += 1;
        if self.since.elapsed() >= Duration::from_secs(5) {
            let secs = self.since.elapsed().as_secs_f64();
            eprintln!(
                "frame blending: {:.0} pictures/s, {:.2}-{:.2} ms apart; {}-{} per frame, {} frames without a new picture, {} pictures too late for their frame, over {} frames",
                self.pictures as f64 / secs,
                self.gap.0,
                self.gap.1,
                self.per_frame.0.min(self.per_frame.1),
                self.per_frame.1,
                self.missed,
                self.late,
                self.frames
            );
            *self = Self { last: self.last, frame: self.frame, in_frame: self.in_frame, ..Self::new(self.t0, self.fps as u32) };
        }
    }

    /// A picture arrived after its frame had been closed.
    fn too_late(&mut self) {
        self.late += 1;
    }
}

/// How long after a frame's time it's closed: a picture is known to be new a
/// few milliseconds after it showed (the game capture compares it with the last
/// one), and one that came up inside the frame must still get in. A guess.
const BLEND_LATE: f64 = 0.012;

/// Like [`spawn_pacer`], for frame blending: instead of looking at the screen
/// on a clock, every new picture is added to the frame it falls in, as it
/// arrives, weighed by how long it stays up.
fn spawn_blend_pacer(
    fps: u32,
    latest: Arc<d3d::Latest>,
    mut blend: FrameBlend,
    converter: d3d::Converter,
    encoder: h264::Encoder,
    preview: d3d::Previewer,
    mut camera: Option<camera::CameraLayer>,
    clock: Arc<Clock>,
) -> (Arc<AtomicBool>, JoinHandle<()>) {
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let handle = thread::spawn(move || {
        let fps_f = fps as f64;
        let t0 = host_now();
        clock.set(t0);
        let mut next: i64 = 0; // the frame being made: seconds next/fps .. (next+1)/fps after t0
        let mut dropped = 0u64;
        // When the picture that's up came up (not before the frame began).
        let mut up_since = t0;
        let mut have_picture = false;
        let (mut seen, _) = latest.arrivals();
        let mut stats = BlendStats::new(t0, fps);
        if latest.has_frame() {
            blend.hold(&latest);
            have_picture = true;
        }

        // The frame ends: encode it, and begin the next.
        let mut close = |blend: &mut FrameBlend, up_since: &mut f64, next: &mut i64, have_picture: bool| {
            let end = t0 + (*next + 1) as f64 / fps_f;
            if have_picture {
                blend.add(end - up_since.max(t0 + *next as f64 / fps_f));
                blend.finish();
                if let Some(c) = camera.as_mut() {
                    c.pull();
                }
                match encoder.pool.take() {
                    Some(slot) => match converter.convert(slot) {
                        Ok(()) => encoder.encode(slot, *next),
                        Err(e) => {
                            encoder.pool.give(slot);
                            eprintln!("{e:#}");
                        }
                    },
                    None => {
                        dropped += 1;
                        crate::FRAMES_SKIPPED.fetch_add(1, Ordering::Relaxed);
                        if dropped.is_power_of_two() {
                            eprintln!("video encoder is behind: {dropped} frames skipped");
                        }
                    }
                }
                if let Err(e) = preview.submit() {
                    eprintln!("preview: {e:#}");
                }
            }
            *up_since = end;
            *next += 1;
        };

        while !stop2.load(Ordering::Relaxed) {
            let frame_end = t0 + (next + 1) as f64 / fps_f + BLEND_LATE;
            let wait = frame_end - host_now();
            if wait > 0.0 {
                latest.wait_for_frame(seen, Duration::from_secs_f64(wait.min(0.05)));
            }
            // Every picture the capture handed over, in order; else the newest.
            let queued = latest.drain_queue();
            let (count, arrived) = latest.arrivals();
            let mut arrivals: Vec<(f64, Option<d3d::Queued>)> = queued.into_iter().map(|q| (q.at, Some(q))).collect();
            if arrivals.is_empty() && count != seen {
                arrivals.push((arrived, None));
            }
            seen = count;
            for (at, picture) in arrivals {
                // Frames that ended before the picture arrived took the old one.
                let mut burst = 0;
                while t0 + (next + 1) as f64 / fps_f <= at && burst < fps * 2 {
                    close(&mut blend, &mut up_since, &mut next, have_picture);
                    burst += 1;
                }
                if have_picture {
                    blend.add(at - up_since.max(t0 + next as f64 / fps_f));
                }
                match picture {
                    Some(q) => blend.hold_queued(q),
                    None => blend.hold(&latest),
                }
                stats.arrival(at);
                if at < t0 + next as f64 / fps_f {
                    stats.too_late();
                }
                have_picture = true;
                up_since = at;
            }
            let mut burst = 0;
            while t0 + (next + 1) as f64 / fps_f + BLEND_LATE <= host_now() && burst < fps * 2 {
                close(&mut blend, &mut up_since, &mut next, have_picture);
                burst += 1;
            }
        }
        encoder.finish();
    });
    (stop, handle)
}

/// Encode the newest screen image every 1/fps on the host clock. WGC only sends
/// frames when the screen changes; repeating the last one keeps the output CFR.
/// Frame N is stamped N/fps seconds after t0, the same clock the audio uses.
fn spawn_pacer(
    fps: u32,
    latest: Arc<d3d::Latest>,
    converter: d3d::Converter,
    encoder: h264::Encoder,
    preview: d3d::Previewer,
    mut camera: Option<camera::CameraLayer>,
    clock: Arc<Clock>,
) -> (Arc<AtomicBool>, JoinHandle<()>) {
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let handle = thread::spawn(move || {
        let fps_f = fps as f64;
        let t0 = host_now();
        clock.set(t0);
        let mut next: i64 = 0;
        let mut dropped = 0u64;

        while !stop2.load(Ordering::Relaxed) {
            let now = host_now();
            let due = ((now - t0) * fps_f) as i64;
            // After a stall, catch up gradually rather than in one huge burst.
            let mut burst = 0;
            while next <= due && burst < fps * 2 {
                if let Some(c) = camera.as_mut() {
                    c.pull();
                }
                if latest.has_frame() {
                    match encoder.pool.take() {
                        Some(slot) => match converter.convert(slot) {
                            Ok(()) => encoder.encode(slot, next),
                            Err(e) => {
                                encoder.pool.give(slot);
                                eprintln!("{e:#}");
                            }
                        },
                        // The encoder can't keep up: skip this frame (the file
                        // shows the previous one a little longer).
                        None => {
                            dropped += 1;
                            crate::FRAMES_SKIPPED.fetch_add(1, Ordering::Relaxed);
                            if dropped.is_power_of_two() {
                                eprintln!("video encoder is behind: {dropped} frames skipped");
                            }
                        }
                    }
                }
                next += 1;
                burst += 1;
            }
            // The Sources page is showing the preview: hand it this picture too.
            if let Err(e) = preview.submit() {
                eprintln!("preview: {e:#}");
            }
            let wake = t0 + next as f64 / fps_f;
            let wait = wake - host_now();
            if wait > 0.0 {
                thread::sleep(Duration::from_secs_f64(wait));
            }
        }
        encoder.finish();
    });
    (stop, handle)
}
