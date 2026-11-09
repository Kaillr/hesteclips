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
mod dmabuf;
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
mod layers;
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
pub(crate) use layers::{app_window_size, video_size};
pub use kwin::helper as kde_stream_helper;

use crate::mixer::{self, Clock, LiveAudio, MixChange, MixInput};
use crate::mp4file::Layout;
use crate::mp4mux::Spec;
use crate::outputs::{Capture, Outputs};
use crate::shutter::Shutter;
use crate::sources::{AudioCapture, mix_inputs, track_layout};
use crate::stream::StreamSettings;
use crate::writer::{self, Writer};
use crate::{AUDIO_BITRATE, BlendShape, Device, EncodeSettings, Mode, Recorder, Running, VideoSource};
use image::Canvas;

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

/// Open camera `device`'s own picture, shrunk.
pub(crate) fn camera_picture(device: &str, max_width: u32, newer_than: u64) -> Option<crate::webcam::CameraPreview> {
    camera::picture(device, max_width, newer_than)
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
    outputs: Outputs<Session>,
    live: Arc<LiveAudio>,
}

impl LinuxRecorder {
    /// `live` receives the meters and supplies each source's volume.
    pub fn new(live: Arc<LiveAudio>) -> Self {
        Self { outputs: Outputs::new(), live }
    }
}

impl Recorder for LinuxRecorder {
    fn start(&mut self, mode: Mode, settings: &EncodeSettings) -> Result<()> {
        let live = self.live.clone();
        self.outputs.start(mode, settings, |s, mov| Session::start(s, &live, mov))
    }

    fn save_clip(&mut self, dir: &Path) -> Result<crate::PendingClip> {
        self.outputs.save_clip(dir)
    }

    fn stop(&mut self, mode: Mode, dir: Option<&Path>) -> Result<Option<PathBuf>> {
        self.outputs.stop(mode, dir)
    }

    fn stop_all(&mut self) {
        self.outputs.stop_all();
    }

    fn running(&self) -> Running {
        self.outputs.running()
    }

    fn set_replay_seconds(&mut self, seconds: u32) {
        self.outputs.set_replay_seconds(seconds);
    }

    fn screenshot(&mut self, video: &VideoSource) -> Result<crate::PendingShot> {
        if let Some(feed) = self.outputs.session.as_ref().and_then(|s| s.feed.as_ref()) {
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
        self.outputs.session.as_mut().and_then(|s| s.audio.as_mut()).is_some_and(|a| a.set_mic(id, device))
    }

    fn set_blend(&mut self, looks: u32, shape: BlendShape) -> bool {
        self.outputs.session.as_mut().is_some_and(|s| s.set_blend(looks, shape))
    }

    fn set_sources(&mut self, sources: &[crate::sources::AudioSource]) -> bool {
        self.outputs.session.as_mut().is_some_and(|s| s.set_sources(sources, &self.live))
    }

    /// Record something else without restarting: another list of games and
    /// apps, or the whole screen instead of them (or the other way round, or
    /// another display), into the same picture. False if it couldn't.
    fn update_video(&mut self, video: &VideoSource) -> bool {
        let Some(session) = self.outputs.session.as_ref() else { return false };
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
        self.outputs.stop_all();
    }
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
    /// Sources added to the mix and taken out of it while it runs.
    mix_changes: Option<Sender<MixChange>>,
    /// AAC encoder threads, one per track; they end when the mixer drops its senders.
    encoders: Vec<JoinHandle<()>>,
    writer_tx: Sender<writer::Command>,
    writer: Option<Writer>,
    /// Frames a second, the frame's size, and whether the graphics card's
    /// encoder is wanted: what the stream's own encoder is set up from.
    fps: u32,
    size: (u32, u32),
    hardware: bool,
    /// The frame is made on the graphics card (so it can blend, and the
    /// stream's pictures are made their size there).
    on_card: bool,
    /// What's changed in the running pacer: frame blending, and the stream's
    /// pictures.
    ctl: PacerCtl,
}

impl Capture for Session {
    fn writer(&self) -> &Sender<writer::Command> {
        &self.writer_tx
    }

