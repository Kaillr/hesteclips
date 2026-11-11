//! The pictures of a capture's layers other than its own (see
//! `crate::layers`), on macOS: each webcam's newest picture, another
//! display's or an app's window's capture (a ScreenCaptureKit stream of its
//! own), pictures, words and colours, animated pictures, video files
//! (AVFoundation's reader, their sound into their audio sources) and web
//! pages (`super::browser`), for the compositor (`super::gpu`) to draw.
//!
//! The layer list changes while it runs (the app keeps it up to date): a
//! layer added gets its picture, one taken out lets go of it, one changed
//! to another camera, display, window, page or file gets the new one's.
//! Only [`Feeds::update`] changes what's there, once per frame, on the
//! thread making frames.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send};
use objc2_av_foundation::{AVAssetReader, AVAssetReaderStatus, AVAssetReaderTrackOutput, AVAssetTrack, AVMediaTypeAudio, AVMediaTypeVideo, AVURLAsset};
use objc2_core_foundation::{CFRetained, CFString};
use objc2_core_graphics::kCGDisplayStreamYCbCrMatrix_ITU_R_709_2;
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::{
    CVPixelBuffer, kCVImageBufferYCbCrMatrixKey, kCVImageBufferYCbCrMatrix_ITU_R_601_4, kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferPixelFormatTypeKey,
    kCVPixelFormatType_32BGRA, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};
use objc2_foundation::{NSArray, NSDictionary, NSError, NSNumber, NSObject, NSObjectProtocol, NSString, NSURL};
use objc2_screen_capture_kit::{SCContentFilter, SCStream, SCStreamConfiguration, SCStreamOutput, SCStreamOutputType, SCWindow};

use super::camera::CameraLayer;
use super::gpu::{Alpha, bgra_buffer, fill_bgra};
use super::video::{Buffer, display_pixels, shareable_content, size_of};
use super::windows;
use crate::layers::{Animation, Layer, LayerSource, SharedLayers};

/// What a layer shows now, for the compositor: its picture, and how it goes
/// over what's under it.
pub(crate) struct Shown {
    pub picture: Arc<Buffer>,
    /// Another number whenever the picture's another (a frame that's made of
    /// the same needn't be made again).
    pub version: u64,
    pub alpha: Alpha,
    /// NV12 in BT.601 colours (a video file's).
    pub bt601: bool,
}

/// The layers' pictures, for one capture's frames.
pub(crate) struct Feeds {
    /// What the app wants drawn, bottom first.
    layers: SharedLayers,
    /// Each layer's own picture source, by layer id (none for the main
    /// picture).
    feeds: Vec<(u64, Feed)>,
    /// The rate a display or window is captured at.
    fps: u32,
}

/// One layer's picture.
enum Feed {
    Camera { device: String, format: Option<crate::webcam::Format>, layer: CameraLayer },
    Display { id: String, capture: Capture },
    Window { app: String, capture: Capture },
    Browser { url: String, size: (u32, u32), css: String, capture: Capture },
    /// A picture or a colour, made once.
    Still { of: Still, picture: Arc<Buffer> },
    /// An animated picture: the frame showing (by the time since it
    /// started) put in its buffer as it changes.
    Animated { of: Animation, picture: Arc<Buffer>, start: Instant, showing: usize },
    /// A video file playing (its sound into the source `sound`, if any).
    Video { path: PathBuf, size: (u32, u32), sound: Option<String>, video: VideoFeed },
}

/// What a still layer's picture was made from.
#[derive(PartialEq)]
enum Still {
    Picture(crate::layers::Picture),
    Colour([u8; 4]),
}

/// The biggest side a picture layer's made at: bigger is shrunk.
const MAX_PICTURE: u32 = 4096;

impl Feed {
    /// Whether it's still the picture `source` wants.
    fn is(&self, source: &LayerSource) -> bool {
        match (self, source) {
            (Feed::Camera { device, format, .. }, LayerSource::Camera { device: d, format: f, .. }) => device == d && format == f,
            (Feed::Display { id, .. }, LayerSource::Display { id: i, .. }) => id == i,
            (Feed::Window { app, .. }, LayerSource::Window { app: a, .. }) => app == a,
            (Feed::Browser { url, size, css, .. }, LayerSource::Browser { url: u, width, height, css: c, .. }) => url == u && *size == (*width, *height) && css == c,
            (Feed::Still { of: Still::Picture(p), .. }, LayerSource::Picture { picture, .. }) => p == picture,
            (Feed::Still { of: Still::Colour(c), .. }, LayerSource::Colour { rgba, .. }) => c == rgba,
            (Feed::Animated { of, .. }, LayerSource::Animation { animation, .. }) => of == animation,
            (Feed::Video { path, size, sound, .. }, LayerSource::Video { path: p, width, height, sound: s, .. }) => path == p && *size == (*width, *height) && sound == s,
            _ => false,
        }
    }

    /// Its picture now, if it has one.
    fn shown(&mut self) -> Option<Shown> {
        let covers = |(picture, version): (Arc<Buffer>, u64)| Shown { picture, version, alpha: Alpha::Cover, bt601: false };
        match self {
            Feed::Camera { layer, .. } => layer.latest().map(|f| Shown { picture: Arc::new(Buffer(f.buffer.clone())), version: f.seq, alpha: Alpha::Cover, bt601: false }),
            Feed::Display { capture, .. } | Feed::Window { capture, .. } => capture.pictures.get().map(covers),
            Feed::Browser { capture, .. } => capture.pictures.get().map(|(picture, version)| Shown { picture, version, alpha: Alpha::Premultiplied, bt601: false }),
            Feed::Still { picture, .. } => Some(Shown { picture: picture.clone(), version: 0, alpha: Alpha::Straight, bt601: false }),
            Feed::Animated { picture, showing, .. } => Some(Shown { picture: picture.clone(), version: *showing as u64, alpha: Alpha::Straight, bt601: false }),
            Feed::Video { video, .. } => video.pictures.get().map(|(picture, version)| Shown { picture, version, alpha: Alpha::Cover, bt601: video.bt601.load(Ordering::Relaxed) }),
        }
    }
}

