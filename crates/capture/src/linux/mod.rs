//! Native Linux capture backend: the screen from the X server on X11, from
//! the compositor on wlroots-style Wayland desktops, KDE Plasma and GNOME,
//! else through the ScreenCast desktop portal + PipeWire.
//! Works on every X11 desktop, Sway, Hyprland and their kin, KDE Plasma,
//! GNOME, and other Wayland desktops that have a portal.
//!
//! - **Video**: on X11 the monitor picked in the app is read straight from
//!   the X server (`x11`); on wlroots-style desktops it's copied from the
//!   compositor (`wlr`); on KDE Plasma KWin streams it over PipeWire
//!   (`kwin`), on GNOME Mutter does (`mutter`). Elsewhere the portal asks which screen to share (once; the
//!   choice is remembered) and hands over a PipeWire stream of it (`portal`,
//!   `screen`). A pacer thread takes the newest picture every 1/fps, scales
//!   it to the output size, draws the webcam over it and converts it to NV12,
//!   so the output is constant frame rate even when the screen is still and
//!   nothing new arrives. Where the compositor hands pictures over on the
//!   graphics card (DMA-BUF, `gpu`), that's done there (`gpu_frame`); else on
//!   the CPU (`image`). On NVIDIA cards their encoder takes the frame where
//!   it's made, on the card (`nvenc`), and nothing comes down to the CPU but
//!   the encoded frame. Elsewhere ffmpeg encodes the NV12 frame (`ffmpeg`):
//!   VA-API on the GPU where it can, x264 or OpenH264 otherwise.
//! - **Audio**: everything through PipeWire (`audio`): mics, the desktop
//!   (every app but us), and single apps. **Mixing** happens in Rust
//!   (`crate::mixer`), which also drives the live meters; each track is
//!   AAC-encoded by ffmpeg.
//! - **Writing** (`crate::writer`): our own MP4 muxer (`crate::mp4file`),
//!   either straight to a fragmented MP4 (record) or from an in-memory replay
//!   ring when a clip is saved — the same as on Windows.
//!
//! Everything is timestamped on CLOCK_MONOTONIC, the clock PipeWire uses: the
//! first video frame is t=0.
//!
//! Recording games and apps by their windows isn't offered: Wayland doesn't
//! let an app see which window is in front, or capture one without asking.

mod apps;
pub mod audio;
mod blend;
mod camera;
pub mod decode;
pub(crate) mod export;
mod ext_windows;
pub mod ffmpeg;
mod image;
pub mod focus;
mod gpu;
mod gpu_frame;
mod icons;
mod kwin;
mod kwin_windows;
pub mod monitor;
mod mutter;
mod nvenc;
mod portal;
mod screen;
mod vkenc;
mod wlr;
mod wlr_windows;
mod x11;
mod x11_windows;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

pub use camera::{keep_open as keep_camera_open, list_cameras};
pub use kwin::helper as kde_stream_helper;

use crate::mixer::{self, Clock, LiveAudio};
use crate::mp4file::Layout;
use crate::mp4mux::Spec;
use crate::output::{in_progress, timestamp};
use crate::sources::{AudioCapture, mix_inputs, track_layout};
use crate::webcam::{Placement, SharedPlacement};
use crate::writer::{self, Writer};
use crate::{AUDIO_BITRATE, Device, EncodeSettings, Mode, Recorder, VideoSource};
use image::{CameraPicture, Canvas};

/// CLOCK_MONOTONIC in seconds: PipeWire's clock, and so every timestamp's.
pub(crate) fn host_now() -> f64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: a valid clock id and a timespec to fill.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as f64 + ts.tv_nsec as f64 / 1e9
}

/// The one screen entry on Wayland: which screen is picked in the desktop's own dialog.
pub(crate) const SCREEN_ID: &str = "portal";

/// Whether the screens to record are picked in the app (X11, wlroots, KDE,
/// GNOME), not in the desktop's own dialog (other desktops, via the portal).
pub fn screens_listed() -> bool {
    x11::session() || wlr::available() || kwin::available() || mutter::available()
}

/// Screens to record. On X11, KDE and GNOME, every monitor, the main one
/// first; on wlroots-style desktops every monitor, leftmost first. Elsewhere the
/// desktop doesn't tell apps what screens there are, so there's one entry,
/// and the desktop asks which screen it is the first time.
pub fn list_screens() -> Vec<Device> {
    if x11::session() {
        let mut monitors = x11::monitors();
        monitors.sort_by_key(|m| (!m.primary, m.x, m.y));
        if !monitors.is_empty() {
            return monitors
                .into_iter()
                .map(|m| {
                    let main = if m.primary { ", main" } else { "" };
                    Device { name: format!("{} ({}×{}{main})", m.name, m.width, m.height), id: m.id }
                })
                .collect();
        }
    } else if wlr::available() {
        let monitors = wlr::monitors();
        if !monitors.is_empty() {
            return monitors
                .into_iter()
                .map(|m| Device { name: format!("{} ({}×{})", m.name, m.width, m.height), id: m.id })
                .collect();
        }
    } else if kwin::available() {
        let monitors = kwin::monitors();
        if !monitors.is_empty() {
            return monitors
                .into_iter()
                .map(|m| {
                    let main = if m.primary { ", main" } else { "" };
                    Device { name: format!("{} ({}×{}{main})", m.name, m.width, m.height), id: m.id }
                })
                .collect();
        }
    } else if mutter::available() {
        let monitors = mutter::monitors();
        if !monitors.is_empty() {
            return monitors
                .into_iter()
                .map(|m| {
                    let main = if m.primary { ", main" } else { "" };
                    Device { name: format!("{} ({}×{}{main})", m.name, m.width, m.height), id: m.id }
                })
                .collect();
        }
    }
    vec![Device { id: SCREEN_ID.into(), name: "Screen (picked when recording first starts)".into() }]
}

/// The refresh rate of monitor `id` (the fastest's for `None`), where the
/// desktop says: KDE Plasma, so far.
pub(crate) fn display_refresh(id: Option<&str>) -> Option<u32> {
    if x11::session() || !kwin::available() {
        return None;
    }
    let monitors = kwin::monitors();
    match id {
        Some(id) => monitors.iter().find(|m| m.id == id).and_then(|m| m.refresh),
        None => monitors.iter().filter_map(|m| m.refresh).max(),
    }
}