    /// The stream's own pictures, one for each rendition
    /// (`stream::renditions`: as many as the places streamed to want
    /// different ones): a frame for every `every` the pacer makes, at the
    /// rendition's size (made so on the graphics card, or by ffmpeg from the
    /// frame made on the CPU) and rate, through an ffmpeg of its own.
    fn stream_video(&mut self, stream: Option<&StreamSettings>) -> Result<()> {
        let Some(st) = stream else {
            *self.ctl.stream.lock().unwrap() = Some(Vec::new());
            return Ok(());
        };
        if self.pacer.is_none() {
            bail!("the capture can't stream");
        }
        let (renditions, _) = crate::stream::renditions(st, self.fps);
        let mut outs = Vec::with_capacity(renditions.len());
        for (i, r) in renditions.iter().enumerate() {
            let every = (self.fps / r.fps.max(1)).max(1) as i64;
            let size = crate::output_size(self.size.0, self.size.1, r.short_side);
            let input = if self.on_card { size } else { self.size };
            let encoder = ffmpeg::H264::stream(input, size, r.fps, r.kbps, self.hardware, i, self.writer_tx.clone())?;
            outs.push(StreamOut { every, size, scaled: None, sum: None, summing: None, encoder, dropped: 0 });
        }
        *self.ctl.stream.lock().unwrap() = Some(outs);
        Ok(())
    }

    fn finish(self) -> Result<()> {
        self.end()
    }
}

impl Session {
    fn start(s: &EncodeSettings, live: &Arc<LiveAudio>, mov: bool) -> Result<Self> {
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
        let writer = Writer::new(layout, s.fps);
        let writer_tx = writer.tx.clone();

        let mut session = Self {
            feed: Some(feed.clone()),
            away_screen: s.away_screen.clone(),
            pacer: None,
            audio: None,
            mixer: None,
            mix_changes: None,
            encoders: Vec::new(),
            writer_tx,
            writer: Some(writer),
            fps: s.fps,
            size: (width, height),
            hardware: s.use_hardware,
            on_card: false,
            ctl: PacerCtl::default(),
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
            let (mix_changes, changes) = mpsc::channel();
            session.mix_changes = Some(mix_changes);
            let mixer = mixer::spawn_changing_mixer(inputs, changes, mix_track, rest_track, live.clone(), clock.clone(), host_now, MIX_LATENCY, stop.clone());
            session.mixer = Some((stop, mixer));

            let picture = Picture::new(feed.clone(), width, height, s.screen.clone(), s.layers.clone());
            let encoder = VideoEncoder::start(width, height, s, session.writer_tx.clone(), picture.gpu.as_ref())?;
            session.on_card = picture.gpu.is_some();
            let ctl = session.ctl.clone();
            session.pacer = Some(spawn_pacer(s.fps, s.blend_samples, s.blend_shape, picture, encoder, clock, ctl));
            Ok(())
        })();
        if let Err(e) = started {
            let _ = session.end();
            return Err(e);
        }
        Ok(session)
    }

    /// Change frame blending while it runs: the pacer takes it up at its
    /// next frame (blending starts, changes or stops there). The screen's
    /// pictures come as the compositor shows them, whatever the rate: nothing
    /// starts again. False if it can't (blending needs the frame made on the
    /// graphics card).
    fn set_blend(&mut self, looks: u32, shape: BlendShape) -> bool {
        if self.pacer.is_none() || (looks > 1 && !self.on_card) {
            return false;
        }
        *self.ctl.blend.lock().unwrap() = Some((looks.max(1), shape));
        true
    }

    /// Capture these audio sources from now on, without restarting (see
    /// [`Recorder::set_sources`]): the capture starts and stops them, and
    /// the mixer takes them in and out.
    fn set_sources(&mut self, sources: &[crate::sources::AudioSource], live: &LiveAudio) -> bool {
        let (Some(audio), Some(changes)) = (self.audio.as_mut(), &self.mix_changes) else { return false };
        let (added, gone) = audio.set_sources(sources, live);
        for source in gone {
            eprintln!("audio source \"{}\": taken out while capturing", source.name);
            let _ = changes.send(MixChange::Remove(source.id));
        }
        for (source, feed) in added {
            eprintln!("audio source \"{}\": added while capturing, into the mix", source.name);
            let input = MixInput { channel: live.channel(&source.id), id: source.id, feed, in_mix: source.in_mix, track: None };
            let _ = changes.send(MixChange::Add(input));
        }
        true
    }