/// `image` (straight alpha) in a buffer of its own, shrunk to fit
/// [`MAX_PICTURE`] (each pixel the one under it: a picture this big is rare).
fn still_buffer(image: &crate::StillImage) -> Result<Arc<Buffer>> {
    let (w, h) = (image.width.max(1), image.height.max(1));
    anyhow::ensure!(image.bgra.len() >= (w * h * 4) as usize, "the picture's pixels are missing");
    if w <= MAX_PICTURE && h <= MAX_PICTURE {
        return Ok(Arc::new(Buffer(bgra_buffer(w, h, &image.bgra)?)));
    }
    let k = (MAX_PICTURE as f64 / w as f64).min(MAX_PICTURE as f64 / h as f64);
    let (tw, th) = (((w as f64 * k) as u32).max(1), ((h as f64 * k) as u32).max(1));
    let mut small = vec![0u8; (tw * th * 4) as usize];
    for y in 0..th {
        let sy = (y as u64 * h as u64 / th as u64) as u32;
        for x in 0..tw {
            let sx = (x as u64 * w as u64 / tw as u64) as u32;
            let from = ((sy * w + sx) * 4) as usize;
            let to = ((y * tw + x) * 4) as usize;
            small[to..to + 4].copy_from_slice(&image.bgra[from..from + 4]);
        }
    }
    Ok(Arc::new(Buffer(bgra_buffer(tw, th, &small)?)))
}

impl Feeds {
    pub(crate) fn new(layers: SharedLayers, fps: u32) -> Self {
        Self { layers, feeds: Vec::new(), fps }
    }

    /// Follow the layer list: a picture for each layer added (or changed to
    /// another camera, display, window, page or file), none for those
    /// gone; animated pictures moved on, videos told whether they show.
    /// The layers, bottom first, each with its picture now, if it has one.
    pub(crate) fn update(&mut self) -> Vec<(Layer, Option<Shown>)> {
        let mut layers = self.layers.lock().unwrap().clone();
        if layers.is_empty() {
            layers.push(Layer { id: 0, source: LayerSource::Main });
        }
        self.feeds.retain(|(id, feed)| layers.iter().any(|l| l.id == *id && feed.is(&l.source)));
        for l in &layers {
            if matches!(l.source, LayerSource::Main) || self.feeds.iter().any(|(id, _)| *id == l.id) {
                continue;
            }
            match self.start(&l.source) {
                Ok(Some(feed)) => self.feeds.push((l.id, feed)),
                Ok(None) => {}
                Err(e) => eprintln!("layer {}: {e:#}", l.id),
            }
        }
        for (id, feed) in self.feeds.iter_mut() {
            let source = layers.iter().find(|l| l.id == *id).map(|l| &l.source);
            match (feed, source) {
                (Feed::Animated { of, picture, start, showing }, _) => {
                    let at = frame_at(&of.0, start.elapsed());
                    if at != *showing
                        && let Some((frame, _)) = of.0.get(at)
                        && frame.width <= MAX_PICTURE
                        && frame.height <= MAX_PICTURE
                    {
                        let _ = fill_bgra(&picture.0, frame.width as usize, frame.height as usize, &frame.bgra);
                        *showing = at;
                    }
                }
                (Feed::Video { video, .. }, Some(LayerSource::Video { looped, restart, placement, .. })) => video.set(!placement.hidden, *looped, *restart),
                _ => {}
            }
        }
        layers
            .into_iter()
            .map(|l| {
                let shown = self.feeds.iter_mut().find(|(id, _)| *id == l.id).and_then(|(_, f)| f.shown());
                (l, shown)
            })
            .collect()
    }

    /// The picture for a layer that's new.
    fn start(&self, source: &LayerSource) -> Result<Option<Feed>> {
        Ok(Some(match source {
            LayerSource::Main => return Ok(None),
            LayerSource::Camera { device, .. } if device.is_empty() => return Ok(None),
            LayerSource::Camera { device, format, placement } => Feed::Camera { device: device.clone(), format: *format, layer: CameraLayer::new(device, *format, placement.is_hidden()) },
            LayerSource::Display { id, .. } => Feed::Display { id: id.clone(), capture: Capture::start(Target::Display(id.clone()), self.fps) },
            LayerSource::Window { app, .. } => Feed::Window { app: app.clone(), capture: Capture::start(Target::Window(app.clone()), self.fps) },
            LayerSource::Browser { url, width, height, css, .. } => {
                let page = super::browser::Page::open(url.clone(), *width, *height, css.clone())?;
                Feed::Browser { url: url.clone(), size: (*width, *height), css: css.clone(), capture: Capture::start(Target::Page(page), self.fps) }
            }
            LayerSource::Picture { picture, .. } => Feed::Still { of: Still::Picture(picture.clone()), picture: still_buffer(&picture.0)? },
            LayerSource::Colour { rgba, .. } => {
                let [r, g, b, a] = *rgba;
                let image = crate::StillImage { width: 2, height: 2, bgra: [b, g, r, a].repeat(4) };
                Feed::Still { of: Still::Colour(*rgba), picture: still_buffer(&image)? }
            }
            LayerSource::Animation { animation, .. } => {
                let Some((first, _)) = animation.0.first() else { return Ok(None) };
                // One too big for its frames to be put in as they are: its
                // first, still (shrunk).
                let picture = still_buffer(first)?;
                Feed::Animated { of: animation.clone(), picture, start: Instant::now(), showing: 0 }
            }
            LayerSource::Video { path, width, height, sound, placement, looped, .. } => {
                Feed::Video { path: path.clone(), size: (*width, *height), sound: sound.clone(), video: VideoFeed::start(path, *looped, sound.clone(), !placement.hidden) }
            }
        }))
    }
}

