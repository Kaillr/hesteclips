//! The pictures of a capture's layers other than its own (see
//! `crate::layers`): each webcam's newest picture, other displays' and app
//! windows' captures, pictures and colours, animated pictures and video
//! files, web pages, for the frame (`super::Picture`) to draw with the
//! screen's, on the graphics card (`super::gpu_frame`) or the CPU
//! (`super::image`).
//!
//! The layer list changes while it runs (the app keeps it up to date): a
//! layer added gets its picture, one taken out lets go of it, one changed to
//! another camera, display, window, picture or file gets the new one's.
//! [`Feeds::stack`] follows it, on the pacer's thread, once per frame.
//! What takes time to open (a display's or a window's capture, a video, a
//! web page) opens on a thread of its own, and shows once it's there.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use super::camera::CameraLayer;
use super::image::CameraPicture;
use super::screen::{Frame, Order, Screen};
use crate::layers::{Animation, Layer, LayerSource, Picture, SharedLayers};
use crate::webcam::{Format, Placement};
use crate::{ScreenPlacement, StillImage};

/// A layer's picture as it is now.
#[derive(Clone)]
pub(crate) enum Pixels {
    /// A capture's (a display, a window) or a video's frame: in our memory,
    /// or on the graphics card.
    Frame(Arc<Frame>),
    /// A webcam's: B, G, R, A rows.
    Camera(Arc<CameraPicture>),
    /// A picture: B, G, R, A rows, its alpha kept.
    Still(Arc<StillImage>),
    /// An animated picture's frame `n` (B, G, R, A rows, its alpha kept).
    Animated(Animation, usize),
    /// A web page's frame (`super::web`), in our memory or on the graphics
    /// card: its colours premultiplied by its alpha, as Chromium draws them.
    Page(Arc<Frame>),
}

impl Pixels {
    pub(crate) fn size(&self) -> (u32, u32) {
        match self {
            Pixels::Frame(f) | Pixels::Page(f) => (f.width, f.height),
            Pixels::Camera(c) => (c.width, c.height),
            Pixels::Still(s) => (s.width, s.height),
            Pixels::Animated(a, n) => a.0.get(*n).map_or((0, 0), |(s, _)| (s.width, s.height)),
        }
    }

    /// The picture on the graphics card, if it's there.
    pub(crate) fn texture(&self) -> Option<&wgpu::Texture> {
        match self {
            Pixels::Frame(f) | Pixels::Page(f) => f.texture.as_ref(),
            _ => None,
        }
    }

    /// Whether its colours are premultiplied by its alpha already (else
    /// they're blended by it as they are).
    pub(crate) fn premultiplied(&self) -> bool {
        matches!(self, Pixels::Page(_))
    }

    /// The picture in our memory: its rows, the bytes from one to the
    /// next, and which way round its colours are.
    pub(crate) fn rows(&self) -> Option<(&[u8], usize, Order)> {
        match self {
            Pixels::Frame(f) | Pixels::Page(f) if f.texture.is_none() => Some((&f.data, f.stride, f.order)),
            Pixels::Frame(_) | Pixels::Page(_) => None,
            Pixels::Camera(c) => Some((&c.bgra, c.width as usize * 4, Order::Bgr)),
            Pixels::Still(s) => Some((&s.bgra, s.width as usize * 4, Order::Bgr)),
            Pixels::Animated(a, n) => a.0.get(*n).map(|(s, _)| (&s.bgra[..], s.width as usize * 4, Order::Bgr)),
        }
    }
}

/// Where a layer's picture goes.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Place {
    /// A webcam's: cropped to fill its box, turned and flipped.
    Camera(Placement),
    /// Anything else's: into its box as a display's is.
    Boxed(ScreenPlacement),
}

/// A layer that shows: its picture, where it goes, and whether it's
/// blended over what's under it by its alpha (else it covers its box).
#[derive(Clone)]
pub(crate) struct Shown {
    pub id: u64,
    /// Changes whenever its picture does.
    pub seq: u64,
    pub pixels: Pixels,
    pub place: Place,
    pub blended: bool,
}

impl Shown {
    /// What it is and where, to tell whether anything changed.
    pub(crate) fn key(&self) -> (u64, u64, Place, bool) {
        (self.id, self.seq, self.place, self.blended)
    }
}

/// What shows now, bottom first: the layers under the recording's own
/// picture, whether that picture's in the list at all (a scene can do
/// without it), and the layers over it.
#[derive(Default)]
pub(crate) struct Stack {
    pub under: Vec<Shown>,
    pub main: bool,
    pub over: Vec<Shown>,
}

