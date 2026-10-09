//! What a recording shows, on macOS: one ScreenCaptureKit stream of a
//! display, or of the window of whichever listed game or app you're using,
//! scaled into the frame by ScreenCaptureKit itself (letterboxed when the
//! shapes differ), as NV12. Following apps never restarts the stream: when
//! the window to record changes, its filter is swapped (`updateContentFilter`).
//!
//! The pacer asks for each frame to encode ([`Frames::next`]): the stream's
//! newest buffer, or the away screen, as is; with a webcam, the compositor
//! (`super::gpu`) draws it over into a buffer of its own. The same buffer
//! feeds the Sources page's preview.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send};
use objc2_core_foundation::CFRetained;
use objc2_core_graphics::{CGColor, CGDisplayCopyDisplayMode, CGDisplayMode, CGMainDisplayID, kCGDisplayStreamYCbCrMatrix_ITU_R_709_2};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight, CVPixelBufferGetHeightOfPlane,
    CVPixelBufferGetWidth, CVPixelBufferGetWidthOfPlane, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamOutput, SCStreamOutputType, SCWindow,
};

use super::camera::CameraLayer;
use super::gpu::{FramePool, Gpu};
use super::windows;
use crate::webcam::{SharedPlacement, Webcam};
use crate::{StillImage, VideoSource};

/// A CoreVideo buffer moved between threads (they're thread-safe reference
/// counted objects; nothing writes to one after it's handed over).
pub(crate) struct Buffer(pub CFRetained<CVPixelBuffer>);
unsafe impl Send for Buffer {}
unsafe impl Sync for Buffer {}

/// The newest picture, and what it shows.
#[derive(Default)]
pub(crate) struct Latest {
    frame: Mutex<Option<Arc<Buffer>>>,
    /// The game or app being recorded (its id), when following apps.
    app: Mutex<Option<String>>,
    /// Nothing's being recorded: the away screen (or black) shows.
    waiting: AtomicBool,
}

impl Latest {
    fn set(&self, b: CFRetained<CVPixelBuffer>) {
        *self.frame.lock().unwrap() = Some(Arc::new(Buffer(b)));
    }