/// Which of an animation's frames shows `t` after it started: each for its
/// time, over and over.
fn frame_at(frames: &[(crate::StillImage, Duration)], t: Duration) -> usize {
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
// Displays, windows and pages: ScreenCaptureKit streams of their own
// ---------------------------------------------------------------------------

/// The newest picture a stream (or a video) handed over, numbered.
#[derive(Default)]
pub(crate) struct Pictures {
    latest: Mutex<Option<(Arc<Buffer>, u64)>>,
    seq: AtomicU64,
}

impl Pictures {
    fn set(&self, picture: CFRetained<CVPixelBuffer>) {
        let n = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        *self.latest.lock().unwrap() = Some((Arc::new(Buffer(picture)), n));
    }

    /// Left out until the next: the window closed, the display went.
    fn forget(&self) {
        self.latest.lock().unwrap().take();
    }

    fn get(&self) -> Option<(Arc<Buffer>, u64)> {
        self.latest.lock().unwrap().clone()
    }
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSObject))]
    #[ivars = Arc<Pictures>]
    struct LayerOutput;

    unsafe impl NSObjectProtocol for LayerOutput {}

    unsafe impl SCStreamOutput for LayerOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output(&self, _stream: &SCStream, sample: &CMSampleBuffer, kind: SCStreamOutputType) {
            // Idle and blank frames carry no picture: the last one stays.
            if kind == SCStreamOutputType::Screen
                && let Some(image) = unsafe { sample.image_buffer() }
            {
                self.ivars().set(image);
            }
        }
    }
);

impl LayerOutput {
    fn new(to: Arc<Pictures>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(to);
        unsafe { msg_send![super(this), init] }
    }
}

/// What a [`Capture`] captures: a display by id, an app's window by id, a
/// web page's window once it's up.
enum Target {
    Display(String),
    Window(String),
    Page(Arc<super::browser::Page>),
}

impl Target {
    fn name(&self) -> String {
        match self {
            Target::Display(id) => format!("display layer {id}"),
            Target::Window(app) => format!("window layer {app}"),
            Target::Page(p) => format!("web page {}", p.url),
        }
    }
}

/// Another display's, an app window's or a page's capture, on a thread of
/// its own that starts it (and again, every few seconds, while it isn't
/// there or its capture stopped: a window closed, opened again) and keeps
/// it until it's dropped. A window that changes size is captured at its new
/// size; a minimized one keeps its last picture.
struct Capture {
    pictures: Arc<Pictures>,
    stop: Arc<AtomicBool>,
}

/// A stream running, and what it's of.
struct Running {
    stream: Retained<SCStream>,
    _output: Retained<LayerOutput>,
    _watch: Retained<crate::sck::StreamWatch>,
    stopped: Arc<crate::sck::Stopped>,
    /// A window's id, and its size (points) when it was set up.
    window: Option<(u32, (f64, f64))>,
    /// A display's size (pixels) when it was set up.
    display: Option<(u32, (usize, usize))>,
    fps: u32,
    bgra: bool,
}

impl Capture {
    fn start(target: Target, fps: u32) -> Self {
        let pictures = Arc::new(Pictures::default());
        let stop = Arc::new(AtomicBool::new(false));
        let (pictures2, stop2) = (pictures.clone(), stop.clone());
        thread::spawn(move || {
            let name = target.name();
            let mut said = None::<String>;
            while !stop2.load(Ordering::Relaxed) {
                match open(&target, fps, &pictures2) {
                    Ok(mut running) => {
                        said = None;
                        while !stop2.load(Ordering::Relaxed) && keep(&mut running) {
                            thread::sleep(Duration::from_millis(100));
                        }
                        crate::sck::stop_capture(&running.stream);
                        pictures2.forget();
                    }
                    Err(e) => {
                        // Once each time it's another reason.
                        let why = format!("{e:#}");
                        if said.as_ref() != Some(&why) {
                            eprintln!("{name}: {why}");
                            said = Some(why);
                        }
                        pictures2.forget();
                        let until = Instant::now() + Duration::from_secs(3);
                        while Instant::now() < until && !stop2.load(Ordering::Relaxed) {
                            thread::sleep(Duration::from_millis(50));
                        }
                    }
                }
            }
        });
        Self { pictures, stop }
    }
}

impl Drop for Capture {
    /// Its thread stops the stream by itself.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Start capturing `target` into `pictures`.
fn open(target: &Target, fps: u32, pictures: &Arc<Pictures>) -> Result<Running> {
    let content = shareable_content()?;
    let (filter, (w, h), window, display, bgra, cursor) = match target {
        Target::Display(id) => {
            let want: u32 = id.parse().context("no such display")?;
            let display = unsafe { content.displays() }.iter().find(|d| unsafe { d.displayID() } == want).context("the display isn't connected")?;
            let px = display_pixels(&display);
            let filter = unsafe { SCContentFilter::initWithDisplay_excludingWindows(SCContentFilter::alloc(), &display, &NSArray::new()) };
            // As big as an encoder's picture goes, its shape kept.
            let (w, h) = crate::output_size(px.0 as u32, px.1 as u32, None);
            (filter, (w as usize, h as usize), None, Some((want, px)), false, true)
        }
        Target::Window(app) => {
            let win = windows::find_app_window(app, &windows::windows(false)).with_context(|| format!("{app} isn't open"))?;
            let sc: Retained<SCWindow> = unsafe { content.windows() }.iter().find(|w| unsafe { w.windowID() } == win.id).context("its window went away")?;
            let filter = unsafe { SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), &sc) };
            let size = window_pixels(&filter, &content, unsafe { sc.frame() });
            (filter, size, Some((win.id, (win.width, win.height))), None, false, true)
        }
        Target::Page(page) => {
            // Made on the main thread a moment ago: waited for.
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut content = content;
            let sc: Retained<SCWindow> = loop {
                let found = page.window().and_then(|id| unsafe { content.windows() }.iter().find(|w| unsafe { w.windowID() } == id));
                match found {
                    Some(w) => break w,
                    None if Instant::now() < deadline => {
                        thread::sleep(Duration::from_millis(100));
                        content = shareable_content()?;
                    }
                    None => bail!("the page didn't come up"),
                }
            };
            let filter = unsafe { SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), &sc) };
            // At the page's own size: as many pixels as its points.
            let id = unsafe { sc.windowID() };
            (filter, (page.size.0 as usize, page.size.1 as usize), Some((id, (page.size.0 as f64, page.size.1 as f64))), None, true, false)
        }
    };
    let config = layer_config(w, h, fps, bgra, cursor);
    let output = LayerOutput::new(pictures.clone());
    let stopped = Arc::new(crate::sck::Stopped::default());
    let watch = crate::sck::StreamWatch::new(stopped.clone());
    let stream = unsafe { SCStream::initWithFilter_configuration_delegate(SCStream::alloc(), &filter, &config, Some(ProtocolObject::from_ref(&*watch))) };
    let queue = DispatchQueue::new("hesteclips.layer", None);
    unsafe { stream.addStreamOutput_type_sampleHandlerQueue_error(ProtocolObject::from_ref(&*output), SCStreamOutputType::Screen, Some(&queue)) }
        .map_err(|e| anyhow::anyhow!("can't capture it: {}", e.localizedDescription()))?;
    crate::sck::start_capture(&stream)?;
    Ok(Running { stream, _output: output, _watch: watch, stopped, window, display, fps, bgra })
}

