//! Native Linux capture backend: the ScreenCast desktop portal + PipeWire,
//! encoded by ffmpeg. Works on Wayland and X11 desktops that have a portal
//! (GNOME, KDE Plasma, and wlroots or Hyprland ones with theirs).
//!
//! - **Video**: the portal asks which screen to share (once; the choice is
//!   remembered) and hands over a PipeWire stream of it (`portal`,
//!   `screen`). A pacer thread takes the newest picture every 1/fps, scales
//!   it to the output size, draws the webcam over it and converts it to NV12
//!   (`image`), so the output is constant frame rate even when the screen is
//!   still and nothing new arrives. ffmpeg encodes it (`ffmpeg`): NVENC or
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

pub mod audio;
mod camera;
pub mod ffmpeg;
mod image;
mod portal;
mod screen;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result, bail};

pub use camera::{keep_open as keep_camera_open, list_cameras};

use crate::mixer::{self, Clock, LiveAudio};
use crate::mp4file::Layout;
use crate::mp4mux::Spec;
use crate::output::{in_progress, timestamp};
use crate::sources::{AudioCapture, mix_inputs, track_layout};
use crate::webcam::{Placement, SharedPlacement};
use crate::writer::{self, Writer};
use crate::{AUDIO_BITRATE, Device, EncodeSettings, Mode, Recorder, VideoSource};
use image::Canvas;

/// CLOCK_MONOTONIC in seconds: PipeWire's clock, and so every timestamp's.
pub(crate) fn host_now() -> f64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: a valid clock id and a timespec to fill.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as f64 + ts.tv_nsec as f64 / 1e9
}

/// The one screen entry: which screen is picked in the desktop's own dialog.
pub(crate) const SCREEN_ID: &str = "portal";

/// Screens to record. The desktop doesn't tell apps what screens there are
/// (on Wayland it can't be asked), so there's one entry, and the desktop asks
/// which screen it is the first time.
pub fn list_screens() -> Vec<Device> {
    vec![Device { id: SCREEN_ID.into(), name: "Screen (picked when recording first starts)".into() }]
}

/// Forget which screen was picked, so the next capture (the preview, when
/// it restarts) asks the desktop again.
pub fn choose_screen_again() {
    portal::forget_choice();
    screen::forget();
}

/// The screen's size in pixels, once one has been recorded.
pub(crate) fn display_pixels(_id: Option<&str>) -> Option<(u32, u32)> {
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
            let _ = self.stop();
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
}

impl Drop for LinuxRecorder {
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

/// How far behind real time the mixer runs, so every source's audio has
/// arrived (PipeWire delivers every ~10–20 ms, sometimes late). Timestamps
/// are exact, so this only delays writing — it never shifts audio against video.
const MIX_LATENCY: f64 = 0.3;

/// The screen, for a video source: games and apps can't be recorded here.
fn open_screen(source: &VideoSource) -> Result<Arc<screen::Screen>> {
    match source {
        VideoSource::Screen { .. } => screen::acquire(),
        VideoSource::Apps { .. } => bail!("recording games and apps isn't available on Linux — record the screen instead"),
    }
}

/// Everything a running capture owns. Torn down in `finish` in an order that
/// lets every encoder flush into the writer before it closes the file.
struct Session {
    screen: Option<Arc<screen::Screen>>,
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
        let screen = open_screen(&s.video)?;
        let (canvas_w, canvas_h) = screen.latest.get().map_or((1920, 1080), |(f, _)| (f.width, f.height));
        let (width, height) = crate::output_size(canvas_w, canvas_h, s.target_height);
        let clock = Arc::new(Clock::default());

        let (titles, comment, has_rest) = track_layout(&s.sources);
        let tracks = titles.len();
        let layout = Layout {
            spec: Spec { width, height, fps: s.fps, audio_titles: titles, audio_bitrate: AUDIO_BITRATE, comment, mov },
        };
        let writer = match target {
            Target::Record(file) => Writer::record(file, layout, s.fps),
            Target::Replay(seconds) => Writer::replay(seconds, layout, s.fps),
        };
        let writer_tx = writer.tx.clone();