    pub(crate) fn get(&self) -> Option<Arc<Buffer>> {
        self.frame.lock().unwrap().clone()
    }
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSObject))]
    #[ivars = Arc<Output>]
    struct ScreenOutput;

    unsafe impl NSObjectProtocol for ScreenOutput {}

    unsafe impl SCStreamOutput for ScreenOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output(&self, _stream: &SCStream, sample: &CMSampleBuffer, kind: SCStreamOutputType) {
            let out = self.ivars();
            // Idle/blank status frames carry no image: keep the previous one.
            if kind == SCStreamOutputType::Screen && !out.paused.load(Ordering::Relaxed) {
                if let Some(image) = unsafe { sample.image_buffer() } {
                    out.latest.set(image);
                    out.frames.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }
);

/// Where a stream's frames go.
struct Output {
    latest: Arc<Latest>,
    /// The away screen is up: frames still arriving from the window are ignored.
    paused: AtomicBool,
    frames: AtomicU64,
}

impl ScreenOutput {
    fn new(out: Arc<Output>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(out);
        unsafe { msg_send![super(this), init] }
    }
}

/// Run an SCStream start/stop/update call and wait for its completion handler.
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

/// ScreenCaptureKit's view of what can be captured.
pub(crate) fn shareable_content() -> Result<Retained<SCShareableContent>> {
    struct Send<T>(T);
    unsafe impl<T> std::marker::Send for Send<T> {}
    let (tx, rx) = mpsc::channel::<Result<Send<Retained<SCShareableContent>>, String>>();
    let handler = RcBlock::new(move |content: *mut SCShareableContent, err: *mut NSError| {
        let result = match unsafe { Retained::retain(content) } {
            Some(c) => Ok(Send(c)),
            None => Err(unsafe { err.as_ref() }.map(|e| e.localizedDescription().to_string()).unwrap_or_else(|| "unknown error".into())),
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
pub(crate) fn pick_display(content: &SCShareableContent, id: &str) -> Option<Retained<SCDisplay>> {
    let displays = unsafe { content.displays() };
    let by_id = |want: u32| displays.iter().find(|d| unsafe { d.displayID() } == want);
    id.parse().ok().and_then(by_id).or_else(|| by_id(CGMainDisplayID())).or_else(|| displays.iter().next())
}

/// A display's size in pixels.
fn display_pixels(display: &SCDisplay) -> (usize, usize) {
    let mode = CGDisplayCopyDisplayMode(unsafe { display.displayID() });
    let (w, h) = (CGDisplayMode::pixel_width(mode.as_deref()), CGDisplayMode::pixel_height(mode.as_deref()));
    if w == 0 || h == 0 { unsafe { (display.width() as usize, display.height() as usize) } } else { (w, h) }
}

/// What to record and the frame size: a display's own, or for games and
/// apps the main display's (so it can't change mid-file, whatever the
/// windows do).
pub(crate) fn plan(source: &VideoSource, target_height: Option<u32>) -> Result<(Retained<SCShareableContent>, Retained<SCDisplay>, (usize, usize))> {
    let content = shareable_content()?;
    let id = match source {
        VideoSource::Screen { id, .. } => id.as_str(),
        VideoSource::Apps { .. } => "",
    };
    let display = pick_display(&content, id).context("no display to capture")?;
    let (w, h) = display_pixels(&display);
    let (w, h) = crate::output_size(w as u32, h as u32, target_height);
    Ok((content, display, (w as usize, h as usize)))
}

/// One running capture: its stream, and for games and apps the thread that
/// follows them.
pub(crate) struct Picture {
    pub latest: Arc<Latest>,
    stream: Retained<SCStream>,
    _output: Retained<ScreenOutput>,
    follow: Option<(Arc<AtomicBool>, JoinHandle<()>, Arc<Mutex<AppsConfig>>)>,
}

/// The games and apps a capture follows, and what to show while you're in
/// something else.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AppsConfig {
    ids: Vec<String>,
    away_when_unfocused: bool,
}

impl AppsConfig {
    pub(crate) fn of(source: &VideoSource) -> Option<Self> {
        match source {
            VideoSource::Apps { ids, away_when_unfocused, .. } => Some(Self { ids: ids.clone(), away_when_unfocused: *away_when_unfocused }),
            VideoSource::Screen { .. } => None,
        }
    }
}

impl Picture {
    /// Start capturing `source` at `width`×`height`, `fps`. For a display,
    /// waits for its first frame (so a capture that can't see the screen
    /// fails visibly, not with an empty file).
    pub(crate) fn start(
        source: &VideoSource,
        content: &SCShareableContent,
        display: &SCDisplay,
        (width, height): (usize, usize),
        fps: u32,
        away: Option<Arc<Buffer>>,
    ) -> Result<Self> {
        let latest = Arc::new(Latest::default());
        let out = Arc::new(Output { latest: latest.clone(), paused: AtomicBool::new(false), frames: AtomicU64::new(0) });
        let output = ScreenOutput::new(out.clone());
        let config = stream_config(width, height, fps);
        // Games and apps start on the away screen (the stream shows the
        // display until a window is picked, but nothing of it is used).
        let apps = AppsConfig::of(source);
        if let Some(apps) = &apps {
            out.paused.store(true, Ordering::Relaxed);
            // Black with the away screen off.
            show_away(&latest, away.as_ref().filter(|_| apps.away_when_unfocused), width, height);
        }
        let filter = unsafe { SCContentFilter::initWithDisplay_excludingWindows(SCContentFilter::alloc(), display, &NSArray::new()) };
        let stream = unsafe { SCStream::initWithFilter_configuration_delegate(SCStream::alloc(), &filter, &config, None) };
        let queue = DispatchQueue::new("hesteclips.sck.video", None);
        unsafe { stream.addStreamOutput_type_sampleHandlerQueue_error(ProtocolObject::from_ref(&*output), SCStreamOutputType::Screen, Some(&queue)) }
            .map_err(|e| anyhow::anyhow!("can't capture the screen: {}", e.localizedDescription()))?;
        await_completion(|h| unsafe { stream.startCaptureWithCompletionHandler(Some(h)) }).map_err(|e| anyhow::anyhow!("couldn't start screen capture: {e}"))?;
        let mut picture = Self { latest, stream, _output: output, follow: None };
        match apps {
            None => {
                let deadline = Instant::now() + Duration::from_secs(3);
                while picture.latest.get().is_none() {
                    if Instant::now() >= deadline {
                        picture.stop();
                        bail!("the screen isn't delivering frames — check Screen Recording permission");
                    }
                    thread::sleep(Duration::from_millis(10));
                }
            }
            Some(config) => {
                let list = Arc::new(Mutex::new(config));
                let stop = Arc::new(AtomicBool::new(false));
                let stream = SendStream(picture.stream.clone());
                let (latest, display, list2, stop2) = (picture.latest.clone(), unsafe { display.displayID() }, list.clone(), stop.clone());
                let _ = content;
                let handle = thread::spawn(move || follow_apps(stream, out, latest, display, list2, stop2, away, (width, height)));
                picture.follow = Some((stop, handle, list));
            }
        }
        Ok(picture)
    }

    /// Follow a different list of games and apps without restarting. False
    /// if this capture doesn't follow apps.
    pub(crate) fn update(&self, source: &VideoSource) -> bool {
        match (AppsConfig::of(source), &self.follow) {
            (Some(config), Some((_, _, list))) => {
                *list.lock().unwrap() = config;
                true
            }
            _ => false,
        }
    }

    pub(crate) fn stop(mut self) {
        if let Some((stop, handle, _)) = self.follow.take() {
            stop.store(true, Ordering::Relaxed);
            let _ = handle.join();
        }
        let _ = await_completion(|h| unsafe { self.stream.stopCaptureWithCompletionHandler(Some(h)) });
    }
}

struct SendStream(Retained<SCStream>);
unsafe impl Send for SendStream {}

fn stream_config(width: usize, height: usize, fps: u32) -> Retained<SCStreamConfiguration> {
    unsafe {
        let config = SCStreamConfiguration::new();
        config.setWidth(width);
        config.setHeight(height);
        config.setMinimumFrameInterval(CMTime::new(1, fps as i32));
        config.setPixelFormat(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange);
        config.setColorMatrix(kCGDisplayStreamYCbCrMatrix_ITU_R_709_2);
        config.setShowsCursor(true);
        // A window fills the frame as far as its shape allows (up and down),
        // centred, with black around it.
        config.setScalesToFit(true);
        config.setPreservesAspectRatio(true);
        if let Some(black) = CGColor::constant_color(Some(objc2_core_graphics::kCGColorBlack)) {
            config.setBackgroundColor(&black);
        }
        // Games' windows have no shadow worth recording.
        config.setIgnoreShadowsSingleWindow(true);
        // Room for the frame we hold + frames in the encoder without starving SCK.
        config.setQueueDepth(8);
        config
    }
}

/// Put the away screen (or black) up.
fn show_away(latest: &Latest, away: Option<&Arc<Buffer>>, width: usize, height: usize) {
    let frame = match away {
        Some(b) => b.clone(),
        None => match still_buffer(None, width, height) {
            Ok(b) => Arc::new(b),
            Err(_) => return,
        },
    };
    *latest.frame.lock().unwrap() = Some(frame);
    *latest.app.lock().unwrap() = None;
    latest.waiting.store(true, Ordering::Relaxed);
}

/// How often to look at which app is in front.
const RESCAN: Duration = Duration::from_millis(100);

/// Keep the stream on the window of the listed app last in front, for as
/// long as it's showing; the away screen otherwise (see `VideoSource::Apps`).
#[allow(clippy::too_many_arguments)]
fn follow_apps(
    stream: SendStream,
    out: Arc<Output>,
    latest: Arc<Latest>,
    display_id: u32,
    list: Arc<Mutex<AppsConfig>>,
    stop: Arc<AtomicBool>,
    away: Option<Arc<Buffer>>,
    (width, height): (usize, usize),
) {
    let stream = stream.0;
    let mut config = AppsConfig::default();
    // The listed app last in front: recorded until another one is.
    let mut active: Option<String> = None;
    // The window being recorded, and its app.
    let mut current: Option<(u32, String)> = None;
    // A window that couldn't be captured, so it's reported once.
    let mut failed: Option<u32> = None;
    // Started on the away screen, or on black with it off (`held`).
    let mut away_up = list.lock().unwrap().away_when_unfocused;
    // Nothing listed is open and the away screen is off: the last picture
    // (or black) stays.
    let mut held = !away_up;
    latest.waiting.store(true, Ordering::Relaxed);
    let put_away = |away_up: &mut bool| {
        if !*away_up {
            out.paused.store(true, Ordering::Relaxed);
            show_away(&latest, away.as_ref(), width, height);
            *away_up = true;
        }
    };
    let hold = |away_up: &mut bool, held: &mut bool| {
        if !*held {
            out.paused.store(true, Ordering::Relaxed);
            if *away_up || latest.frame.lock().unwrap().is_none() {
                show_away(&latest, None, width, height);
            }
            *latest.app.lock().unwrap() = None;
            latest.waiting.store(true, Ordering::Relaxed);
            *away_up = false;
            *held = true;
        }
    };
    while !stop.load(Ordering::Relaxed) {
        let now = list.lock().unwrap().clone();
        if now != config {
            config = now;
            let still = |a: &String| config.ids.iter().any(|x| x.eq_ignore_ascii_case(a));
            if active.as_ref().is_some_and(|a| !still(a)) {
                active = None;
            }
        }
        let listed = |id: &str| config.ids.iter().find(|a| a.eq_ignore_ascii_case(id)).cloned();
        if let Some((_, id)) = windows::frontmost() {
            if let Some(app) = listed(&id) {
                active = Some(app);
            }
        }
        let all = windows::windows(false);
        // Keep the window being recorded while it's the active app's and open.
        let keep = current.as_ref().is_some_and(|(w, app)| Some(app) == active.as_ref() && all.iter().any(|x| x.id == *w));
        if !keep {
            // The active app's window, else the first listed app that's open.
            let target = active
                .as_ref()
                .and_then(|a| Some((windows::find_app_window(a, &all)?, a.clone())))
                .or_else(|| config.ids.iter().find_map(|a| Some((windows::find_app_window(a, &all)?, a.clone()))));
            let same = current.as_ref().zip(target.as_ref()).is_some_and(|((w, _), (t, _))| *w == t.id);
            if !same {
                current = None;
                match target.filter(|(w, _)| failed != Some(w.id)) {
                    None if config.away_when_unfocused => {
                        held = false;
                        put_away(&mut away_up);
                    }
                    None => hold(&mut away_up, &mut held),
                    Some((w, app)) => match record_window(&stream, w.id) {
                        Ok(()) => {
                            *latest.app.lock().unwrap() = Some(app.clone());
                            active = Some(app.clone());
                            current = Some((w.id, app));
                            // Its first frame replaces the away screen.
                            out.paused.store(false, Ordering::Relaxed);
                            latest.waiting.store(false, Ordering::Relaxed);
                            away_up = false;
                            held = false;
                        }
                        Err(e) => {
                            eprintln!("can't record {app}'s window: {e:#}");
                            failed = Some(w.id);
                        }
                    },
                }
            }
        }
        // The setting changed while nothing listed is open.
        if current.is_none() {
            if config.away_when_unfocused && held {
                held = false;
                put_away(&mut away_up);
            } else if !config.away_when_unfocused && away_up {
                hold(&mut away_up, &mut held);
            }
        }
        // Tabbed out: the recorded window isn't showing anymore (minimized,
        // hidden, on another Space, or full screen in another Space). With
        // the away screen off, the clip keeps its last picture.
        if let Some((w, app)) = &current {
            let showing = all.iter().any(|x| x.id == *w && x.on_screen);
            if !showing {
                if config.away_when_unfocused {
                    put_away(&mut away_up);
                }
            } else if away_up {
                *latest.app.lock().unwrap() = Some(app.clone());
                out.paused.store(false, Ordering::Relaxed);
                latest.waiting.store(false, Ordering::Relaxed);
                away_up = false;
            }
        }
        let until = Instant::now() + RESCAN;
        while Instant::now() < until && !stop.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(20));
        }
    }
    let _ = display_id;
}

/// Point the stream at window `id` (ScreenCaptureKit's list is fetched for it).
fn record_window(stream: &SCStream, id: u32) -> Result<()> {
    let content = shareable_content()?;
    let window: Retained<SCWindow> = unsafe { content.windows() }.iter().find(|w| unsafe { w.windowID() } == id).context("the window went away")?;
    let filter = unsafe { SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), &window) };
    await_completion(|h| unsafe { stream.updateContentFilter_completionHandler(&filter, Some(h)) }).map_err(|e| anyhow::anyhow!("{e}"))
}