/// Whether a running stream goes on: not stopped by the system, its window
/// still there (set up again at its size, when that changed), its display
/// the same size.
fn keep(r: &mut Running) -> bool {
    if r.stopped.take().is_some() {
        return false;
    }
    if let Some((id, size)) = r.window {
        let Some(now) = windows::windows(false).into_iter().find(|w| w.id == id).map(|w| (w.width, w.height)).or_else(|| (r.bgra).then_some(size)) else {
            return false;
        };
        if now != size && !r.bgra {
            r.window = Some((id, now));
            let Ok(content) = shareable_content() else { return true };
            let Some(sc) = unsafe { content.windows() }.iter().find(|w| unsafe { w.windowID() } == id) else { return false };
            let filter = unsafe { SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), &sc) };
            let (w, h) = window_pixels(&filter, &content, unsafe { sc.frame() });
            let config = layer_config(w, h, r.fps, r.bgra, true);
            let (tx, rx) = std::sync::mpsc::channel();
            let handler = block2::RcBlock::new(move |_: *mut NSError| {
                let _ = tx.send(());
            });
            unsafe { r.stream.updateConfiguration_completionHandler(&config, Some(&handler)) };
            let _ = rx.recv_timeout(Duration::from_secs(2));
        }
    }
    if let Some((id, px)) = r.display {
        let now = super::display_pixels(Some(&id.to_string()));
        if now.is_some_and(|(w, h)| (w as usize, h as usize) != px) {
            return false;
        }
    }
    true
}

/// A window's size in pixels (`frame` in points), even: as ScreenCaptureKit
/// has its scale (macOS 14), else as the display under its middle has it.
fn window_pixels(filter: &SCContentFilter, content: &objc2_screen_capture_kit::SCShareableContent, frame: objc2_core_foundation::CGRect) -> (usize, usize) {
    let mut scale = if objc2::available!(macos = 14.0) { (unsafe { filter.pointPixelScale() }) as f64 } else { 0.0 };
    if scale < 1.0 {
        let (cx, cy) = (frame.origin.x + frame.size.width / 2.0, frame.origin.y + frame.size.height / 2.0);
        let displays = unsafe { content.displays() };
        let on = displays.iter().find(|d| {
            let f = unsafe { d.frame() };
            cx >= f.origin.x && cx < f.origin.x + f.size.width && cy >= f.origin.y && cy < f.origin.y + f.size.height
        });
        scale = on.or_else(|| displays.iter().next()).map_or(1.0, |d| display_pixels(&d).0 as f64 / unsafe { d.width() }.max(1) as f64);
    }
    let px = |points: f64| (((points * scale).round() as usize).max(2) + 1) & !1;
    let (w, h) = crate::output_size(px(frame.size.width) as u32, px(frame.size.height) as u32, None);
    (w as usize, h as usize)
}

/// A layer's stream settings: `w`×`h` at `fps`, NV12 as the recording's
/// own (or BGRA with its alpha: a page's), the cursor in it or not.
fn layer_config(w: usize, h: usize, fps: u32, bgra: bool, cursor: bool) -> Retained<SCStreamConfiguration> {
    unsafe {
        let config = SCStreamConfiguration::new();
        config.setWidth(w);
        config.setHeight(h);
        config.setMinimumFrameInterval(CMTime::new(1, fps.max(1) as i32));
        if bgra {
            config.setPixelFormat(kCVPixelFormatType_32BGRA);
            config.setColorSpaceName(objc2_core_graphics::kCGColorSpaceSRGB);
        } else {
            config.setPixelFormat(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange);
            config.setColorMatrix(kCGDisplayStreamYCbCrMatrix_ITU_R_709_2);
        }
        config.setShowsCursor(cursor);
        config.setScalesToFit(true);
        config.setPreservesAspectRatio(true);
        config.setIgnoreShadowsSingleWindow(true);
        config.setQueueDepth(4);
        config
    }
}

// ---------------------------------------------------------------------------
// Video files
// ---------------------------------------------------------------------------

/// A video file playing in a layer: read on a thread of its own by
/// AVFoundation (H.264, HEVC, ProRes…, in MP4, MOV or M4V), each frame up
/// when its time comes, NV12 as the compositor draws it. At its end it starts
/// over (`looped`), else its last picture stays; hidden it waits where it
/// is, and shown again goes on, or starts over (`restart`).
pub(crate) struct VideoFeed {
    pictures: Arc<Pictures>,
    /// Its pictures are BT.601 (standard definition), else BT.709.
    bt601: Arc<AtomicBool>,
    state: Arc<VideoState>,
}