impl Stack {
    /// Only the recording's own picture.
    pub(crate) fn main_only() -> Self {
        Self { under: Vec::new(), main: true, over: Vec::new() }
    }

    /// What shows, to tell whether anything changed.
    pub(crate) fn key(&self) -> StackKey {
        (self.under.iter().map(Shown::key).collect(), self.main, self.over.iter().map(Shown::key).collect())
    }
}

/// What a [`Stack`] shows: each layer's `Shown::key`, under the picture,
/// whether there's the picture, and over it.
pub(crate) type StackKey = (Vec<(u64, u64, Place, bool)>, bool, Vec<(u64, u64, Place, bool)>);

/// The layers' pictures, for one capture (the recording's, or the preview's).
pub(crate) struct Feeds {
    layers: SharedLayers,
    /// The capture's frame rate (a web page is drawn up to it).
    fps: u32,
    /// Each layer's own picture source, by layer id (none for the main
    /// picture).
    feeds: Vec<(u64, Feed)>,
}

/// One layer's picture.
enum Feed {
    Camera { device: String, format: Option<Format>, layer: CameraLayer },
    Display { id: String, capture: CaptureFeed },
    Window { app: String, capture: CaptureFeed },
    /// A picture or a colour.
    Still { of: Still, picture: Arc<StillImage>, seq: u64 },
    /// An animated picture: the frame showing by the time since it started.
    Animated { of: Animation, start: Instant, seq: u64 },
    Video { path: PathBuf, size: (u32, u32), sound: Option<String>, video: VideoFeed },
    /// A web page, drawn by Chromium's engine (`super::web`).
    Browser { url: String, size: (u32, u32), css: String, page: super::web::PageHandle },
}

/// What a still layer's picture was made from.
#[derive(PartialEq)]
enum Still {
    Picture(Picture),
    Colour([u8; 4]),
}

/// A number no other picture has: a still's, or the first of an
/// animation's or a capture's numbers (each has 2^40 of them).
pub(crate) fn fresh_seq() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed) << 40
}

impl Feed {
    /// Whether it's still the picture `source` wants.
    fn is(&self, source: &LayerSource) -> bool {
        match (self, source) {
            (Feed::Camera { device, format, .. }, LayerSource::Camera { device: d, format: f, .. }) => device == d && format == f,
            (Feed::Display { id, .. }, LayerSource::Display { id: i, .. }) => id == i,
            (Feed::Window { app, .. }, LayerSource::Window { app: a, .. }) => app == a,
            (Feed::Still { of: Still::Picture(p), .. }, LayerSource::Picture { picture, .. }) => p == picture,
            (Feed::Still { of: Still::Colour(c), .. }, LayerSource::Colour { rgba, .. }) => c == rgba,
            (Feed::Animated { of, .. }, LayerSource::Animation { animation, .. }) => of == animation,
            (Feed::Video { path, size, sound, .. }, LayerSource::Video { path: p, width, height, sound: s, .. }) => path == p && *size == (*width, *height) && sound == s,
            (Feed::Browser { url, size, css, .. }, LayerSource::Browser { url: u, width, height, css: c, .. }) => url == u && *size == (*width, *height) && css == c,
            _ => false,
        }
    }

    /// Its picture as it is now, and its number.
    fn picture(&mut self) -> Option<(Pixels, u64)> {
        match self {
            Feed::Camera { layer, .. } => layer.latest().map(|c| {
                let seq = c.seq;
                (Pixels::Camera(c), seq)
            }),
            Feed::Display { capture, .. } | Feed::Window { capture, .. } => capture.picture().map(|(f, seq)| (Pixels::Frame(f), seq)),
            Feed::Still { picture, seq, .. } => Some((Pixels::Still(picture.clone()), *seq)),
            Feed::Animated { of, start, seq } => {
                let n = frame_at(&of.0, start.elapsed());
                Some((Pixels::Animated(of.clone(), n), *seq + n as u64))
            }
            Feed::Video { video, .. } => video.picture().map(|(f, seq)| (Pixels::Frame(f), seq)),
            Feed::Browser { page, .. } => page.picture().map(|(f, seq)| (Pixels::Page(f), seq)),
        }
    }
}

impl Feeds {
    pub(crate) fn new(layers: SharedLayers, fps: u32) -> Self {
        Self { layers, fps, feeds: Vec::new() }
    }