        let mut session = Self {
            screen: Some(screen.clone()),
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

            let encoder = ffmpeg::H264::start(width, height, s, session.writer_tx.clone())?;
            let picture = Picture::new(screen.latest.clone(), width, height, s.webcam.as_ref());
            session.pacer = Some(spawn_pacer(s.fps, picture, encoder, clock));
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
        if let Some(screen) = self.screen.take() {
            screen::release(screen);
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

/// Makes the frame: the screen fitted to the output size, the webcam on top.
/// Redraws only when one of them changed.
struct Picture {
    latest: Arc<screen::Latest>,
    canvas: Canvas,
    camera: Option<(camera::CameraLayer, SharedPlacement)>,
    /// What the canvas shows: the screen picture's number, the camera picture
    /// and where it was placed.
    shows: Option<(u64, Option<usize>, Option<Placement>)>,
    /// The canvas as NV12, made when it changes.
    nv12: Vec<u8>,
    /// Counts the canvas's changes, so the preview is only sent new ones.
    version: u64,
}

impl Picture {
    fn new(latest: Arc<screen::Latest>, width: u32, height: u32, webcam: Option<&crate::webcam::Webcam>) -> Self {
        let camera = webcam.map(|w| (camera::CameraLayer::new(w), w.placement.clone()));
        let mut canvas = Canvas::new(width, height);
        canvas.clear();
        Self { latest, canvas, camera, shows: None, nv12: Vec::new(), version: 0 }
    }

    /// Bring the canvas up to date. Returns whether there's a picture yet.
    fn update(&mut self, nv12: bool) -> bool {
        let Some((frame, seq)) = self.latest.get() else { return false };
        let (cam, place) = match &mut self.camera {
            Some((layer, place)) => {
                let place = *place.lock().unwrap();
                (layer.latest().filter(|_| !place.is_hidden()), Some(place))
            }
            None => (None, None),
        };
        let now = (seq, cam.as_ref().map(|c| Arc::as_ptr(c) as usize), place);
        if self.shows.as_ref() != Some(&now) {
            self.canvas.fill_from(&frame);
            if let (Some(cam), Some(place)) = (&cam, &place) {
                self.canvas.draw_camera(cam, place);
            }
            self.shows = Some(now);
            self.nv12.clear();
            self.version += 1;
        }
        if nv12 && self.nv12.is_empty() {
            self.canvas.to_nv12(&mut self.nv12);
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
fn submit_preview(picture: &Picture, sent: &mut Previewed) {
    if sent.version == Some(picture.version) || !crate::preview::wants_frame() {
        return;
    }
    sent.version = Some(picture.version);
    let canvas = &picture.canvas;
    let mut rgba = sent.spare.take().and_then(|f| Arc::try_unwrap(f).ok()).map(|f| f.rgba).unwrap_or_default();
    let (w, h) = canvas.to_preview(PREVIEW_MAX, &mut rgba);
    let frame = crate::preview::PreviewFrame {
        width: w,
        height: h,
        recorded: (canvas.width, canvas.height),
        rgba,
        seq: 0,
        waiting: false,
        app: None,
    };
    sent.spare = Some(crate::preview::publish_frame(sent.generation, frame));
}

/// Encode the newest picture every 1/fps on the host clock. The screen only
/// sends pictures when something changes; repeating the last one keeps the
/// output constant frame rate. Frame N is stamped N/fps seconds after t0, the
/// same clock the audio uses.
fn spawn_pacer(fps: u32, mut picture: Picture, encoder: ffmpeg::H264, clock: Arc<Clock>) -> (Arc<AtomicBool>, JoinHandle<()>) {
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
                    if picture.update(true) {
                        let mut frame = encoder.buffer();
                        frame.clear();
                        frame.extend_from_slice(&picture.nv12);
                        // The encoder can't keep up: skip this frame (the file
                        // shows the previous one a little longer).
                        if !encoder.encode(frame, next) {
                            dropped += 1;
                            if dropped.is_power_of_two() {
                                eprintln!("video encoder is behind: {dropped} frames skipped");
                            }
                        }
                    }
                    next += 1;
                    burst += 1;
                }
                if !ended {
                    if let Some(why) = picture.latest.ended() {
                        // Sharing was stopped from the desktop: the file keeps the last picture.
                        eprintln!("{why}");
                        ended = true;
                    }
                }
                // The Sources page is showing the preview: hand it this picture too.
                submit_preview(&picture, &mut sent);
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
}

impl PreviewCapture {
    pub(crate) fn start(source: VideoSource, target_height: Option<u32>, fps: u32, webcam: Option<crate::webcam::Webcam>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let error = Arc::new(Mutex::new(None));
        let (stop2, error2) = (stop.clone(), error.clone());
        let generation = crate::preview::new_producer();
        thread::spawn(move || {
            let screen = match open_screen(&source) {
                Ok(s) => s,
                Err(e) => {
                    *error2.lock().unwrap() = Some(format!("{e:#}"));
                    return;
                }
            };
            let (w, h) = screen.latest.get().map_or((1920, 1080), |(f, _)| (f.width, f.height));
            let (width, height) = crate::output_size(w, h, target_height);
            let mut picture = Picture::new(screen.latest.clone(), width, height, webcam.as_ref());
            let mut sent = Previewed::new(generation);
            // Paced like the recording would be.
            let every = 1.0 / fps.max(1) as f64;
            let t0 = host_now();
            let mut n = 0u64;
            while !stop2.load(Ordering::Relaxed) {
                if let Some(why) = picture.latest.ended() {
                    *error2.lock().unwrap() = Some(why);
                    break;
                }
                if crate::preview::wants_frame() && picture.update(false) {
                    submit_preview(&picture, &mut sent);
                }
                n += 1;
                let wait = t0 + n as f64 * every - host_now();
                if wait > 0.0 {
                    thread::sleep(Duration::from_secs_f64(wait));
                } else if wait < -0.5 {
                    n = ((host_now() - t0) / every) as u64; // fell far behind: skip ahead
                }
            }
            drop(picture);
            screen::release(screen);
        });
        Self { stop, error }
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