#[derive(Default)]
struct VideoState {
    stop: AtomicBool,
    shown: AtomicBool,
    looped: AtomicBool,
    /// From the start: it's been shown again, with restart on.
    again: AtomicBool,
}

impl VideoFeed {
    fn start(path: &Path, looped: bool, sound: Option<String>, shown: bool) -> Self {
        let pictures = Arc::new(Pictures::default());
        let bt601 = Arc::new(AtomicBool::new(false));
        let state = Arc::new(VideoState::default());
        state.shown.store(shown, Ordering::Relaxed);
        state.looped.store(looped, Ordering::Relaxed);
        let (pictures2, bt6012, state2, path) = (pictures.clone(), bt601.clone(), state.clone(), path.to_owned());
        thread::spawn(move || {
            if let Err(e) = play_video(&path, sound, &pictures2, &bt6012, &state2) {
                eprintln!("video layer {}: {e:#}", path.display());
            }
        });
        Self { pictures, bt601, state }
    }

    /// The layer as it is now: shown or hidden, looping or not, starting
    /// over when it's shown again or not.
    fn set(&self, shown: bool, looped: bool, restart: bool) {
        let was = self.state.shown.swap(shown, Ordering::AcqRel);
        if shown && !was && restart {
            self.state.again.store(true, Ordering::Release);
        }
        self.state.looped.store(looped, Ordering::Relaxed);
    }
}

impl Drop for VideoFeed {
    fn drop(&mut self) {
        self.state.stop.store(true, Ordering::Relaxed);
    }
}