/// A still picture (or black) as an NV12 buffer the size of the frame,
/// fitted with black bars, converted once: shown as is while away.
pub(crate) fn still_buffer(image: Option<&StillImage>, width: usize, height: usize) -> Result<Buffer> {
    let buf = FramePool::new(width, height)?.take()?;
    unsafe {
        if CVPixelBufferLockBaseAddress(&buf, CVPixelBufferLockFlags(0)) != 0 {
            bail!("couldn't fill a frame");
        }
        let y = CVPixelBufferGetBaseAddressOfPlane(&buf, 0) as *mut u8;
        let ys = CVPixelBufferGetBytesPerRowOfPlane(&buf, 0);
        let c = CVPixelBufferGetBaseAddressOfPlane(&buf, 1) as *mut u8;
        let cs = CVPixelBufferGetBytesPerRowOfPlane(&buf, 1);
        let (cw, ch) = (CVPixelBufferGetWidthOfPlane(&buf, 1), CVPixelBufferGetHeightOfPlane(&buf, 1));
        // Black (studio range).
        for r in 0..height {
            std::ptr::write_bytes(y.add(r * ys), 16, width);
        }
        for r in 0..ch {
            std::ptr::write_bytes(c.add(r * cs), 128, cw * 2);
        }
        if let Some(img) = image.filter(|i| i.width > 0 && i.height > 0) {
            // Fitted, centred; each output pixel takes the image's pixel under
            // it (the away screen is shown at roughly its own size).
            let (fx, fy, fw, fh) = fit(img.width as usize, img.height as usize, width, height);
            let px = |x: usize, yy: usize| {
                let sx = ((x - fx) * img.width as usize / fw).min(img.width as usize - 1);
                let sy = ((yy - fy) * img.height as usize / fh).min(img.height as usize - 1);
                let p = &img.bgra[(sy * img.width as usize + sx) * 4..][..4];
                (p[2] as f32, p[1] as f32, p[0] as f32)
            };
            for yy in fy..fy + fh {
                for x in fx..fx + fw {
                    let (r, g, b) = px(x, yy);
                    *y.add(yy * ys + x) = (16.0 + (0.2126 * r + 0.7152 * g + 0.0722 * b) * 219.0 / 255.0).round() as u8;
                }
            }
            for cy in fy / 2..(fy + fh) / 2 {
                for cx in fx / 2..(fx + fw) / 2 {
                    let (r, g, b) = px(cx * 2, cy * 2);
                    let luma = 0.2126 * r + 0.7152 * g + 0.0722 * b;
                    let cb = 128.0 + (b - luma) / 1.8556 * 224.0 / 255.0;
                    let cr = 128.0 + (r - luma) / 1.5748 * 224.0 / 255.0;
                    *c.add(cy * cs + cx * 2) = cb.round().clamp(16.0, 240.0) as u8;
                    *c.add(cy * cs + cx * 2 + 1) = cr.round().clamp(16.0, 240.0) as u8;
                }
            }
        }
        CVPixelBufferUnlockBaseAddress(&buf, CVPixelBufferLockFlags(0));
    }
    Ok(Buffer(buf))
}