/// Whether frames can be blended: on the graphics card, when it takes the
/// screen's pictures.
pub(crate) fn frame_blending() -> bool {
    screen::gpu_frames().is_some()
}

/// Previews running on their threads, which `shutdown` waits for.
static PREVIEWS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Before the app exits: stop every capture still running in the
/// background, so none is using the graphics card while its driver unloads.
pub(crate) fn shutdown() {
    let deadline = Instant::now() + Duration::from_secs(2);
    while PREVIEWS.load(Ordering::Acquire) > 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    screen::shutdown();
}

/// Whether games and apps can be recorded (KDE Plasma, so far).
pub(crate) fn app_capture() -> bool {
    apps::available()
}

/// The open camera's own picture, shrunk.
pub(crate) fn camera_picture(max_width: u32, newer_than: u64) -> Option<crate::webcam::CameraPreview> {
    camera::picture(max_width, newer_than)
}

/// A camera's settings (V4L2 controls).
pub(crate) fn camera_controls(device: &str) -> Vec<crate::webcam::CameraControl> {
    camera::controls(device)
}

/// Set a camera's setting.
pub(crate) fn set_camera_control(device: &str, id: u32, value: i64) -> Result<()> {
    camera::set_control(device, id, value)
}

/// A program's icon from the desktop's icon theme.
pub(crate) fn app_icon(id: &str, size: u32) -> Option<Vec<u8>> {
    icons::app_icon(id, size)
}

/// Running programs with a window, for `VideoSource::Apps`.
pub(crate) fn list_windowed_apps() -> Vec<Device> {
    if apps::available() { apps::list() } else { Vec::new() }
}

/// Forget which screen was picked, so the next capture (the preview, when
/// it restarts) asks the desktop again.
pub fn choose_screen_again() {
    portal::forget_choice();
    screen::forget();
}

/// The screen's size in pixels: on X11, wlroots, KDE and GNOME the
/// monitor's, else the last one the portal shared.
pub(crate) fn display_pixels(id: Option<&str>) -> Option<(u32, u32)> {
    if x11::session()
        && let Some(m) = x11::find(&x11::monitors(), id.unwrap_or(""))
    {
        return Some((m.width as u32, m.height as u32));
    }
    if !x11::session() && wlr::available() {
        let monitors = wlr::monitors();
        if let Some(m) = monitors.iter().find(|m| Some(m.id.as_str()) == id).or(monitors.first()) {
            return Some((m.width, m.height));
        }
    }
    if !x11::session() && kwin::available() {
        let monitors = kwin::monitors();
        if let Some(m) = monitors.iter().find(|m| Some(m.id.as_str()) == id).or(monitors.first()) {
            return Some((m.width, m.height));
        }
    }
    if !x11::session() && mutter::available() {
        let monitors = mutter::monitors();
        if let Some(m) = monitors.iter().find(|m| Some(m.id.as_str()) == id).or(monitors.first()) {
            return Some((m.width, m.height));
        }
    }
    portal::last_size()
}

pub struct LinuxRecorder {
    session: Option<Session>,
    mode: Option<Mode>,
    current_file: Option<PathBuf>,
    replay_seconds: u32,
    output_dir: PathBuf,
    container_ext: String,
    live: Arc<LiveAudio>,
}

impl LinuxRecorder {
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

impl Recorder for LinuxRecorder {
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

    fn screenshot(&mut self, video: &VideoSource) -> Result<crate::PendingShot> {
        if let Some(feed) = self.session.as_ref().and_then(|s| s.feed.as_ref()) {
            return shot_of(feed);
        }
        // Nothing running: a capture of its own, for one picture. A screen
        // has its picture by now; games and apps take a moment to find the
        // one that's showing.
        let feed = Feed::open(video, None)?;
        let deadline = Instant::now() + Duration::from_secs(2);
        while (feed.get().is_none() || feed.waiting()) && Instant::now() < deadline {
            feed.want();
            thread::sleep(Duration::from_millis(10));
        }
        let shot = shot_of(&feed);
        feed.release();
        shot
    }

    fn set_mic(&mut self, id: &str, device: &str) -> bool {
        self.session.as_mut().and_then(|s| s.audio.as_mut()).is_some_and(|a| a.set_mic(id, device))
    }

    /// Record something else without restarting: another list of games and
    /// apps, or the whole screen instead of them (or the other way round, or
    /// another display), into the same picture. False if it couldn't.
    fn update_video(&mut self, video: &VideoSource) -> bool {
        let Some(session) = self.session.as_ref() else { return false };
        let Some(feed) = session.feed.as_ref() else { return false };
        match feed.switch(video, session.away_screen.as_deref()) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("can't switch what's recorded: {e:#}");
                false
            }
        }
    }
}

/// A screenshot of the newest picture: all of the screen, or the game's
/// window, at its own size, without the webcam. Made opaque B, G, R, A rows
/// when finished.
fn shot_of(feed: &Feed) -> Result<crate::PendingShot> {
    if feed.waiting() {
        bail!("none of the games and apps you record is showing");
    }
    let (frame, _) = feed.get().context("nothing has been captured yet. Try again in a moment")?;
    Ok(crate::PendingShot::new(move || {
        // On the graphics card: read back.
        if let Some(t) = &frame.texture {
            let card = gpu::gpu().context("the graphics card went away")?;
            let mut bgra = card.read(t);
            if t.format() == wgpu::TextureFormat::Rgba8Unorm {
                bgra.chunks_exact_mut(4).for_each(|p| p.swap(0, 2));
            }
            bgra.chunks_exact_mut(4).for_each(|p| p[3] = 255);
            return Ok(crate::StillImage { width: t.width(), height: t.height(), bgra });
        }
        let row = frame.width as usize * 4;
        let mut bgra = Vec::with_capacity(row * frame.height as usize);
        for line in frame.data.chunks(frame.stride).take(frame.height as usize) {
            for p in line[..row].chunks_exact(4) {
                match frame.order {
                    screen::Order::Bgr => bgra.extend_from_slice(&[p[0], p[1], p[2], 255]),
                    screen::Order::Rgb => bgra.extend_from_slice(&[p[2], p[1], p[0], 255]),
                }
            }
        }
        Ok(crate::StillImage { width: frame.width, height: frame.height, bgra })
    }))
}