/// Play `path` into `pictures` till it's stopped (see [`VideoFeed`]), each
/// frame when its time comes; its sound into the audio source `sound`, if
/// it has one.
fn play_video(path: &Path, sound: Option<String>, pictures: &Pictures, bt601: &AtomicBool, state: &VideoState) -> Result<()> {
    let mut reader = VideoReader::open(path)?;
    // Its first sound goes with its first picture.
    let mut sound = sound.and_then(|id| match Sound::open(path, &id) {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("video layer {}: no sound ({e:#})", path.display());
            None
        }
    });
    let put = |picture: CFRetained<CVPixelBuffer>| {
        bt601.store(is_bt601(&picture), Ordering::Relaxed);
        pictures.set(picture);
    };
    // When its start was (or would have been) up: moved on by the time it's
    // been hidden, so it goes on where it was.
    let mut origin = Instant::now();
    let mut hidden_since: Option<Instant> = None;
    let mut ended = false;
    // When the last frame went up: one stamped as it was, or before it,
    // goes a frame after it.
    let mut last_due = None::<Instant>;
    let frame = Duration::from_secs_f64(1.0 / reader.fps.max(1.0));
    while !state.stop.load(Ordering::Relaxed) {
        if !state.shown.load(Ordering::Acquire) {
            hidden_since.get_or_insert_with(Instant::now);
            thread::sleep(Duration::from_millis(20));
            continue;
        }
        if let Some(t) = hidden_since.take() {
            origin += t.elapsed();
        }
        if state.again.swap(false, Ordering::AcqRel) || (ended && state.looped.load(Ordering::Relaxed)) {
            ended = false;
            reader = VideoReader::open(path)?;
            origin = Instant::now();
            last_due = Some(origin);
            if let Some(s) = sound.as_mut() {
                s.rewind();
            }
            if let Some((_, picture)) = reader.next()? {
                put(picture);
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
        let Some((at, picture)) = reader.next()? else {
            ended = true;
            continue;
        };
        let due = match last_due {
            Some(last) if origin + at <= last => last + frame,
            _ => origin + at,
        };
        last_due = Some(due);
        loop {
            let now = Instant::now();
            if now >= due || state.stop.load(Ordering::Relaxed) || state.again.load(Ordering::Relaxed) || !state.shown.load(Ordering::Relaxed) {
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
            origin = now.checked_sub(at).unwrap_or(now);
        }
        put(picture);
    }
    Ok(())
}

/// Whether a picture says its colours are BT.601's.
fn is_bt601(picture: &CVPixelBuffer) -> bool {
    let Some(matrix) = (unsafe { picture.attachment(kCVImageBufferYCbCrMatrixKey, std::ptr::null_mut()) }) else { return false };
    matrix.downcast::<CFString>().is_ok_and(|m| *m == *unsafe { kCVImageBufferYCbCrMatrix_ITU_R_601_4 })
}

/// A file's first track of `media` (video or sound), with its asset.
fn track_of(path: &Path, media: &NSString) -> Result<(Retained<AVURLAsset>, Retained<AVAssetTrack>)> {
    let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
    let asset = unsafe { AVURLAsset::URLAssetWithURL_options(&url, None) };
    let (tx, rx) = std::sync::mpsc::channel();
    let handler = block2::RcBlock::new(move |tracks: *mut NSArray<AVAssetTrack>, err: *mut NSError| {
        let first = unsafe { tracks.as_ref() }.and_then(|t| t.firstObject());
        struct Send<T>(T);
        unsafe impl<T> std::marker::Send for Send<T> {}
        let _ = tx.send(first.map(Send).ok_or_else(|| unsafe { err.as_ref() }.map_or_else(|| "it has none".to_owned(), |e| e.localizedDescription().to_string())));
    });
    unsafe { asset.loadTracksWithMediaType_completionHandler(media, &handler) };
    let track = rx.recv_timeout(Duration::from_secs(10)).context("the file didn't open")?.map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok((asset, track.0))
}

/// A reader of `track` (in `asset`) giving its samples as `settings` says.
fn reader_of(asset: &AVURLAsset, track: &AVAssetTrack, settings: &NSDictionary<NSString, objc2::runtime::AnyObject>) -> Result<(Retained<AVAssetReader>, Retained<AVAssetReaderTrackOutput>)> {
    let reader = unsafe { AVAssetReader::assetReaderWithAsset_error(asset) }.map_err(|e| anyhow::anyhow!("it can't be read: {}", e.localizedDescription()))?;
    let output = unsafe { AVAssetReaderTrackOutput::assetReaderTrackOutputWithTrack_outputSettings(track, Some(settings)) };
    unsafe { output.setAlwaysCopiesSampleData(false) };
    if !unsafe { reader.canAddOutput(&output) } {
        bail!("it can't be decoded");
    }
    unsafe { reader.addOutput(&output) };
    if !unsafe { reader.startReading() } {
        let why = unsafe { reader.error() }.map_or_else(|| "it can't be decoded".to_owned(), |e| e.localizedDescription().to_string());
        bail!("{why}");
    }
    Ok((reader, output))
}

/// A CoreFoundation string constant as an NSString key (toll-free bridged).
fn cf_key(key: &CFString) -> Retained<NSString> {
    unsafe { Retained::retain(key as *const CFString as *mut NSString) }.expect("a string constant")
}

/// A video file's pictures, in order, NV12 in IOSurfaces.
struct VideoReader {
    reader: Retained<AVAssetReader>,
    output: Retained<AVAssetReaderTrackOutput>,
    /// Its first picture's time, which its times count from.
    first: Option<f64>,
    /// Its frame rate, as its track says (30 when it doesn't).
    fps: f64,
}

// Used from one thread at a time (the video layer's).
unsafe impl Send for VideoReader {}

impl VideoReader {
    fn open(path: &Path) -> Result<Self> {
        let (asset, track) = track_of(path, unsafe { AVMediaTypeVideo }.context("no video media type")?).context("it has no video")?;
        let fps = unsafe { track.nominalFrameRate() } as f64;
        let settings = NSDictionary::<NSString, objc2::runtime::AnyObject>::from_retained_objects(
            &[&*cf_key(unsafe { kCVPixelBufferPixelFormatTypeKey }), &*cf_key(unsafe { kCVPixelBufferIOSurfacePropertiesKey })],
            &[
                Retained::into_super(Retained::into_super(Retained::into_super(NSNumber::new_u32(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange)))),
                Retained::into_super(Retained::into_super(NSDictionary::<NSString, objc2::runtime::AnyObject>::new())),
            ],
        );
        let (reader, output) = reader_of(&asset, &track, &settings)?;
        Ok(Self { reader, output, first: None, fps: if fps.is_finite() && fps > 1.0 { fps } else { 30.0 } })
    }

    /// The next picture: when it shows (after the first), and its pixels.
    /// `None` at its end.
    fn next(&mut self) -> Result<Option<(Duration, CFRetained<CVPixelBuffer>)>> {
        loop {
            let Some(sample) = (unsafe { self.output.copyNextSampleBuffer() }) else {
                if unsafe { self.reader.status() } == AVAssetReaderStatus::Failed {
                    let why = unsafe { self.reader.error() }.map_or_else(|| "it couldn't be read on".to_owned(), |e| e.localizedDescription().to_string());
                    bail!("{why}");
                }
                return Ok(None);
            };
            // A sample without a picture (an empty edit): on to the next.
            let Some(picture) = (unsafe { sample.image_buffer() }) else { continue };
            let t = unsafe { sample.presentation_time_stamp().seconds() };
            let t = if t.is_finite() { t } else { 0.0 };
            let first = *self.first.get_or_insert(t);
            return Ok(Some((Duration::from_secs_f64((t - first).max(0.0)), picture)));
        }
    }
}

/// A video file's picture size, if AVFoundation plays it (its first
/// picture's, decoded).
pub(crate) fn video_size(path: &Path) -> Option<(u32, u32)> {
    let mut reader = VideoReader::open(path).ok()?;
    let (_, picture) = reader.next().ok()??;
    let (w, h) = size_of(&picture);
    Some((w as u32, h as u32))
}

/// A video file's sound, read in order a block at a time, as float samples
/// at its own rate and channels (the mixer's feed converts them). Never all
/// of it at once: a long video's would fill memory.
struct SoundReader {
    reader: Retained<AVAssetReader>,
    output: Retained<AVAssetReaderTrackOutput>,
    rate: u32,
    channels: u32,
    /// Its first block's time, which its times count from (so its first
    /// sound goes with the video's first picture).
    first: Option<f64>,
    /// A block read to learn its rate, not handed over yet.
    read: Option<(f64, Vec<f32>)>,
}

unsafe impl Send for SoundReader {}

impl SoundReader {
    fn open(path: &Path) -> Result<Self> {
        let (asset, track) = track_of(path, unsafe { AVMediaTypeAudio }.context("no audio media type")?).context("it has no sound")?;
        // Linear PCM, 32-bit float, interleaved (AVFAudio's keys are their names).
        let key = |k: &str| NSString::from_str(k);
        let keys = [key("AVFormatIDKey"), key("AVLinearPCMBitDepthKey"), key("AVLinearPCMIsFloatKey"), key("AVLinearPCMIsNonInterleaved"), key("AVLinearPCMIsBigEndianKey")];
        let values: [Retained<objc2::runtime::AnyObject>; 5] = [
            Retained::into_super(Retained::into_super(Retained::into_super(NSNumber::new_u32(u32::from_be_bytes(*b"lpcm"))))),
            Retained::into_super(Retained::into_super(Retained::into_super(NSNumber::new_u32(32)))),
            Retained::into_super(Retained::into_super(Retained::into_super(NSNumber::new_bool(true)))),
            Retained::into_super(Retained::into_super(Retained::into_super(NSNumber::new_bool(false)))),
            Retained::into_super(Retained::into_super(Retained::into_super(NSNumber::new_bool(false)))),
        ];
        let key_refs: Vec<&NSString> = keys.iter().map(|k| &**k).collect();
        let settings = NSDictionary::<NSString, objc2::runtime::AnyObject>::from_retained_objects(&key_refs, &values);
        let (reader, output) = reader_of(&asset, &track, &settings)?;
        let mut me = Self { reader, output, rate: 0, channels: 0, first: None, read: None };
        me.read = me.read_block()?;
        anyhow::ensure!(me.rate > 0 && me.channels > 0, "its sound has no rate or channels");
        Ok(me)
    }

    /// A block from the reader: its time (after the first), its samples,
    /// interleaved; its rate and channels noted.
    fn read_block(&mut self) -> Result<Option<(f64, Vec<f32>)>> {
        loop {
            let Some(sample) = (unsafe { self.output.copyNextSampleBuffer() }) else {
                if unsafe { self.reader.status() } == AVAssetReaderStatus::Failed {
                    bail!("its sound couldn't be read on");
                }
                return Ok(None);
            };
            let Some((pcm, channels)) = crate::sck::interleaved_pcm(&sample) else { continue };
            if let Some(desc) = unsafe { sample.format_description() } {
                let asbd = unsafe { &*objc2_core_media::CMAudioFormatDescriptionGetStreamBasicDescription(&desc) };
                self.rate = asbd.mSampleRate.round() as u32;
            }
            self.channels = channels as u32;
            let t = unsafe { sample.presentation_time_stamp().seconds() };
            let t = if t.is_finite() { t } else { 0.0 };
            let first = *self.first.get_or_insert(t);
            return Ok(Some(((t - first).max(0.0), pcm)));
        }
    }

    /// The next block of sound: when it starts (seconds after the first),
    /// and its samples, interleaved. `None` at its end.
    fn next(&mut self) -> Result<Option<(f64, Vec<f32>)>> {
        match self.read.take() {
            Some(b) => Ok(Some(b)),
            None => self.read_block(),
        }
    }
}

/// A video layer's sound: read from its file as it plays, each block handed
/// to the mix a moment before it's heard (as on Windows).
struct Sound {
    path: PathBuf,
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
        let playing = crate::sources::video_sound::Playing::new(id, reader.rate);
        Ok(Self { path: path.to_owned(), reader, playing, held: None, ended: false })
    }

    /// Back to its start, with the picture: read again from the top.
    fn rewind(&mut self) {
        self.held = None;
        self.ended = false;
        match SoundReader::open(&self.path) {
            Ok(r) => self.reader = r,
            Err(e) => {
                eprintln!("video sound: {e:#}");
                self.ended = true;
            }
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
                    Ok(Some(b)) => b,
                    Ok(None) => {
                        self.ended = true;
                        return;
                    }
                    Err(e) => {
                        eprintln!("video sound: {e:#}");
                        self.ended = true;
                        return;
                    }
                },
            };
            if block.0 > played + SOUND_AHEAD {
                self.held = Some(block);
                return;
            }
            self.playing.push_at(host_origin + block.0, &block.1, self.reader.channels as usize);
        }
    }
}