/// The largest rectangle with `w`×`h`'s shape that fits in `into_w`×`into_h`,
/// centered: (x, y, width, height), even values.
fn fit(w: usize, h: usize, into_w: usize, into_h: usize) -> (usize, usize, usize, usize) {
    let (fw, fh) = if w * into_h > h * into_w { (into_w, h * into_w / w) } else { (w * into_h / h, into_h) };
    let (fw, fh) = (fw & !1, fh & !1);
    (((into_w - fw) / 2) & !1, ((into_h - fh) / 2) & !1, fw.max(2), fh.max(2))
}

/// Turns the picture (and the webcam over it) into frames to encode, and
/// feeds the preview.
pub(crate) struct Frames {
    latest: Arc<Latest>,
    camera: Option<(CameraLayer, SharedPlacement, Gpu, FramePool)>,
    /// The last frame made, and what it was made from (screen frame, camera
    /// frame, placement), to repeat it when nothing changed.
    last: Option<(Arc<Buffer>, usize, u64, crate::webcam::Placement)>,
    preview: crate::mac::preview::Producer,
}

impl Frames {
    pub(crate) fn new(latest: Arc<Latest>, webcam: Option<&Webcam>, width: usize, height: usize, generation: u64) -> Result<Self> {
        let camera = match webcam {
            Some(w) => Some((CameraLayer::new(w), w.placement.clone(), Gpu::new()?, FramePool::new(width, height)?)),
            None => None,
        };
        Ok(Self { latest, camera, last: None, preview: crate::mac::preview::Producer::new(generation) })
    }