impl Drop for LinuxRecorder {
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

/// How far behind real time the mixer runs, so every source's audio has
/// arrived (PipeWire delivers every ~10–20 ms, sometimes late). Timestamps
/// are exact, so this only delays writing — it never shifts audio against video.
const MIX_LATENCY: f64 = 0.3;

/// Where the pictures come from: a screen, or games and apps. Switched while
/// capturing (`switch`), it carries on with the new one's pictures, numbered
/// on from the old one's.
#[derive(Clone)]
struct Feed(Arc<Switchable>);

struct Switchable {
    source: std::sync::RwLock<Source>,
    /// Added to the source's own picture numbers: past the last one handed
    /// out before it was switched in, so they keep going up.
    base: std::sync::atomic::AtomicU64,
    /// The highest picture number handed out.
    last: std::sync::atomic::AtomicU64,
    /// Every picture is kept as it arrives (`queue_arrivals`): the next
    /// source too.
    queuing: AtomicBool,
}

enum Source {
    Screen(Arc<screen::Screen>),
    Apps(Arc<apps::Apps>),
}

impl Source {
    /// Start `source`'s pictures. Games and apps show the away screen (else
    /// black) until one of them is found.
    fn open(source: &VideoSource, away_screen: Option<&crate::StillImage>) -> Result<Self> {
        match source {
            VideoSource::Screen { id, .. } => Ok(Self::Screen(screen::acquire(id)?)),
            VideoSource::Apps { ids, away_when_unfocused, .. } if apps::available() => {
                Ok(Self::Apps(apps::Apps::start(ids, *away_when_unfocused, away_screen)))
            }
            VideoSource::Apps { .. } => bail!("recording games and apps is only available on KDE Plasma so far — record the screen instead"),
        }
    }

    /// Let go. A screen keeps running a little longer, in case it's taken up
    /// again right away (`screen::release`).
    fn release(self) {
        if let Self::Screen(s) = self {
            screen::release(s);
        }
    }
}

impl Feed {
    fn open(video: &VideoSource, away_screen: Option<&crate::StillImage>) -> Result<Self> {
        let source = Source::open(video, away_screen)?;
        Ok(Self(Arc::new(Switchable {
            source: std::sync::RwLock::new(source),
            base: Default::default(),
            last: Default::default(),
            queuing: AtomicBool::new(false),
        })))
    }

    /// The size it's recorded at: the screen's (or the main display's), as
    /// `size` makes it.
    fn recorded_size(&self, size: crate::VideoSize) -> (u32, u32) {
        let (w, h) = self.size();
        size.of(w, h)
    }

    fn source(&self) -> std::sync::RwLockReadGuard<'_, Source> {
        self.0.source.read().unwrap_or_else(|e| e.into_inner())
    }

    /// A picture number of the source's as one of ours.
    fn ours(&self, seq: u64) -> u64 {
        let n = seq + self.0.base.load(Ordering::Relaxed);
        self.0.last.fetch_max(n, Ordering::Relaxed);
        n
    }

    /// Carry on with `video`'s pictures: the same games and apps take the
    /// new list as they run; anything else is opened, then put in place of
    /// what was there.
    fn switch(&self, video: &VideoSource, away_screen: Option<&crate::StillImage>) -> Result<()> {
        if let (Source::Apps(a), VideoSource::Apps { ids, away_when_unfocused, .. }) = (&*self.source(), video) {
            a.set(ids, *away_when_unfocused);
            return Ok(());
        }
        let new = Source::open(video, away_screen)?;
        let queuing = self.0.queuing.load(Ordering::Relaxed);
        match &new {
            Source::Screen(s) if queuing => s.latest.queue_arrivals(),
            Source::Apps(a) if queuing => a.queue_arrivals(),
            _ => {}
        }
        let old = {
            let mut source = self.0.source.write().unwrap_or_else(|e| e.into_inner());
            self.0.base.store(self.0.last.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
            std::mem::replace(&mut *source, new)
        };
        old.release();
        Ok(())
    }

    /// The size pictures are fitted into before scaling: the screen's, or
    /// for games and apps the main display's.
    fn size(&self) -> (u32, u32) {
        match &*self.source() {
            Source::Screen(s) => s.latest.get().map_or((1920, 1080), |(f, _)| (f.width, f.height)),
            Source::Apps(_) => display_pixels(None).unwrap_or((1920, 1080)),
        }
    }

    /// Ask a source that reads the screen on request (X11) for the next picture.
    fn want(&self) {
        match &*self.source() {
            Source::Screen(s) => s.latest.want(),
            Source::Apps(a) => a.want(),
        }
    }

    /// The newest picture and a number that changes with it.
    fn get(&self) -> Option<(Arc<screen::Frame>, u64)> {
        let source = self.source();
        let got = match &*source {
            Source::Screen(s) => s.latest.get(),
            Source::Apps(a) => a.get(),
        };
        got.map(|(f, n)| (f, self.ours(n)))
    }

    /// Keep every picture as it arrives, for `drain` (frame blending).
    fn queue_arrivals(&self) {
        self.0.queuing.store(true, Ordering::Relaxed);
        match &*self.source() {
            Source::Screen(s) => s.latest.queue_arrivals(),
            Source::Apps(a) => a.queue_arrivals(),
        }
    }

    /// The pictures arrived since the last drain, oldest first.
    fn drain(&self) -> Vec<screen::Arrival> {
        let source = self.source();
        let mut list = match &*source {
            Source::Screen(s) => s.latest.drain(),
            Source::Apps(a) => a.drain(),
        };
        for a in &mut list {
            a.seq = self.ours(a.seq);
        }
        list
    }

    /// Wait up to `timeout` for a picture newer than `seen` (a screen; games
    /// and apps just wait).
    fn wait(&self, seen: u64, timeout: Duration) {
        match &*self.source() {
            Source::Screen(s) => s.latest.wait(seen.saturating_sub(self.0.base.load(Ordering::Relaxed)), timeout),
            Source::Apps(_) => thread::sleep(timeout.min(Duration::from_millis(1))),
        }
    }

    /// Why the pictures stopped, if they did (games and apps don't stop).
    fn ended(&self) -> Option<String> {
        match &*self.source() {
            Source::Screen(s) => s.latest.ended(),
            Source::Apps(_) => None,
        }
    }

    /// Whether none of the games and apps recorded is showing.
    fn waiting(&self) -> bool {
        matches!(&*self.source(), Source::Apps(a) if a.waiting())
    }

    /// The game or app whose window is recorded, while it shows.
    fn app(&self) -> Option<String> {
        match &*self.source() {
            Source::Apps(a) => a.app(),
            Source::Screen(_) => None,
        }
    }

    /// Let go (what's left of it goes with the last copy). A screen keeps
    /// running a little longer, in case it's taken up again right away
    /// (`screen::release`).
    fn release(self) {
        if let Source::Screen(s) = &*self.source() {
            screen::release(s.clone());
        }
    }
}

/// Everything a running capture owns. Torn down in `finish` in an order that
/// lets every encoder flush into the writer before it closes the file.
struct Session {
    feed: Option<Feed>,
    /// Shown while none of the games and apps is, for switching to them.
    away_screen: Option<Arc<crate::StillImage>>,
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
        // How it keeps up (the warnings by the recorder) counts from here.
        crate::capture_started();
        let feed = Feed::open(&s.video, s.away_screen.as_deref())?;
        let (width, height) = feed.recorded_size(s.size);
        let clock = Arc::new(Clock::default());