/// App `app`'s window's size, in pixels, if it's open: what its layer
/// captures. From the window server (cheap: the UI asks every few seconds).
pub(crate) fn app_window_size(app: &str) -> Option<(u32, u32)> {
    let (w, h) = windows::find_app_window(app, &windows::windows(false))?.pixels();
    let px = |v: f64| (((v.round() as u32).max(2)) + 1) & !1;
    Some(crate::output_size(px(w), px(h), None))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A recording with every kind of layer over the screen, each in a box
    /// of its own along the top and middle of the frame: a colour, a picture
    /// half see-through, an animated one, a video file playing (its sound in
    /// the clip's), another display, an app's window (the terminal's) and a
    /// webcam. In the file each box shows what it should, and the clip has
    /// the video's sound. Needs ffmpeg, Screen Recording, and a camera.
    #[test]
    #[ignore = "records the screen, a window and a camera: cargo test -p capture every_kind_of_layer -- --ignored --nocapture"]
    fn every_kind_of_layer() {
        use crate::layers::{Animation, Layer, LayerSource, Picture};
        use crate::sources::{AudioSource, SourceKind};
        use crate::{EncodeSettings, Framing, Mode, Recorder, ScreenPlacement, VideoSource};
        let dir = std::env::temp_dir().join(format!("hc-every-layer-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A blue video with a tone.
        let video = dir.join("blue.mp4");
        let made = crate::stream::ffmpeg()
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "color=c=0x0000ff:s=320x180:r=30:d=3", "-f", "lavfi", "-i", "sine=f=440:d=3", "-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac", "-shortest"])
            .arg(&video)
            .status()
            .unwrap();
        assert!(made.success());
        assert_eq!(video_size(&video), Some((320, 180)));
        let still = |w: u32, h: u32, bgra: [u8; 4]| crate::StillImage { width: w, height: h, bgra: bgra.repeat((w * h) as usize) };
        let at = |x: f32, y: f32| ScreenPlacement { rect: [x, y, 0.2, 0.2], ..ScreenPlacement::whole(Framing::Stretch) };
        let screen = crate::list_screens().into_iter().next().expect("a screen");
        let (_, front) = windows::frontmost().expect("an app in front");
        let camera = crate::webcam::list_cameras().into_iter().next().expect("a camera");
        crate::webcam::keep_open(vec![(camera.id.clone(), None)]);
        let list = vec![
            Layer { id: 0, source: LayerSource::Main },
            Layer { id: 1, source: LayerSource::Colour { rgba: [255, 0, 255, 255], placement: at(0.0, 0.0) } },
            Layer { id: 2, source: LayerSource::Picture { picture: Picture(Arc::new(still(64, 64, [0, 255, 0, 128]))), placement: at(0.25, 0.0) } },
            Layer { id: 3, source: LayerSource::Animation { animation: Animation(Arc::new(vec![(still(32, 32, [0, 0, 255, 255]), Duration::from_millis(100)), (still(32, 32, [0, 255, 255, 255]), Duration::from_millis(100))])), placement: at(0.5, 0.0) } },
            Layer { id: 4, source: LayerSource::Video { path: video.clone(), width: 320, height: 180, fps: 30.0, looped: true, restart: true, sound: Some("video".into()), placement: at(0.75, 0.0) } },
            Layer { id: 5, source: LayerSource::Display { id: screen.id.clone(), placement: at(0.0, 0.5) } },
            Layer { id: 6, source: LayerSource::Window { app: front.clone(), placement: at(0.3, 0.5) } },
            Layer { id: 7, source: LayerSource::Camera { device: camera.id.clone(), format: None, placement: crate::webcam::Placement { x: 0.6, y: 0.5, w: 0.2, h: 0.2, crop: [0.0; 4], flip_h: false, flip_v: false, turns: 0, stretch: 1.0 } } },
        ];
        let settings = EncodeSettings {
            output_dir: dir.clone(),
            container_ext: "mp4".into(),
            fps: 30,
            blend_samples: 1,
            blend_shape: Default::default(),
            video_bitrate_kbps: 8000,
            size: crate::VideoSize { short_side: Some(720), shape: None, exact: None },
            use_hardware: true,
            replay_seconds: 10,
            video: VideoSource::Screen { id: screen.id.clone(), hook: Default::default(), method: Default::default() },
            away_screen: None,
            webcam: None,
            layers: Arc::new(Mutex::new(list)),
            screen: Arc::new(Mutex::new(ScreenPlacement::whole(Framing::Fit))),
            sources: vec![AudioSource { id: "video".into(), name: "Video".into(), kind: SourceKind::Video { id: "video".into() }, in_mix: true, own_track: false }],
            stream: None,
        };
        let mut rec = crate::sck::SckRecorder::new(crate::mixer::LiveAudio::new());
        rec.start(Mode::Record, &settings).unwrap();
        thread::sleep(Duration::from_secs(3));
        // How bright the camera's own picture is (a dark room's is dark).
        let seen = crate::webcam::camera_picture(&camera.id, 64, 0).expect("the camera's picture");
        let camera_level = seen.rgba.chunks_exact(4).map(|p| (p[0] as f64 + p[1] as f64 + p[2] as f64) / 3.0).sum::<f64>() / (seen.rgba.len() / 4) as f64;
        thread::sleep(Duration::from_secs(1));
        let file = rec.stop(Mode::Record, None).unwrap().expect("a recording");
        crate::webcam::keep_open(Vec::new());
        // Its picture 3 s in, as RGB.
        let (w, h) = crate::frame_size(&settings.video, settings.size).unwrap();
        let out = crate::stream::ffmpeg().args(["-v", "error", "-ss", "3", "-i"]).arg(&file).args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "rgb24", "-"]).output().unwrap();
        assert_eq!(out.stdout.len(), (w * h * 3) as usize, "a frame {w}×{h}");
        let px = |fx: f32, fy: f32| {
            let (x, y) = ((fx * w as f32) as usize, (fy * h as f32) as usize);
            let i = (y * w as usize + x) * 3;
            [out.stdout[i], out.stdout[i + 1], out.stdout[i + 2]]
        };
        let mid = |x: f32, y: f32| px(x + 0.1, y + 0.1);
        eprintln!("colour {:?} picture {:?} animation {:?} video {:?} display {:?} window {:?} camera {:?}", mid(0.0, 0.0), mid(0.25, 0.0), mid(0.5, 0.0), mid(0.75, 0.0), mid(0.0, 0.5), mid(0.3, 0.5), mid(0.6, 0.5));
        let [r, g, b] = mid(0.0, 0.0);
        assert!(r > 200 && g < 60 && b > 200, "the colour's magenta");
        let [r, g, b] = mid(0.25, 0.0);
        assert!(g as i32 > r as i32 + 40 && g as i32 > b as i32 + 40, "half green over the screen");
        let [r, g, b] = mid(0.5, 0.0);
        assert!(r > 200 && b < 60, "the animation's red or yellow: {:?}", [r, g, b]);
        let [r, g, b] = mid(0.75, 0.0);
        assert!(b > 180 && r < 70 && g < 70, "the video's blue");
        // What a display and a window show isn't known: not black. The
        // camera's box is about as bright as its own picture.
        let spots = |x: f32, y: f32| (0..5).flat_map(move |i| (0..5).map(move |j| (x + 0.02 + i as f32 * 0.04, y + 0.02 + j as f32 * 0.04)));
        let lit = |x: f32, y: f32| spots(x, y).any(|(sx, sy)| px(sx, sy).iter().any(|c| *c > 24));
        assert!(lit(0.0, 0.5), "the display shows");
        assert!(lit(0.3, 0.5), "the window shows");
        let level = spots(0.6, 0.5).map(|(sx, sy)| px(sx, sy).iter().map(|c| *c as f64).sum::<f64>() / 3.0).sum::<f64>() / 25.0;
        eprintln!("the camera's box {level:.0}, its own picture {camera_level:.0}");
        assert!((level - camera_level).abs() < 25.0, "the camera's picture in its box: {level:.0}, its own {camera_level:.0}");
        // The video's tone is in the clip's sound.
        let vol = std::process::Command::new("ffmpeg").args(["-v", "info", "-i"]).arg(&file).args(["-af", "volumedetect", "-f", "null", "-"]).output().unwrap();
        let text = String::from_utf8_lossy(&vol.stderr);
        let mean = text.lines().find_map(|l| l.split("mean_volume:").nth(1)).and_then(|v| v.trim().trim_end_matches(" dB").parse::<f64>().ok()).expect("its sound's level");
        eprintln!("the clip's sound: {mean} dB");
        assert!(mean > -40.0, "the video's tone: {mean} dB");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Each frame shows for its time, then the next; after the last, the
    /// first again.
    #[test]
    fn an_animation_shows_each_frame_for_its_time() {
        let still = || crate::StillImage { width: 1, height: 1, bgra: vec![0; 4] };
        let frames = [(still(), Duration::from_millis(100)), (still(), Duration::from_millis(50)), (still(), Duration::from_millis(200))];
        let at = |ms: u64| frame_at(&frames, Duration::from_millis(ms));
        assert_eq!([at(0), at(99), at(100), at(149), at(150), at(349), at(350), at(460)], [0, 0, 1, 1, 2, 2, 0, 1]);
        assert_eq!(frame_at(&frames[..1], Duration::from_secs(9)), 0);
    }
}