    /// Follow the layer list (a picture for each layer added or changed,
    /// none for those gone), and say what shows now.
    pub(crate) fn stack(&mut self) -> Stack {
        let layers = self.layers.lock().map(|l| l.clone()).unwrap_or_default();
        self.follow(&layers);
        let mut stack = Stack::default();
        for l in &layers {
            if matches!(l.source, LayerSource::Main) {
                stack.main = true;
                continue;
            }
            if !l.shown() {
                continue;
            }
            let Some(feed) = self.feeds.iter_mut().find(|(id, _)| *id == l.id).map(|(_, f)| f) else { continue };
            let Some((pixels, seq)) = feed.picture() else { continue };
            let place = match &l.source {
                LayerSource::Camera { placement, .. } => Place::Camera(*placement),
                LayerSource::Display { placement, .. }
                | LayerSource::Window { placement, .. }
                | LayerSource::Picture { placement, .. }
                | LayerSource::Colour { placement, .. }
                | LayerSource::Browser { placement, .. }
                | LayerSource::Animation { placement, .. }
                | LayerSource::Video { placement, .. } => Place::Boxed(*placement),
                LayerSource::Main => continue,
            };
            let shown = Shown { id: l.id, seq, pixels, place, blended: l.source.blended() };
            if stack.main { stack.over.push(shown) } else { stack.under.push(shown) }
        }
        stack
    }

    /// A feed for each layer added (or changed to another camera, display,
    /// picture…), none for those gone; each video told whether it shows.
    fn follow(&mut self, layers: &[Layer]) {
        self.feeds.retain(|(id, feed)| layers.iter().any(|l| l.id == *id && feed.is(&l.source)));
        for l in layers {
            if self.feeds.iter().any(|(id, _)| *id == l.id) {
                continue;
            }
            let feed = match &l.source {
                LayerSource::Main => continue,
                // A webcam with no camera (one's being set up): nothing.
                LayerSource::Camera { device, .. } if device.is_empty() => continue,
                LayerSource::Camera { device, format, placement } => Feed::Camera { device: device.clone(), format: *format, layer: CameraLayer::new(device, *format, placement.is_hidden()) },
                LayerSource::Display { id, .. } => Feed::Display { id: id.clone(), capture: CaptureFeed::start(Target::Display(id.clone())) },
                LayerSource::Window { app, .. } => Feed::Window { app: app.clone(), capture: CaptureFeed::start(Target::Window(app.clone())) },
                LayerSource::Picture { picture, .. } => Feed::Still { of: Still::Picture(picture.clone()), picture: picture.0.clone(), seq: fresh_seq() },
                LayerSource::Colour { rgba, .. } => {
                    let [r, g, b, a] = *rgba;
                    let picture = Arc::new(StillImage { width: 2, height: 2, bgra: [b, g, r, a].repeat(4) });
                    Feed::Still { of: Still::Colour(*rgba), picture, seq: fresh_seq() }
                }
                LayerSource::Animation { animation, .. } => {
                    if animation.0.is_empty() {
                        continue;
                    }
                    Feed::Animated { of: animation.clone(), start: Instant::now(), seq: fresh_seq() }
                }
                LayerSource::Video { path, width, height, fps, looped, sound, placement, .. } => {
                    let video = VideoFeed::start(path, (*width, *height), *fps, *looped, sound.clone(), !placement.hidden);
                    Feed::Video { path: path.clone(), size: (*width, *height), sound: sound.clone(), video }
                }
                LayerSource::Browser { url, width, height, css, .. } => {
                    let spec = super::web::Spec { layer: l.id, url: url.clone(), width: *width, height: *height, css: css.clone() };
                    Feed::Browser { url: url.clone(), size: (*width, *height), css: css.clone(), page: super::web::PageHandle::open(spec, self.fps, self.layers.clone()) }
                }
            };
            self.feeds.push((l.id, feed));
        }
        for (id, feed) in &mut self.feeds {
            if let Feed::Video { video, .. } = feed
                && let Some(LayerSource::Video { looped, restart, placement, .. }) = layers.iter().find(|l| l.id == *id).map(|l| &l.source)
            {
                video.set(!placement.hidden, *looped, *restart);
            }
        }
    }
}

/// Which of an animation's frames shows `t` after it started: each for its
/// time, over and over.
fn frame_at(frames: &[(StillImage, Duration)], t: Duration) -> usize {
    let total: Duration = frames.iter().map(|(_, d)| *d).sum();
    if total.is_zero() || frames.len() < 2 {
        return 0;
    }
    let mut left = Duration::from_nanos((t.as_nanos() % total.as_nanos()) as u64);
    for (i, (_, d)) in frames.iter().enumerate() {
        if left < *d {
            return i;
        }
        left -= *d;
    }
    frames.len() - 1
}

// ---------------------------------------------------------------------------
// Displays and windows
// ---------------------------------------------------------------------------

/// What a [`CaptureFeed`] captures: a display by id, an app's window by
/// program (`apps::list`'s ids).
enum Target {
    Display(String),
    Window(String),
}