        let (titles, comment, has_rest) = track_layout(&s.sources);
        let tracks = titles.len();
        let layout = Layout {
            spec: Spec { width, height, fps: s.fps, audio_titles: titles, audio_bitrate: AUDIO_BITRATE, comment, mov, reorder: 0 },
        };
        let writer = match target {
            Target::Record(file) => Writer::record(file, layout, s.fps),
            Target::Replay(seconds) => Writer::replay(seconds, layout, s.fps),
        };
        let writer_tx = writer.tx.clone();

        let mut session = Self {
            feed: Some(feed.clone()),
            away_screen: s.away_screen.clone(),
            pacer: None,
            audio: None,
            mixer: None,
            encoders: Vec::new(),
            writer_tx,
            writer: Some(writer),
        };
        let started = (|| -> Result<()> {
            // One PCM channel + AAC encoder per track, in layout order: the
            // mix, each own track, the rest.
            let mut pcm_senders = Vec::new();
            for track in 0..tracks {
                let (tx, rx) = mpsc::channel::<Vec<f32>>();
                pcm_senders.push(tx);
                session.encoders.push(ffmpeg::spawn_aac(track, AUDIO_BITRATE, rx, session.writer_tx.clone())?);
            }
            let mut tracks = pcm_senders.into_iter();
            let mix_track = tracks.next();
            let audio = AudioCapture::start(&s.sources, live, clock.clone())?;
            let inputs = mix_inputs(&s.sources, &audio.feeds, live, |_| tracks.next());
            let rest_track = if has_rest { tracks.next() } else { None };
            session.audio = Some(audio);
            let stop = Arc::new(AtomicBool::new(false));
            let mixer = mixer::spawn_mixer(inputs, mix_track, rest_track, live.clone(), clock.clone(), host_now, MIX_LATENCY, stop.clone());
            session.mixer = Some((stop, mixer));

            let picture = Picture::new(feed.clone(), width, height, s.screen.clone(), s.webcam.as_ref());
            let encoder = VideoEncoder::start(width, height, s, session.writer_tx.clone(), picture.gpu.as_ref())?;
            // Frame blending is done on the graphics card.
            session.pacer = Some(if s.blend_samples > 1 && picture.gpu.is_some() {
                blend::spawn_blend_pacer(s.fps, s.blend_samples, crate::shutter::Shutter::new(s.blend_samples, s.blend_shape), picture, encoder, clock)
            } else {
                if s.blend_samples > 1 {
                    eprintln!("frame blending needs the graphics card to take the screen's pictures: recording without it");
                }
                spawn_pacer(s.fps, picture, encoder, clock)
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
        // The pacer flushes the video encoder on its way out.
        if let Some((stop, handle)) = self.pacer.take() {
            stop.store(true, Ordering::Relaxed);
            let _ = handle.join();
        }
        if let Some(feed) = self.feed.take() {
            feed.release();
        }
        if let Some(audio) = self.audio.take() {
            audio.stop();
        }
        // The mixer writes what's left and drops its senders; the AAC
        // encoders then flush and exit.
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

/// Makes the frame: the screen put where its placement says (a box in the
/// output size, as its framing says), the webcam over it or under it.
/// Redraws only when one of them changed.
struct Picture {
    feed: Feed,
    width: u32,
    height: u32,
    /// Made on the CPU here, or on the graphics card (`gpu`) when the
    /// screen's pictures arrive there.
    canvas: Canvas,
    gpu: Option<gpu_frame::GpuPicture>,
    screen: crate::SharedScreen,
    camera: Option<(camera::CameraLayer, SharedPlacement)>,
    /// What the canvas shows: the screen picture's number and where it went,
    /// the camera picture and where it was placed.
    shows: Option<(u64, crate::ScreenPlacement, Option<usize>, Option<Placement>)>,
    /// The webcam picture and where it was placed, as the card's layer has it.
    camera_shown: Option<(Option<usize>, Option<Placement>)>,
    /// The canvas as NV12, made when it changes.
    nv12: Vec<u8>,
    /// Counts the canvas's changes, so the preview is only sent new ones.
    version: u64,
}

impl Picture {
    fn new(feed: Feed, width: u32, height: u32, screen: crate::SharedScreen, webcam: Option<&crate::webcam::Webcam>) -> Self {
        let camera = webcam.map(|w| (camera::CameraLayer::new(w), w.placement.clone()));
        let placed = *screen.lock().unwrap();
        let gpu = screen::gpu_frames().map(|g| gpu_frame::GpuPicture::new(g, width, height, placed));
        let mut canvas = if gpu.is_some() { Canvas::new(0, 0) } else { Canvas::new(width, height) };
        canvas.clear();
        Self { feed, width, height, canvas, gpu, screen, camera, shows: None, camera_shown: None, nv12: Vec::new(), version: 0 }
    }

    /// Where the screen goes now (on the card too, for the frames made
    /// there next), and the webcam's picture and place: on the card, its
    /// layer is drawn again if either changed.
    fn layers(&mut self) -> (crate::ScreenPlacement, Option<Arc<CameraPicture>>, Option<Placement>) {
        let placed = *self.screen.lock().unwrap();
        if let Some(g) = &mut self.gpu {
            g.set_screen(placed);
        }
        let (cam, place) = self.camera_layer();
        (placed, cam, place)
    }

    /// On the card: draw the webcam's layer again if its picture or place
    /// changed. Returns the camera picture and place shown.
    fn camera_layer(&mut self) -> (Option<Arc<CameraPicture>>, Option<Placement>) {
        let (cam, place) = match &mut self.camera {
            Some((layer, place)) => {
                let place = *place.lock().unwrap();
                (layer.latest().filter(|_| !place.is_hidden()), Some(place))
            }
            None => (None, None),
        };
        let now = (cam.as_ref().map(|c| Arc::as_ptr(c) as usize), place);
        if let Some(g) = &mut self.gpu
            && self.camera_shown != Some(now)
        {
            g.set_camera(cam.as_deref().zip(place.as_ref()));
            self.camera_shown = Some(now);
        }
        (cam, place)
    }

    /// Bring the canvas up to date. Returns whether there's a picture yet.
    fn update(&mut self, nv12: bool) -> bool {
        // A source that reads the screen on request (X11) reads the next one now.
        self.feed.want();
        let Some((frame, seq)) = self.feed.get() else { return false };
        let (placed, cam, place) = self.layers();
        let now = (seq, placed, cam.as_ref().map(|c| Arc::as_ptr(c) as usize), place);
        if self.shows.as_ref() != Some(&now) {
            match &mut self.gpu {
                Some(g) => {
                    if !g.draw(&frame, seq) {
                        return false;
                    }
                }
                None => match (&cam, &place) {
                    // The screen in front: the webcam first, the screen over it.
                    (Some(cam), Some(place)) if placed.in_front => {
                        self.canvas.clear();
                        self.canvas.draw_camera(cam, place);
                        self.canvas.draw_screen(&frame, &placed, true);
                    }
                    (Some(cam), Some(place)) => {
                        self.canvas.draw_screen(&frame, &placed, false);
                        self.canvas.draw_camera(cam, place);
                    }
                    _ => self.canvas.draw_screen(&frame, &placed, false),
                },
            }
            self.shows = Some(now);
            self.nv12.clear();
            self.version += 1;
        }
        if nv12 && self.nv12.is_empty() {
            match &self.gpu {
                Some(g) => g.nv12(&mut self.nv12),
                None => self.canvas.to_nv12(&mut self.nv12),
            }
        }
        true
    }
}

/// The largest the preview is made: it's shown in a box a third of the
/// window tall, so more is only more to convert and upload every frame.
const PREVIEW_MAX: (u32, u32) = (1280, 720);

/// What's been sent to the preview.
struct Previewed {
    generation: u64,
    /// The canvas version last sent.
    version: Option<u64>,
    /// The last frame sent, whose memory the next one reuses once the UI is
    /// done with it.
    spare: Option<Arc<crate::preview::PreviewFrame>>,
}

impl Previewed {
    /// For the producer of this generation (`crate::preview::new_producer`).
    fn new(generation: u64) -> Self {
        Self { generation, version: None, spare: None }
    }
}

/// Hand the preview the picture, if it's wanted and has changed since the
/// last one (a still screen sends nothing new: no work here, and none for
/// the UI to upload).
fn submit_preview(picture: &mut Picture, sent: &mut Previewed) {
    if sent.version == Some(picture.version) || !crate::preview::wants_frame() {
        return;
    }
    sent.version = Some(picture.version);
    let mut rgba = sent.spare.take().and_then(|f| Arc::try_unwrap(f).ok()).map(|f| f.rgba).unwrap_or_default();
    let (w, h) = match &mut picture.gpu {
        Some(g) => g.preview(PREVIEW_MAX, &mut rgba),
        None => picture.canvas.to_preview(PREVIEW_MAX, &mut rgba),
    };
    let frame = crate::preview::PreviewFrame {
        width: w,
        height: h,
        recorded: (picture.width, picture.height),
        rgba,
        seq: 0,
        waiting: picture.feed.waiting(),
        app: picture.feed.app(),
    };
    sent.spare = Some(crate::preview::publish_frame(sent.generation, frame));
}

/// A frame handed to the video encoder: counted for the UI (to tell what
/// share of the frames the [`skipped`] ones are).
fn encoded() {
    crate::count_encoded();
}

/// The encoder can't keep up (its queue is full): `n` frames are skipped,
/// and the file shows the one before them longer. Counted for the UI's
/// warning, and logged each time the total passes a power of two.
fn skipped(dropped: &mut u64, n: u64) {
    let before = *dropped;
    *dropped += n;
    crate::count_skipped(n);
    if before.checked_ilog2() != dropped.checked_ilog2() {
        eprintln!("video encoder is behind: {dropped} frames skipped");
    }
}

/// The recording's video encoder: NVIDIA's, driven directly (`nvenc`); else
/// the card's through Vulkan Video (`vkenc`: AMD's, Intel's), where the
/// frame is made on the card; else ffmpeg's (`ffmpeg`: VA-API, x264 or
/// OpenH264).
pub(super) enum VideoEncoder {
    Nvenc(nvenc::Nvenc),
    Vulkan(vkenc::VkEncoder),
    Ffmpeg(ffmpeg::H264),
}

impl VideoEncoder {
    fn start(width: u32, height: u32, s: &EncodeSettings, out: Sender<writer::Command>, picture: Option<&gpu_frame::GpuPicture>) -> Result<Self> {
        // HASTECLIP_ENCODER=nvenc, vulkan or ffmpeg: only that one (or
        // ffmpeg's), even where another would start, to compare.
        let only = std::env::var("HASTECLIP_ENCODER").ok();
        let wanted = |name: &str| only.as_deref().is_none_or(|o| o == name);
        if s.use_hardware && wanted("nvenc") {
            match nvenc::Nvenc::start(width, height, s.fps, s.video_bitrate_kbps, out.clone(), picture.map(|p| p as &dyn gpu_frame::Nv12Source), false) {
                Ok(Some(e)) => return Ok(Self::Nvenc(e)),
                Ok(None) => {}
                Err(e) => eprintln!("NVIDIA's video encoder didn't start: {e:#}"),
            }
        }
        if let (true, true, Some(p)) = (s.use_hardware, wanted("vulkan"), picture) {
            match vkenc::VkEncoder::start(width, height, s.fps, s.video_bitrate_kbps, out.clone(), p, false) {
                Ok(Some(e)) => return Ok(Self::Vulkan(e)),
                Ok(None) => {}
                Err(e) => eprintln!("the card's video encoder (Vulkan Video) didn't start: {e:#}"),
            }
        }
        Ok(Self::Ffmpeg(ffmpeg::H264::start(width, height, s, out)?))
    }

    /// Whether it takes the picture as NV12 in our memory (`Picture::nv12`),
    /// rather than from the graphics card.
    fn wants_cpu_nv12(&self, picture: &Picture) -> bool {
        match self {
            Self::Nvenc(_) => picture.gpu.is_none(),
            Self::Vulkan(_) => false,
            Self::Ffmpeg(_) => true,
        }
    }

    /// Encode the picture as frame `n`. False if it's skipped: the encoder
    /// is behind.
    fn encode(&self, picture: &Picture, n: i64) -> bool {
        match self {
            Self::Ffmpeg(f) => {
                let mut frame = f.buffer();
                frame.clear();
                frame.extend_from_slice(&picture.nv12);
                f.encode(frame, n)
            }
            Self::Nvenc(e) => match &picture.gpu {
                Some(g) => e.encode_gpu(g, picture.version, n),
                None => e.encode_cpu(&picture.nv12, picture.version, n),
            },
            Self::Vulkan(e) => picture.gpu.as_ref().is_some_and(|g| e.encode_gpu(g, picture.version, n)),
        }
    }

    /// Encode the frame `g` has made (blended: `blend`) as frame `n`, its
    /// picture number `version`.
    fn encode_made(&self, g: &gpu_frame::GpuPicture, version: u64, n: i64) -> bool {
        match self {
            Self::Ffmpeg(f) => {
                let mut frame = f.buffer();
                g.nv12(&mut frame);
                f.encode(frame, n)
            }
            Self::Nvenc(e) => e.encode_gpu(g, version, n),
            Self::Vulkan(e) => e.encode_gpu(g, version, n),
        }
    }

    /// Encode what's queued and wait for the last frame.
    fn finish(self) {
        match self {
            Self::Ffmpeg(f) => f.finish(),
            Self::Nvenc(e) => e.finish(),
            Self::Vulkan(e) => e.finish(),
        }
    }
}

/// Encode the newest picture every 1/fps on the host clock. The screen only
/// sends pictures when something changes; repeating the last one keeps the
/// output constant frame rate. Frame N is stamped N/fps seconds after t0, the
/// same clock the audio uses.
fn spawn_pacer(fps: u32, mut picture: Picture, encoder: VideoEncoder, clock: Arc<Clock>) -> (Arc<AtomicBool>, JoinHandle<()>) {
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let handle = thread::Builder::new()
        .name("pacer".into())
        .spawn(move || {
            let mut sent = Previewed::new(crate::preview::new_producer());
            let fps_f = fps as f64;
            let t0 = host_now();
            clock.set(t0);
            let mut next: i64 = 0;
            let mut dropped = 0u64;
            let mut ended = false;
            while !stop2.load(Ordering::Relaxed) {
                let due = ((host_now() - t0) * fps_f) as i64;
                // After a stall, catch up gradually rather than in one huge burst.
                let mut burst = 0;
                while next <= due && burst < fps * 2 {
                    let cpu_nv12 = encoder.wants_cpu_nv12(&picture);
                    if picture.update(cpu_nv12) {
                        // The encoder can't keep up: skip this frame (the file
                        // shows the previous one a little longer).
                        if encoder.encode(&picture, next) {
                            encoded();
                        } else {
                            skipped(&mut dropped, 1);
                        }
                    }
                    next += 1;
                    burst += 1;
                }
                if !ended {
                    if let Some(why) = picture.feed.ended() {
                        // Sharing was stopped from the desktop: the file keeps the last picture.
                        eprintln!("{why}");
                        ended = true;
                    }
                }
                // The Sources page is showing the preview: hand it this picture too.
                submit_preview(&mut picture, &mut sent);
                let wait = t0 + next as f64 / fps_f - host_now();
                if wait > 0.0 {
                    thread::sleep(Duration::from_secs_f64(wait));
                }
            }
            encoder.finish();
            crate::preview::clear(sent.generation);
        })
        .expect("spawning the pacer thread");
    (stop, handle)
}

/// A capture made only for the preview (`crate::preview::VideoPreview`). Runs
/// entirely on its own thread: starting doesn't wait for the capture to open
/// (or for the desktop's "which screen?" dialog), and dropping doesn't wait
/// for it to close.
pub(crate) struct PreviewCapture {
    stop: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
    /// Its pictures' source once open, switched in place (`update`).
    feed: Arc<Mutex<Option<Feed>>>,
    away_screen: Option<Arc<crate::StillImage>>,
}

impl PreviewCapture {
    pub(crate) fn start(source: VideoSource, size: crate::VideoSize, fps: u32, away_screen: Option<Arc<crate::StillImage>>, webcam: Option<crate::webcam::Webcam>, screen: crate::SharedScreen) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let error = Arc::new(Mutex::new(None));
        let opened: Arc<Mutex<Option<Feed>>> = Default::default();
        let (stop2, error2, opened2, away2) = (stop.clone(), error.clone(), opened.clone(), away_screen.clone());
        let generation = crate::preview::new_producer();
        PREVIEWS.fetch_add(1, Ordering::AcqRel);
        thread::spawn(move || {
            // Counted down however the thread ends.
            struct Running;
            impl Drop for Running {
                fn drop(&mut self) {
                    PREVIEWS.fetch_sub(1, Ordering::AcqRel);
                }
            }
            let _running = Running;
            let feed = match Feed::open(&source, away2.as_deref()) {
                Ok(f) => f,
                Err(e) => {
                    *error2.lock().unwrap() = Some(format!("{e:#}"));
                    crate::preview::clear(generation);
                    return;
                }
            };
            *opened2.lock().unwrap() = Some(feed.clone());
            let (width, height) = feed.recorded_size(size);
            let mut picture = Picture::new(feed.clone(), width, height, screen, webcam.as_ref());
            let mut sent = Previewed::new(generation);
            // Paced like the recording would be.
            let every = 1.0 / fps.max(1) as f64;
            let t0 = host_now();
            let mut n = 0u64;
            while !stop2.load(Ordering::Relaxed) {
                if let Some(why) = picture.feed.ended() {
                    *error2.lock().unwrap() = Some(why);
                    break;
                }
                if crate::preview::wants_frame() && picture.update(false) {
                    submit_preview(&mut picture, &mut sent);
                }
                n += 1;
                let wait = t0 + n as f64 * every - host_now();
                if wait > 0.0 {
                    thread::sleep(Duration::from_secs_f64(wait));
                } else if wait < -0.5 {
                    n = ((host_now() - t0) / every) as u64; // fell far behind: skip ahead
                }
            }
            opened2.lock().unwrap().take();
            drop(picture);
            feed.release();
            crate::preview::clear(sent.generation);
        });
        Self { stop, error, feed: opened, away_screen }
    }

    /// Show `source` instead, without starting over (as a running capture
    /// switches). False if it isn't open yet, or couldn't.
    pub(crate) fn update(&self, source: &VideoSource) -> bool {
        let Some(feed) = self.feed.lock().unwrap().clone() else { return false };
        feed.switch(source, self.away_screen.as_deref()).inspect_err(|e| eprintln!("preview: can't switch: {e:#}")).is_ok()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Recording games and apps follows the picked program's window once
    /// it's in focus, at the window's own size, and says which it is.
    #[test]
    #[ignore = "opens a window and captures it: cargo test -p capture records_an_apps_window -- --ignored"]
    fn records_an_apps_window() {
        let listed = list_windowed_apps();
        eprintln!("apps: {:?}", listed.iter().map(|d| format!("{} = {}", d.id, d.name)).collect::<Vec<_>>());
        // Konsole: one program that isn't otherwise running, so closing it leaves none.
        let mut term = std::process::Command::new("konsole").args(["--separate", "-e", "sh", "-c", "while true; do date +%T.%N; sleep 0.02; done"]).spawn().expect("konsole");
        thread::sleep(Duration::from_millis(1500));
        let video = VideoSource::Apps { ids: vec!["konsole".into()], away_when_unfocused: true, hook: crate::GameHook { auto: false, allowed: Vec::new() } };
        let feed = Feed::open(&video, None).unwrap();
        let t = Instant::now();
        while feed.app().is_none() && t.elapsed() < Duration::from_secs(3) {
            thread::sleep(Duration::from_millis(20));
        }
        let app = feed.app();
        let (frame, seq) = feed.get().unwrap();
        thread::sleep(Duration::from_millis(500));
        let (_, seq2) = feed.get().unwrap();
        let lit = frame.data.chunks_exact(4).filter(|p| p[0] > 16 || p[1] > 16 || p[2] > 16).count();
        eprintln!("app {app:?} after {} ms: {}x{}, {lit} lit pixels, pictures {seq:#x} → {seq2:#x}", t.elapsed().as_millis(), frame.width, frame.height);
        let _ = term.kill();
        thread::sleep(Duration::from_millis(500));
        eprintln!("after closing: app {:?}, waiting {}", feed.app(), feed.waiting());
        assert_eq!(app.as_deref(), Some("konsole"));
        assert!(lit > 0, "all black");
        assert!(seq2 > seq, "no new pictures");
        assert!(feed.waiting());
    }

    /// Recording the main screen at 1080p60 for 5 seconds: a file with every
    /// frame, the CPU it took (HASTECLIP_NO_GPU_CAPTURE=1 for the CPU's way),
    /// and a screenshot meanwhile.
    #[test]
    #[ignore = "records the screen: cargo test -p capture records_the_screen -- --ignored"]
    fn records_the_screen() {
        let screen = list_screens().into_iter().next().expect("a screen");
        let video = VideoSource::Screen { id: screen.id.clone(), hook: crate::GameHook { auto: false, allowed: Vec::new() }, method: Default::default() };
        let dir = std::env::temp_dir().join("hc-record-test");
        let _ = std::fs::remove_dir_all(&dir);
        let settings = EncodeSettings {
            output_dir: dir.clone(),
            container_ext: "mp4".into(),
            fps: 60,
            // HASTECLIP_BLEND=<looks a frame>, HASTECLIP_SOFT=1 for the soft shape.
            blend_samples: std::env::var("HASTECLIP_BLEND").ok().and_then(|s| s.parse().ok()).unwrap_or(1),
            blend_shape: if std::env::var_os("HASTECLIP_SOFT").is_some() { crate::BlendShape::Soft } else { crate::BlendShape::Even },
            video_bitrate_kbps: 8000,
            size: crate::VideoSize { short_side: Some(1080), ..Default::default() },
            use_hardware: true,
            replay_seconds: 5,
            video: video.clone(),
            away_screen: None,
            // HASTECLIP_WEBCAM=<device> draws a webcam in the corner.
            webcam: std::env::var("HASTECLIP_WEBCAM").ok().map(|device| crate::webcam::Webcam {
                device,
                format: None,
                placement: Arc::new(Mutex::new(crate::webcam::Placement::default_for(16.0 / 9.0, 16.0 / 9.0))),
            }),
            screen: Arc::new(Mutex::new(crate::ScreenPlacement::whole(crate::Framing::Fit))),
            sources: Vec::new(),
        };
        let cpu = || {
            let mut u: libc::rusage = unsafe { std::mem::zeroed() };
            // SAFETY: a struct to fill.
            unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
            u.ru_utime.tv_sec as f64 + u.ru_utime.tv_usec as f64 / 1e6 + u.ru_stime.tv_sec as f64 + u.ru_stime.tv_usec as f64 / 1e6
        };
        let mut rec = LinuxRecorder::new(Arc::new(LiveAudio::default()));
        rec.start(Mode::Record, &settings).unwrap();
        thread::sleep(Duration::from_secs(1));
        let (t, c) = (Instant::now(), cpu());
        thread::sleep(Duration::from_secs(4));
        let used = (cpu() - c) / t.elapsed().as_secs_f64();
        let shot = rec.screenshot(&video).unwrap().finish().unwrap();
        let file = rec.stop(None).unwrap().expect("a file");
        shutdown();
        let probe = std::process::Command::new("ffprobe")
            .args(["-v", "error", "-select_streams", "v:0", "-count_frames", "-show_entries", "stream=width,height,nb_read_frames:format=duration", "-of", "csv=p=0"])
            .arg(&file)
            .output()
            .unwrap();
        eprintln!("refresh: {:?} Hz (fastest {:?})", display_refresh(Some(&screen.id)), display_refresh(None));
        eprintln!("gpu: {}, {:.0}% of a core, screenshot {}x{}, file: {}", screen::gpu_frames().is_some(), used * 100.0, shot.width, shot.height, String::from_utf8_lossy(&probe.stdout).replace('\n', " "));
        if let Some(out) = std::env::var_os("HASTECLIP_FRAME") {
            let _ = std::process::Command::new("ffmpeg").args(["-v", "error", "-y", "-sseof", "-1", "-i"]).arg(&file).args(["-frames:v", "1"]).arg(out).status();
        }
        assert!(shot.bgra.chunks_exact(4).any(|p| p[0] > 16 || p[1] > 16 || p[2] > 16), "black screenshot");
    }

    /// An X11 window (XWayland's, as a Proton game's) is followed and read
    /// through XComposite: HASTECLIP_APPS_X11=1 on KDE.
    #[test]
    #[ignore = "opens an X11 window and captures it: HASTECLIP_APPS_X11=1 cargo test -p capture records_an_x11_window -- --ignored"]
    fn records_an_x11_window() {
        let mut cube = std::process::Command::new("vkcube").args(["--wsi", "xcb"]).spawn().expect("vkcube");
        thread::sleep(Duration::from_millis(1500));
        let listed = list_windowed_apps();
        eprintln!("apps: {:?}", listed.iter().map(|d| format!("{} = {}", d.id, d.name)).collect::<Vec<_>>());
        let video = VideoSource::Apps { ids: vec!["vkcube".into()], away_when_unfocused: true, hook: crate::GameHook { auto: false, allowed: Vec::new() } };
        let feed = Feed::open(&video, None).unwrap();
        let t = Instant::now();
        while feed.app().is_none() && t.elapsed() < Duration::from_secs(3) {
            thread::sleep(Duration::from_millis(20));
        }
        let app = feed.app();
        thread::sleep(Duration::from_millis(300));
        let (frame, seq) = feed.get().unwrap();
        for _ in 0..30 {
            feed.want();
            thread::sleep(Duration::from_millis(16));
        }
        let (_, seq2) = feed.get().unwrap();
        let lit = frame.data.chunks_exact(4).filter(|p| p[0] > 16 || p[1] > 16 || p[2] > 16).count();
        eprintln!("app {app:?}: {}x{}, {lit} lit pixels, pictures {seq:#x} → {seq2:#x}", frame.width, frame.height);
        if let Some(out) = std::env::var_os("HASTECLIP_SHOT") {
            let (w, h) = (frame.width, frame.height);
            let rgb: Vec<u8> = frame.data.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0]]).collect();
            std::fs::write(out, [format!("P6\n{w} {h}\n255\n").as_bytes(), &rgb].concat()).unwrap();
        }
        let _ = cube.kill();
        thread::sleep(Duration::from_millis(500));
        eprintln!("after closing: app {:?}, waiting {}", feed.app(), feed.waiting());
        assert_eq!(app.as_deref(), Some("vkcube"));
        assert!(lit > 0, "all black");
        assert!(seq2 > seq, "no new pictures");
    }

    /// A window on Sway and its kin is followed and captured with the ext
    /// protocols: run inside one (a nested Sway with `foot` open will do).
    #[test]
    #[ignore = "needs Sway: WAYLAND_DISPLAY=<sway's> XDG_CURRENT_DESKTOP=sway cargo test -p capture records_a_wlr_window -- --ignored"]
    fn records_a_wlr_window() {
        let listed = list_windowed_apps();
        eprintln!("apps: {:?}", listed.iter().map(|d| format!("{} = {}", d.id, d.name)).collect::<Vec<_>>());
        let video = VideoSource::Apps { ids: vec!["foot".into()], away_when_unfocused: true, hook: crate::GameHook { auto: false, allowed: Vec::new() } };
        let feed = Feed::open(&video, None).unwrap();
        let t = Instant::now();
        while feed.app().is_none() && t.elapsed() < Duration::from_secs(3) {
            thread::sleep(Duration::from_millis(20));
        }
        let app = feed.app();
        thread::sleep(Duration::from_millis(300));
        let (frame, seq) = feed.get().unwrap();
        thread::sleep(Duration::from_millis(500));
        let (_, seq2) = feed.get().unwrap();
        let lit = frame.data.chunks_exact(4).filter(|p| p[0] > 16 || p[1] > 16 || p[2] > 16).count();
        eprintln!("app {app:?}: {}x{}, {lit} lit pixels, pictures {seq:#x} → {seq2:#x}", frame.width, frame.height);
        if let Some(out) = std::env::var_os("HASTECLIP_SHOT") {
            let (w, h) = (frame.width, frame.height);
            let rgb: Vec<u8> = frame.data.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0]]).collect();
            std::fs::write(out, [format!("P6\n{w} {h}\n255\n").as_bytes(), &rgb].concat()).unwrap();
        }
        assert_eq!(app.as_deref(), Some("foot"));
        assert!(lit > 0, "all black");
        assert!(seq2 > seq, "no new pictures");
    }