    /// Stop capturing and finish what the writer was doing (a recording's
    /// file; the ring's let go).
    fn end(mut self) -> Result<()> {
        // The pacer flushes the video encoder on its way out.
        if let Some((stop, handle)) = self.pacer.take() {
            stop.store(true, Ordering::Relaxed);
            let _ = handle.join();
        }
        // A stream the pacer stopped before it took it up: finished here
        // (its ffmpegs would hold the writer open).
        let pending = self.ctl.stream.lock().unwrap().take();
        for s in pending.into_iter().flatten() {
            s.finish();
        }
        if let Some(feed) = self.feed.take() {
            feed.release();
        }
        if let Some(audio) = self.audio.take() {
            audio.stop();
        }
        // The mixer writes what's left and drops its senders; the AAC
        // encoders then flush and exit.
        self.mix_changes = None;
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
/// output size, as its framing says), the other layers (webcams, displays,
/// windows, pictures, videos: `layers`) under it and over it, in their
/// order. Redraws only when one of them changed.
struct Picture {
    feed: Feed,
    width: u32,
    height: u32,
    /// Made on the CPU here, or on the graphics card (`gpu`) when the
    /// screen's pictures arrive there.
    canvas: Canvas,
    gpu: Option<gpu_frame::GpuPicture>,
    screen: crate::SharedScreen,
    /// The other layers' pictures, and what of them shows now.
    feeds: layers::Feeds,
    stack: layers::Stack,
    /// What the canvas shows: the screen picture's number, where it went,
    /// and the other layers' (`Stack::key`).
    shows: Option<(u64, crate::ScreenPlacement, layers::StackKey)>,
    /// The canvas as NV12, made when it changes.
    nv12: Vec<u8>,
    /// Counts the canvas's changes, so the preview is only sent new ones.
    version: u64,
}

impl Picture {
    fn new(feed: Feed, width: u32, height: u32, screen: crate::SharedScreen, layers: crate::SharedLayers) -> Self {
        let placed = *screen.lock().unwrap();
        let gpu = screen::gpu_frames().map(|g| gpu_frame::GpuPicture::new(g, width, height, placed));
        let mut canvas = if gpu.is_some() { Canvas::new(0, 0) } else { Canvas::new(width, height) };
        canvas.clear();
        Self { feed, width, height, canvas, gpu, screen, feeds: layers::Feeds::new(layers), stack: layers::Stack::main_only(), shows: None, nv12: Vec::new(), version: 0 }
    }

    /// Where the screen goes now (hidden when the list hasn't it: a scene
    /// without the screen), and what of the other layers shows: on the
    /// card, for the frames made there next (a set of layers that changed
    /// is drawn again).
    fn layers(&mut self) -> crate::ScreenPlacement {
        self.stack = self.feeds.stack();
        let mut placed = *self.screen.lock().unwrap();
        placed.hidden |= !self.stack.main;
        if let Some(g) = &mut self.gpu {
            g.set_screen(placed);
            g.set_layers(&self.stack.under, &self.stack.over);
        }
        placed
    }

    /// Bring the canvas up to date. Returns whether there's a picture yet.
    fn update(&mut self, nv12: bool) -> bool {
        // A source that reads the screen on request (X11) reads the next one now.
        self.feed.want();
        let Some((frame, seq)) = self.feed.get() else { return false };
        let placed = self.layers();
        let now = (seq, placed, self.stack.key());
        if self.shows.as_ref() != Some(&now) {
            match &mut self.gpu {
                Some(g) => {
                    if !g.draw(&frame, seq) {
                        return false;
                    }
                }
                None => {
                    // The layers under the screen, the screen around them
                    // (on black where there's none), then those over it.
                    let under = !self.stack.under.is_empty();
                    if under {
                        self.canvas.clear();
                        for l in &self.stack.under {
                            self.canvas.draw_layer(l);
                        }
                    }
                    self.canvas.draw_screen(&frame, &placed, under);
                    for l in &self.stack.over {
                        self.canvas.draw_layer(l);
                    }
                }
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

/// The largest the preview is made: full size up to 1440p, as Windows', so
/// things can be lined up on it to the pixel (shrunk, a 1080p picture was
/// drawn soft); a 4K one is shrunk to 1440p, as every frame is read back,
/// copied and uploaded, 60 times a second.
const PREVIEW_MAX: (u32, u32) = (2560, 2560);

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

/// What the session changes in the running pacer: frame blending, and the
/// stream's pictures (`Session::set_blend`, `Session::stream_video`). Each
/// is taken up at the pacer's next turn.
#[derive(Clone, Default)]
struct PacerCtl {
    blend: Arc<Mutex<Option<(u32, BlendShape)>>>,
    /// The stream started (one per rendition, `stream::renditions`), or
    /// stopped (none).
    stream: Arc<Mutex<Option<Vec<StreamOut>>>>,
}

/// One of the stream's pictures (a rendition, `stream::renditions`), in the
/// pacer: a frame for every `every` of the pacer's, made its size on the
/// graphics card (read back as NV12) or, the frame made on the CPU, by
/// ffmpeg, and encoded by an ffmpeg of its own.
struct StreamOut {
    every: i64,
    size: (u32, u32),
    /// Where the card makes it that size: made with its first frame.
    scaled: Option<gpu_frame::Scaled>,
    /// With frame blending on, `every` over one: each of its frames the
    /// average of all the pacer's in its time (`gpu_frame::FrameSum`, made
    /// with its first), and which of its frames is being made.
    sum: Option<gpu_frame::FrameSum>,
    summing: Option<i64>,
    encoder: ffmpeg::H264,
    /// Frames its encoder couldn't take.
    dropped: u64,
}

impl StreamOut {
    /// Whether the pacer's frame `k` is wanted (`blending`: frame blending's
    /// on): every one while it averages its own, else those at its own
    /// frames' times.
    fn wants(&self, k: i64, blending: bool) -> bool {
        (blending && self.every > 1) || k.rem_euclid(self.every) == 0
    }

    /// The pacer's frame `k`, as `picture` has it now (`blending`: frame
    /// blending's on, on the card): its frame from it once one's due. Made
    /// on the CPU, its NV12 must be made (`Picture::update`).
    fn frame(&mut self, picture: &Picture, k: i64, blending: bool) {
        if !self.wants(k, blending) {
            return;
        }
        let n = k.div_euclid(self.every);
        let mut averaged = false;
        match &picture.gpu {
            Some(g) if blending && self.every > 1 => {
                let sum = self.sum.get_or_insert_with(|| g.frame_sum());
                // A frame of a later one: the one being made never got its
                // last (left out), and is let go of.
                if self.summing.is_some_and(|s| s != n) {
                    g.clear_sum(sum);
                }
                self.summing = Some(n);
                g.add_to_sum(sum);
                if k.rem_euclid(self.every) != self.every - 1 {
                    return;
                }
                self.summing = None;
                if !g.resolve_sum(sum) {
                    return;
                }
                averaged = true;
            }
            _ => {
                // Blending stopped partway through one: let go of.
                if self.summing.take().is_some()
                    && let (Some(g), Some(sum)) = (&picture.gpu, &mut self.sum)
                {
                    g.clear_sum(sum);
                }
                if k.rem_euclid(self.every) != 0 {
                    return;
                }
            }
        }
        let mut nv12 = self.encoder.buffer();
        let whole = match &picture.gpu {
            Some(g) => {
                let (w, h) = self.size;
                let scaled = self.scaled.get_or_insert_with(|| g.scaled(w, h));
                g.scaled_nv12(scaled, if averaged { self.sum.as_ref() } else { None }, &mut nv12);
                gpu_frame::nv12_len(w, h)
            }
            None => {
                nv12.clear();
                nv12.extend_from_slice(&picture.nv12);
                gpu_frame::nv12_len(picture.width, picture.height)
            }
        };
        // Only whole frames go in: ffmpeg reads them back to back.
        if nv12.len() != whole || !self.encoder.encode(nv12, n) {
            self.dropped += 1;
        }
    }

    /// Encode what's handed in and wait for the last frame.
    fn finish(self) {
        if self.dropped > 0 {
            eprintln!("stream: {} frames left out, its encoder behind", self.dropped);
        }
        self.encoder.finish();
    }
}

/// What the pacer makes frames of and with, however it times them.
struct Pace {
    picture: Picture,
    encoder: VideoEncoder,
    /// The stream's own pictures, while it streams (one per rendition).
    stream: Vec<StreamOut>,
    /// Frame N is at `t0 + N / fps`.
    t0: f64,
    fps: u32,
    /// Frames the encoder couldn't take.
    skipped: u64,
    sent: Previewed,
    /// The pictures stopped coming, and it was said.
    ended: bool,
}

impl Pace {
    /// Frame `k` was made (`picture` has it; `blending`: frame blending's
    /// on): the stream's pictures', where they want it.
    fn stream_frame(&mut self, k: i64, blending: bool) {
        for s in &mut self.stream {
            s.frame(&self.picture, k, blending);
        }
    }

    /// Sharing was stopped from the desktop: said once (the file keeps the
    /// last picture).
    fn check_ended(&mut self) {
        if !self.ended
            && let Some(why) = self.picture.feed.ended()
        {
            eprintln!("{why}");
            self.ended = true;
        }
    }

    /// The stream started or stopped, if the session says so. A stopped
    /// stream's encoders are finished on a thread of their own (it waits
    /// for them).
    fn take_stream(&mut self, ctl: &PacerCtl) {
        let Some(change) = ctl.stream.lock().unwrap().take() else { return };
        let old = std::mem::replace(&mut self.stream, change);
        if old.is_empty() {
            return;
        }
        let _ = thread::Builder::new().name("stream-end".into()).spawn(move || old.into_iter().for_each(StreamOut::finish));
    }
}

/// How the pacer times its frames: each the newest picture at its time, or
/// blended from looks at the screen on the graphics card.
enum Timing {
    /// Frame `next` is the next to make, at its time.
    Plain { next: i64 },
    Blend(blend::Blending),
}

impl Timing {
    /// Blend `looks` a frame (one: none), weighed by `shape`, from the next
    /// frame on: blending starts, changes or stops there.
    fn change(&mut self, pace: &mut Pace, looks: u32, shape: BlendShape) {
        let mut looks = looks.max(1);
        if looks > 1 && pace.picture.gpu.is_none() {
            eprintln!("frame blending needs the graphics card to take the screen's pictures: recording without it");
            looks = 1;
        }
        let shutter = Shutter::new(looks, shape);
        let new = match self {
            Timing::Plain { next } if looks > 1 => {
                crate::eprintln_later!("frame blending: {looks} looks a frame");
                Some(Timing::Blend(blend::Blending::start(pace, looks, shutter, *next)))
            }
            Timing::Plain { .. } => None,
            Timing::Blend(b) if looks == 1 => {
                if let Some(g) = &mut pace.picture.gpu {
                    g.stop_blending();
                }
                // The frame has the last blended one in it: drawn again from
                // the picture that's up, even a still one.
                pace.picture.shows = None;
                crate::blending(pace.fps, 1);
                crate::eprintln_later!("frame blending: off from now on");
                Some(Timing::Plain { next: b.closed() + 1 })
            }
            Timing::Blend(b) => {
                b.change(pace, looks, shutter);
                None
            }
        };
        if let Some(t) = new {
            *self = t;
        }
    }
}

/// Make a frame every 1/fps on the host clock, for the encoder (and the
/// stream's own while it streams): each the newest picture at its time, or
/// with frame blending (on the graphics card) the average of `looks` looks
/// at the screen (`blend`). The screen only sends pictures when something
/// changes; repeating the last one keeps the output constant frame rate.
/// Frame N is stamped N/fps seconds after t0, the same clock the audio
/// uses. What `ctl` says changed (frame blending, the stream) is taken up
/// at the next frame.
fn spawn_pacer(fps: u32, looks: u32, shape: BlendShape, picture: Picture, encoder: VideoEncoder, clock: Arc<Clock>, ctl: PacerCtl) -> (Arc<AtomicBool>, JoinHandle<()>) {
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let handle = thread::Builder::new()
        .name("pacer".into())
        .spawn(move || {
            let t0 = host_now();
            clock.set(t0);
            let mut pace = Pace { picture, encoder, stream: Vec::new(), t0, fps, skipped: 0, sent: Previewed::new(crate::preview::new_producer()), ended: false };
            let mut timing = Timing::Plain { next: 0 };
            if looks > 1 {
                timing.change(&mut pace, looks, shape);
            }
            while !stop2.load(Ordering::Relaxed) {
                // Frame blending changed, the stream started or stopped.
                let change = ctl.blend.lock().unwrap().take();
                if let Some((looks, shape)) = change {
                    timing.change(&mut pace, looks, shape);
                }
                pace.take_stream(&ctl);
                match &mut timing {
                    Timing::Plain { next } => plain_step(&mut pace, next),
                    Timing::Blend(b) => b.step(&mut pace),
                }
            }
            for s in std::mem::take(&mut pace.stream) {
                s.finish();
            }
            pace.encoder.finish();
            crate::preview::clear(pace.sent.generation);
        })
        .expect("spawning the pacer thread");
    (stop, handle)
}

/// One turn of the pacer without blending: the frames due made (each the
/// newest picture), then a wait for the next one's time.
fn plain_step(pace: &mut Pace, next: &mut i64) {
    let fps_f = pace.fps as f64;
    let due = ((host_now() - pace.t0) * fps_f) as i64;
    // After a stall, catch up gradually rather than in one huge burst.
    let mut burst = 0;
    while *next <= due && burst < pace.fps * 2 {
        // The stream's frame from the CPU: its NV12 (ffmpeg makes it the stream's size).
        let stream_cpu = pace.picture.gpu.is_none() && pace.stream.iter().any(|s| s.wants(*next, false));
        let cpu_nv12 = pace.encoder.wants_cpu_nv12(&pace.picture) || stream_cpu;
        if pace.picture.update(cpu_nv12) {
            // The encoder can't keep up: skip this frame (the file
            // shows the previous one a little longer).
            if pace.encoder.encode(&pace.picture, *next) {
                encoded();
            } else {
                skipped(&mut pace.skipped, 1);
            }
            pace.stream_frame(*next, false);
        }
        *next += 1;
        burst += 1;
    }
    pace.check_ended();
    // The Sources page is showing the preview: hand it this picture too.
    submit_preview(&mut pace.picture, &mut pace.sent);
    // Pictures kept for blending (it was on) aren't wanted now.
    drop(pace.picture.feed.drain());
    let wait = pace.t0 + *next as f64 / fps_f - host_now();
    if wait > 0.0 {
        thread::sleep(Duration::from_secs_f64(wait));
    }
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
    pub(crate) fn start(source: VideoSource, size: crate::VideoSize, fps: u32, away_screen: Option<Arc<crate::StillImage>>, layers: crate::SharedLayers, screen: crate::SharedScreen) -> Self {
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
            let mut picture = Picture::new(feed.clone(), width, height, screen, layers);
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
            webcam: None,
            screen: Arc::new(Mutex::new(crate::ScreenPlacement::whole(crate::Framing::Fit))),
            // HASTECLIP_WEBCAM=<device> draws a webcam in the corner.
            layers: Arc::new(Mutex::new(
                std::iter::once(crate::Layer { id: 0, source: crate::LayerSource::Main })
                    .chain(std::env::var("HASTECLIP_WEBCAM").ok().map(|device| crate::Layer {
                        id: 1,
                        source: crate::LayerSource::Camera { device, format: None, placement: crate::webcam::Placement::default_for(16.0 / 9.0, 16.0 / 9.0) },
                    }))
                    .collect(),
            )),
            sources: Vec::new(),
            stream: None,
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
        let file = rec.stop(Mode::Record, None).unwrap().expect("a file");
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

    /// Clipping, going live, frame blending changed while it runs, and a
    /// recording beside them, all from one capture: each gets what it
    /// should (`crate::outputs::tests::everything_at_once`). Blending needs
    /// the graphics card to take the screen's pictures; ffmpeg and ffprobe
    /// stand in for the server and look at what came out.
    #[test]
    #[ignore = "captures the screen and streams it to a local server: cargo test -p capture everything_at_once -- --ignored --nocapture"]
    fn everything_at_once() {
        let mut rec = LinuxRecorder::new(Arc::new(LiveAudio::default()));
        crate::outputs::tests::everything_at_once(&mut rec);
        drop(rec);
        shutdown();
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