    /// The frame to encode now, if there's a picture yet.
    pub(crate) fn next(&mut self) -> Option<Arc<Buffer>> {
        let screen = self.latest.get()?;
        let frame = match &mut self.camera {
            None => screen,
            Some((layer, placement, gpu, pool)) => {
                let p = *placement.lock().unwrap();
                let cam = if p.is_hidden() { None } else { layer.latest() };
                // No webcam showing: the screen's own buffer, as without one.
                if cam.is_none() {
                    self.last = None;
                    self.preview.offer(&screen, self.latest.waiting.load(Ordering::Relaxed), self.latest.app.lock().unwrap().clone());
                    return Some(screen);
                }
                let key = (Arc::as_ptr(&screen) as usize, cam.as_ref().map_or(0, |c| c.seq), p);
                match &self.last {
                    // Nothing changed: the same frame again (the encoder holds it anyway).
                    Some((b, s, c, lp)) if (*s, *c, *lp) == key => b.clone(),
                    _ => match pool.take().and_then(|out| gpu.compose(&screen.0, cam.as_ref().map(|c| &*c.buffer), p, &out).map(|()| out)) {
                        Ok(out) => {
                            let b = Arc::new(Buffer(out));
                            self.last = Some((b.clone(), key.0, key.1, key.2));
                            b
                        }
                        Err(e) => {
                            eprintln!("webcam: {e:#}");
                            screen
                        }
                    },
                }
            }
        };
        self.preview.offer(&frame, self.latest.waiting.load(Ordering::Relaxed), self.latest.app.lock().unwrap().clone());
        Some(frame)
    }
}

/// For tests and benches: a buffer's size.
#[allow(dead_code)]
pub(crate) fn size_of(b: &CVPixelBuffer) -> (usize, usize) {
    (CVPixelBufferGetWidth(b), CVPixelBufferGetHeight(b))
}