    /// A screenshot is all of the screen at its own size, opaque.
    #[test]
    #[ignore = "captures the screen: cargo test -p capture screenshot_of_the_screen -- --ignored"]
    fn screenshot_of_the_screen() {
        let screen = list_screens().into_iter().next().expect("a screen");
        let video = VideoSource::Screen { id: screen.id.clone(), hook: crate::GameHook { auto: false, allowed: Vec::new() }, method: Default::default() };
        let mut rec = LinuxRecorder::new(Arc::new(LiveAudio::default()));
        let t = Instant::now();
        let img = rec.screenshot(&video).unwrap().finish().unwrap();
        eprintln!("{} {}×{} in {} ms", screen.name, img.width, img.height, t.elapsed().as_millis());
        assert_eq!(img.bgra.len(), (img.width * img.height * 4) as usize);
        assert!(img.bgra.chunks_exact(4).all(|p| p[3] == 255));
        assert!(img.bgra.chunks_exact(4).any(|p| p[0] > 16 || p[1] > 16 || p[2] > 16), "all black");
        if let Some(out) = std::env::var_os("HASTECLIP_SHOT") {
            let (w, h) = (img.width, img.height);
            let rgb: Vec<u8> = img.bgra.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0]]).collect();
            let header = format!("P6\n{w} {h}\n255\n");
            std::fs::write(out, [header.as_bytes(), &rgb].concat()).unwrap();
        }
    }
}