/// Another display's, or an app window's, capture: started on a thread of
/// its own (and again, every few seconds, while it isn't there or its
/// capture stopped: a window closed and opened again), kept until dropped.
/// While there's none, nothing shows; a minimized window's last picture
/// stays.
struct CaptureFeed {
    shared: Arc<CaptureShared>,
    /// A window's program, whose window's size is said as it's seen
    /// ([`app_window_size`]), and the size last said.
    app: Option<String>,
    seen: AtomicU64,
}

#[derive(Default)]
struct CaptureShared {
    stop: AtomicBool,
    /// The capture running, and the first of its pictures' numbers.
    screen: Mutex<Option<(Arc<Screen>, u64)>>,
}

impl CaptureFeed {
    fn start(target: Target) -> Self {
        let shared = Arc::new(CaptureShared::default());
        let app = match &target {
            Target::Window(app) => Some(app.clone()),
            Target::Display(_) => None,
        };
        let shared2 = shared.clone();
        let _ = thread::Builder::new().name("layer capture".into()).spawn(move || capture_loop(&target, &shared2));
        Self { shared, app, seen: AtomicU64::new(0) }
    }

    /// Its newest picture, and its number.
    fn picture(&self) -> Option<(Arc<Frame>, u64)> {
        let screen = self.shared.screen.lock().ok()?;
        let (s, base) = screen.as_ref()?;
        // One read on request (X11) reads the next one now.
        s.latest.want();
        let (f, seq) = s.latest.get()?;
        if let Some(app) = &self.app {
            let size = u64::from(f.width) << 32 | u64::from(f.height);
            if self.seen.swap(size, Ordering::Relaxed) != size {
                saw_window(app, (f.width, f.height));
            }
        }
        Some((f, base + seq))
    }
}

impl Drop for CaptureFeed {
    /// Its thread lets go of the capture by itself.
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
    }
}

/// Keep `target` captured into `shared` until told to stop.
fn capture_loop(target: &Target, shared: &CaptureShared) {
    let name = match target {
        Target::Display(id) => format!("display layer {id}"),
        Target::Window(app) => format!("window layer {app}"),
    };
    let stopped = || shared.stop.load(Ordering::Relaxed);
    let wait = |d: Duration| {
        let until = Instant::now() + d;
        while Instant::now() < until && !stopped() {
            thread::sleep(Duration::from_millis(50));
        }
    };
    let mut said = None::<String>;
    while !stopped() {
        let opened: Result<(Arc<Screen>, Option<String>)> = match target {
            // Never the desktop's own "which screen?" dialog for a layer.
            Target::Display(_) if !super::screens_listed() => Err(anyhow::anyhow!("this desktop doesn't say which displays there are")),
            // Not one that's there: nothing (a capture of it would take the
            // main display instead).
            Target::Display(id) if !super::list_screens().iter().any(|d| d.id == *id) => Err(anyhow::anyhow!("the display isn't connected")),
            Target::Display(id) => super::screen::acquire(id).map(|s| (s, None)),
            Target::Window(_) if !super::apps::available() => Err(anyhow::anyhow!("windows can't be captured on this desktop")),
            Target::Window(app) => match super::apps::find_window(app) {
                Some(window) => super::apps::open_window(&window).map(|s| (Arc::new(s), Some(window))),
                None => Err(anyhow::anyhow!("it isn't open")),
            },
        };
        match opened {
            Ok((screen, window)) => {
                said = None;
                *shared.screen.lock().unwrap() = Some((screen.clone(), fresh_seq()));
                // Till its capture stops, or its window closes (one hidden
                // away is looked for again: it may be another).
                let mut looked = Instant::now();
                while !stopped() && screen.latest.ended().is_none() {
                    if let Some(w) = &window
                        && looked.elapsed() > Duration::from_millis(250)
                    {
                        if !super::apps::window_open(w) {
                            break;
                        }
                        looked = Instant::now();
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                shared.screen.lock().unwrap().take();
                // A display's capture is kept a moment, as the screen's is
                // (it's shared: taken up again, it goes on).
                if window.is_none() {
                    super::screen::release(screen);
                }
            }
            Err(e) => {
                // Once each time it's another reason.
                let why = format!("{e:#}");
                if said.as_ref() != Some(&why) {
                    eprintln!("{name}: {why}");
                    said = Some(why);
                }
                wait(Duration::from_secs(if matches!(target, Target::Window(_)) { 1 } else { 3 }));
            }
        }
    }
}

/// App `exe`'s window's size, in pixels, as a layer of it last saw it
/// (`crate::layers::app_window_size`); else as the desktop says, where it does.
pub(crate) fn app_window_size(exe: &str) -> Option<(u32, u32)> {
    if let Some(size) = WINDOW_SIZES.lock().ok()?.iter().find(|(a, _)| a.eq_ignore_ascii_case(exe)).map(|(_, s)| *s) {
        return Some(size);
    }
    super::apps::window_size(exe)
}

/// Each app's window's size, as its layer last captured it.
static WINDOW_SIZES: Mutex<Vec<(String, (u32, u32))>> = Mutex::new(Vec::new());

/// Remember app `exe`'s window's size from its capture's picture.
fn saw_window(exe: &str, size: (u32, u32)) {
    if let Ok(mut sizes) = WINDOW_SIZES.lock() {
        match sizes.iter_mut().find(|(a, _)| a == exe) {
            Some((_, s)) => *s = size,
            None => sizes.push((exe.to_owned(), size)),
        }
    }
}

// ---------------------------------------------------------------------------
// Videos
// ---------------------------------------------------------------------------

/// A video file playing in a layer: decoded on a thread of its own, each
/// frame put up when its time comes. At its end it starts over (`looped`),
/// else its last picture stays; hidden it waits where it is, and shown again
/// goes on, or starts over (`restart`). Its sound goes into the audio source
/// `sound` names, as it plays (`crate::sources::video_sound`).
struct VideoFeed {
    shared: Arc<VideoShared>,
}

#[derive(Default)]
struct VideoShared {
    stop: AtomicBool,
    shown: AtomicBool,
    looped: AtomicBool,
    /// From the start: it's been shown again, with restart on.
    again: AtomicBool,
    /// The frame up, and its number.
    frame: Mutex<Option<(Arc<Frame>, u64)>>,
}

/// The biggest a video layer's decoded at: a bigger one is shrunk.
const MAX_VIDEO: (u32, u32) = (3840, 2160);

/// A size that fits in `max`, keeping the shape, its sides even.
fn fit_within((w, h): (u32, u32), max: (u32, u32)) -> (u32, u32) {
    if w <= max.0 && h <= max.1 {
        return (w.max(2) & !1, h.max(2) & !1);
    }
    let scale = (max.0 as f64 / w as f64).min(max.1 as f64 / h as f64);
    (((w as f64 * scale) as u32).max(2) & !1, ((h as f64 * scale) as u32).max(2) & !1)
}

impl VideoFeed {
    fn start(path: &Path, size: (u32, u32), fps: f64, looped: bool, sound: Option<String>, shown: bool) -> Self {
        let shared = Arc::new(VideoShared::default());
        shared.shown.store(shown, Ordering::Relaxed);
        shared.looped.store(looped, Ordering::Relaxed);
        let (shared2, path) = (shared.clone(), path.to_owned());
        let size = fit_within((size.0.max(2), size.1.max(2)), MAX_VIDEO);
        let _ = thread::Builder::new().name("video layer".into()).spawn(move || {
            if let Err(e) = play_video(&path, size, fps, sound, &shared2) {
                eprintln!("video layer {}: {e:#}", path.display());
            }
        });
        Self { shared }
    }

    /// The layer as it is now: shown or hidden, looping or not, starting
    /// over when it's shown again or not.
    fn set(&self, shown: bool, looped: bool, restart: bool) {
        let was = self.shared.shown.swap(shown, Ordering::AcqRel);
        if shown && !was && restart {
            self.shared.again.store(true, Ordering::Release);
        }
        self.shared.looped.store(looped, Ordering::Relaxed);
    }

    fn picture(&self) -> Option<(Arc<Frame>, u64)> {
        self.shared.frame.lock().ok()?.clone()
    }
}

impl Drop for VideoFeed {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
    }
}

/// Play `path` at `size` into `shared` at `fps` (0: as the file says) till
/// it's stopped (see [`VideoFeed`]), each frame when its time comes; its
/// sound into the audio source `sound`, if it has one.
fn play_video(path: &Path, size: (u32, u32), fps: f64, sound: Option<String>, shared: &VideoShared) -> Result<()> {
    let mut frames = Frames::open(path, size, fps)?;
    // Its first sound goes with its first picture.
    let mut sound = sound.and_then(|id| match Sound::open(path, &id) {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("video layer {}: no sound ({e:#})", path.display());
            None
        }
    });
    let fps = frames.fps();
    let at = |index: u64| Duration::from_secs_f64(index as f64 / fps);
    let mut seq = fresh_seq();
    let mut put = |frame: Frame| {
        seq += 1;
        *shared.frame.lock().unwrap() = Some((Arc::new(frame), seq));
    };
    // When frame 0 was (or would have been) up: moved on by the time it's
    // been hidden, so it goes on where it was.
    let mut origin = Instant::now();
    let mut hidden_since: Option<Instant> = None;
    let mut ended = false;
    // When the last frame went up: one numbered as it was (two close
    // together in a video of an uneven rate) goes a frame after it.
    let mut last_due = None::<Instant>;
    while !shared.stop.load(Ordering::Relaxed) {
        if !shared.shown.load(Ordering::Acquire) {
            hidden_since.get_or_insert_with(Instant::now);
            thread::sleep(Duration::from_millis(20));
            continue;
        }
        if let Some(t) = hidden_since.take() {
            origin += t.elapsed();
        }
        if shared.again.swap(false, Ordering::AcqRel) || (ended && shared.looped.load(Ordering::Relaxed)) {
            ended = false;
            origin = Instant::now();
            last_due = Some(origin);
            if let Some(s) = sound.as_mut() {
                s.rewind();
            }
            if let Some((_, f)) = frames.first()? {
                put(f);
            }
            continue;
        }
        if let Some(s) = sound.as_mut() {
            s.feed(origin);
        }
        if ended {
            thread::sleep(Duration::from_millis(20));
            continue;
        }
        let Some((index, frame)) = frames.next()? else {
            ended = true;
            continue;
        };
        let due = match last_due {
            Some(last) if origin + at(index) <= last => last + at(1),
            _ => origin + at(index),
        };
        last_due = Some(due);
        loop {
            let now = Instant::now();
            if now >= due || shared.stop.load(Ordering::Relaxed) || shared.again.load(Ordering::Relaxed) || !shared.shown.load(Ordering::Relaxed) {
                break;
            }
            if let Some(s) = sound.as_mut() {
                s.feed(origin);
            }
            thread::sleep((due - now).min(Duration::from_millis(10)));
        }
        // Far behind (the computer busy): its time is from now on.
        let now = Instant::now();
        if now > due + Duration::from_millis(500) {
            origin = now.checked_sub(at(index)).unwrap_or(now);
        }
        put(frame);
    }
    Ok(())
}

/// A video's frames, in order, from its start: decoded here (MP4 and MOV),
/// else by an ffmpeg of its own (any other file it reads), at `size`, RGBA.
enum Frames {
    Here(Box<crate::decode::Decoder>),
    Ffmpeg(Piped),
}

impl Frames {
    fn open(path: &Path, size: (u32, u32), fps: f64) -> Result<Self> {
        match crate::decode::Decoder::open_rgba(path, size.0) {
            Ok(mut d) => {
                // Frames are numbered by their time at this rate.
                d.set_fps(fps);
                Ok(Self::Here(Box::new(d)))
            }
            Err(_) => Ok(Self::Ffmpeg(Piped::open(path, size, if fps > 1.0 { fps } else { 30.0 })?)),
        }
    }

    fn fps(&self) -> f64 {
        let fps = match self {
            Self::Here(d) => d.fps,
            Self::Ffmpeg(p) => p.fps,
        };
        if fps.is_finite() && fps > 1.0 { fps } else { 30.0 }
    }

    /// The next frame and its number; none at the end.
    fn next(&mut self) -> Result<Option<(u64, Frame)>> {
        match self {
            Self::Here(d) => Ok(d.next()?.map(|p| (p.index, rgba_frame(p.width, p.height, p.rgba)))),
            Self::Ffmpeg(p) => p.next(),
        }
    }

    /// Back to the start: its first frame (the next is the one after it).
    fn first(&mut self) -> Result<Option<(u64, Frame)>> {
        match self {
            Self::Here(d) => Ok(d.frame(0)?.map(|p| (p.index, rgba_frame(p.width, p.height, p.rgba)))),
            Self::Ffmpeg(p) => {
                p.restart()?;
                p.next()
            }
        }
    }
}

/// Decoded pixels (R, G, B, A rows, packed) as a frame.
fn rgba_frame(width: u32, height: u32, rgba: Vec<u8>) -> Frame {
    Frame { width, height, stride: width as usize * 4, order: Order::Rgb, data: rgba, texture: None }
}

/// A video decoded by ffmpeg into a pipe, a frame at a time as it's read
/// (ffmpeg waits while it isn't): at `size`, `fps` frames a second.
struct Piped {
    path: PathBuf,
    size: (u32, u32),
    fps: f64,
    child: Child,
    out: ChildStdout,
    next: u64,
}

impl Piped {
    fn open(path: &Path, size: (u32, u32), fps: f64) -> Result<Self> {
        let (child, out) = Self::spawn(path, size, fps)?;
        Ok(Self { path: path.to_owned(), size, fps, child, out, next: 0 })
    }

    fn spawn(path: &Path, (w, h): (u32, u32), fps: f64) -> Result<(Child, ChildStdout)> {
        let mut child = super::ffmpeg::ffmpeg()
            .arg("-i")
            .arg(path)
            .args(["-an", "-sn", "-vf", &format!("scale={w}:{h}"), "-r", &format!("{fps}"), "-f", "rawvideo", "-pix_fmt", "rgba", "pipe:1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("couldn't start ffmpeg")?;
        let out = child.stdout.take().context("ffmpeg's output")?;
        Ok((child, out))
    }

    fn next(&mut self) -> Result<Option<(u64, Frame)>> {
        let (w, h) = self.size;
        let mut rgba = vec![0u8; w as usize * h as usize * 4];
        if self.out.read_exact(&mut rgba).is_err() {
            if self.next == 0 {
                bail!("ffmpeg couldn't read it");
            }
            return Ok(None);
        }
        let index = self.next;
        self.next += 1;
        Ok(Some((index, rgba_frame(w, h, rgba))))
    }

    fn restart(&mut self) -> Result<()> {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let (child, out) = Self::spawn(&self.path, self.size, self.fps)?;
        (self.child, self.out, self.next) = (child, out, 0);
        Ok(())
    }
}

impl Drop for Piped {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A video layer's sound: read from its file as it plays (by an ffmpeg of
/// its own, never all at once: a long video's would fill memory), each block
/// handed to the mix a moment before it's heard.
struct Sound {
    reader: SoundReader,
    playing: crate::sources::video_sound::Playing,
    /// A block read whose time hasn't come yet.
    held: Option<(f64, Vec<f32>)>,
    /// All of it's been handed over (or it can't be read further).
    ended: bool,
}

/// How far ahead of being heard a video's sound goes to the mix (which runs
/// behind real time): little enough that what's gone when it starts over
/// is done in a moment.
const SOUND_AHEAD: f64 = 0.05;

impl Sound {
    fn open(path: &Path, id: &str) -> Result<Self> {
        let reader = SoundReader::open(path)?;
        let playing = crate::sources::video_sound::Playing::new(id, SoundReader::RATE);
        Ok(Self { reader, playing, held: None, ended: false })
    }

    /// Back to its start, with the picture.
    fn rewind(&mut self) {
        self.held = None;
        self.ended = false;
        if let Err(e) = self.reader.rewind() {
            eprintln!("video sound: {e:#}");
            self.ended = true;
        }
    }

    /// What's heard up to a moment from now handed to the mix, each block
    /// at its time after `origin` (when the video's start was up).
    fn feed(&mut self, origin: Instant) {
        if self.ended {
            return;
        }
        let played = origin.elapsed().as_secs_f64();
        let host_origin = crate::sources::host_now() - played;
        loop {
            let block = match self.held.take() {
                Some(b) => b,
                None => match self.reader.next() {
                    Some(b) => b,
                    None => {
                        self.ended = true;
                        return;
                    }
                },
            };
            if block.0 > played + SOUND_AHEAD {
                self.held = Some(block);
                return;
            }
            self.playing.push_at(host_origin + block.0, &block.1, SoundReader::CHANNELS);
        }
    }
}

/// A file's sound, from its start, a block at a time: decoded by ffmpeg
/// into a pipe, 48 kHz stereo.
struct SoundReader {
    path: PathBuf,
    child: Child,
    out: ChildStdout,
    /// Frames read so far: the next block's time.
    read: u64,
}

impl SoundReader {
    const RATE: u32 = 48_000;
    const CHANNELS: usize = 2;
    /// Frames a block.
    const BLOCK: usize = 1024;

    fn open(path: &Path) -> Result<Self> {
        let (child, out) = Self::spawn(path)?;
        let mut reader = Self { path: path.to_owned(), child, out, read: 0 };
        // A file with no sound says so at once (ffmpeg ends with nothing).
        let first = reader.next().context("it has none")?;
        reader.rewind()?;
        drop(first);
        Ok(reader)
    }

    fn spawn(path: &Path) -> Result<(Child, ChildStdout)> {
        let mut child = super::ffmpeg::ffmpeg()
            .arg("-i")
            .arg(path)
            .args(["-vn", "-sn", "-ac", "2", "-ar", "48000", "-f", "f32le", "pipe:1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("couldn't start ffmpeg")?;
        let out = child.stdout.take().context("ffmpeg's output")?;
        Ok((child, out))
    }

    /// The next block (its time from the start, and its samples,
    /// interleaved); none at the end.
    fn next(&mut self) -> Option<(f64, Vec<f32>)> {
        let mut bytes = vec![0u8; Self::BLOCK * Self::CHANNELS * 4];
        let mut got = 0;
        while got < bytes.len() {
            match self.out.read(&mut bytes[got..]) {
                Ok(0) | Err(_) => break,
                Ok(n) => got += n,
            }
        }
        let frames = got / (Self::CHANNELS * 4);
        if frames == 0 {
            return None;
        }
        let samples = bytes[..frames * Self::CHANNELS * 4].chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
        let at = self.read as f64 / Self::RATE as f64;
        self.read += frames as u64;
        Some((at, samples))
    }

    fn rewind(&mut self) -> Result<()> {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let (child, out) = Self::spawn(&self.path)?;
        (self.child, self.out, self.read) = (child, out, 0);
        Ok(())
    }
}

impl Drop for SoundReader {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A video file's picture size, if it can be played: read here (MP4,
/// MOV), else asked of ffprobe.
pub(crate) fn video_size(path: &Path) -> Option<(u32, u32)> {
    if let Ok(d) = crate::decode::Decoder::open_rgba(path, 64) {
        return Some(d.source_size());
    }
    let ffprobe = std::env::current_exe().ok().and_then(|exe| Some(exe.parent()?.join("ffprobe"))).filter(|p| p.is_file()).unwrap_or_else(|| PathBuf::from("ffprobe"));
    let out = std::process::Command::new(ffprobe)
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=width,height", "-of", "csv=p=0:s=x"])
        .arg(path)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let (w, h) = text.lines().next()?.trim().split_once('x')?;
    let (w, h) = (w.trim().parse().ok()?, h.trim().parse().ok()?);
    (w > 0 && h > 0).then_some((w, h))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    /// Each frame shows for its time, then the next; after the last, the
    /// first again.
    #[test]
    fn an_animation_shows_each_frame_for_its_time() {
        let still = || crate::StillImage { width: 1, height: 1, bgra: vec![0; 4] };
        let frames = [(still(), Duration::from_millis(100)), (still(), Duration::from_millis(50)), (still(), Duration::from_millis(200))];
        let at = |ms: u64| super::frame_at(&frames, Duration::from_millis(ms));
        assert_eq!([at(0), at(99), at(100), at(149), at(150), at(349), at(350), at(460)], [0, 0, 1, 1, 2, 2, 0, 1]);
        assert_eq!(super::frame_at(&frames[..1], Duration::from_secs(9)), 0);
    }

    /// A video file decoded here or by ffmpeg plays to its end in order,
    /// then from its first frame again; its sound reads as long as it.
    #[test]
    fn a_video_plays_to_its_end_and_from_the_start_again() {
        if super::super::ffmpeg::ffmpeg().arg("-version").stdout(std::process::Stdio::null()).status().is_err() {
            eprintln!("no ffmpeg here: skipped");
            return;
        }
        let dir = std::env::temp_dir().join(format!("hc-video-layer-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for ext in ["mp4", "webm"] {
            let path = dir.join(format!("v.{ext}"));
            let codec: &[&str] = if ext == "mp4" { &["-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac"] } else { &["-c:v", "libvpx-vp9", "-deadline", "realtime", "-c:a", "libopus"] };
            let made = super::super::ffmpeg::ffmpeg()
                .args(["-y", "-f", "lavfi", "-i", "testsrc=size=320x180:rate=30:duration=1", "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000:duration=1"])
                .args(codec)
                .arg(&path)
                .status()
                .unwrap();
            if !made.success() {
                eprintln!("{ext}: ffmpeg can't make one here: skipped");
                continue;
            }
            assert_eq!(super::video_size(&path), Some((320, 180)), "{ext}");
            let mut frames = super::Frames::open(&path, (320, 180), 30.0).unwrap();
            let (mut last, mut n) = (None::<u64>, 0);
            while let Some((index, f)) = frames.next().unwrap() {
                assert!(last.is_none_or(|l| index > l), "{ext}: in order: {index} after {last:?}");
                assert_eq!((f.width, f.height), (320, 180));
                last = Some(index);
                n += 1;
            }
            assert!((28..=31).contains(&n), "{ext}: {n} frames");
            assert_eq!(frames.first().unwrap().map(|(i, _)| i), Some(0), "{ext}");
            assert!(frames.next().unwrap().is_some_and(|(i, _)| (1..5).contains(&i)), "{ext}");
            let mut sound = super::SoundReader::open(&path).unwrap();
            let mut secs = 0.0;
            while let Some((t, samples)) = sound.next() {
                assert!(t >= secs - 1e-9);
                secs = t + (samples.len() / 2) as f64 / 48_000.0;
            }
            assert!((0.9..1.15).contains(&secs), "{ext}: sound {secs:.2} s");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
