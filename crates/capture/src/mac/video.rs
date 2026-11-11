//! What a recording shows, on macOS: one ScreenCaptureKit stream of a
//! display, or of the window of whichever listed game or app you're using,
//! scaled into the frame by ScreenCaptureKit itself (letterboxed when the
//! shapes differ), as NV12. Following apps never restarts the stream: when
//! the window to record changes, its filter is swapped (`updateContentFilter`).
//!
//! The pacer asks for each frame to encode ([`Frames::next`]): the stream's
//! newest buffer, or the away screen, as is; with other layers showing
//! (webcams, displays, windows, pictures, words, pages, videos: see
//! `super::layers`), or the screen in a box of its own, the compositor
//! (`super::gpu`) makes it in a buffer of its own. The same buffer feeds the
//! Sources page's preview.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send};
use objc2_core_foundation::{CFRetained, CGRect};
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

use super::gpu::{Compositor, Draw, FramePool};
use super::layers::Feeds;
use super::windows;
use crate::layers::{LayerSource, SharedLayers};
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
    /// For frame blending, which needs every picture and when it came up,
    /// not just the newest.
    arrivals: Mutex<Arrivals>,
    arrived: Condvar,
    /// Where the newest pictures have what they show, by picture: they're
    /// made into frames a little after they come up.
    drawn: Mutex<VecDeque<(Weak<Buffer>, Drawn)>>,
}

/// Pictures as they come up, numbered: `seq` counts every change of the
/// picture (the away screen too, which isn't queued), so a change without
/// an arrival shows as a number the queue never had.
#[derive(Default)]
struct Arrivals {
    seq: u64,
    /// `None` until blending asks for them.
    queue: Option<VecDeque<Arrival>>,
}

/// A picture from the stream and the host time it was shown on the screen.
pub(crate) struct Arrival {
    pub at: f64,
    pub seq: u64,
    pub buffer: Arc<Buffer>,
}

/// Pictures kept for blending before the oldest go (each holds one of the
/// stream's buffers; it has 8). The blending takes them within a millisecond
/// or so; this is for a stall.
const MAX_QUEUED: usize = 4;

impl Latest {
    fn set(&self, b: CFRetained<CVPixelBuffer>, at: f64, drawn: Option<Drawn>) {
        let b = Arc::new(Buffer(b));
        if let Some(d) = drawn {
            self.note(&b, d);
        }
        *self.frame.lock().unwrap() = Some(b.clone());
        let mut a = self.arrivals.lock().unwrap();
        a.seq += 1;
        let seq = a.seq;
        if let Some(q) = a.queue.as_mut() {
            if q.len() >= MAX_QUEUED {
                // Frame blending fell behind: that picture missed its frame.
                q.pop_front();
                crate::count_missed(1);
            }
            q.push_back(Arrival { at, seq, buffer: b });
        }
        drop(a);
        self.arrived.notify_all();
    }

    /// The picture was put up some other way (the away screen): all of it
    /// shows.
    fn put(&self, b: Arc<Buffer>) {
        let (w, h) = size_of(&b.0);
        self.note(&b, Drawn { at: [0, 0, w as u32, h as u32], shows: None });
        *self.frame.lock().unwrap() = Some(b);
        self.arrivals.lock().unwrap().seq += 1;
        self.arrived.notify_all();
    }

    /// Where `b` has what it shows, kept for the newest few pictures (as
    /// many as the stream has buffers), without holding on to them.
    fn note(&self, b: &Arc<Buffer>, d: Drawn) {
        let mut kept = self.drawn.lock().unwrap();
        if kept.len() >= 8 {
            kept.pop_front();
        }
        kept.push_back((Arc::downgrade(b), d));
    }

    /// Where picture `b` has what it shows, when it's one of the newest (a
    /// blend of several isn't).
    fn drawn(&self, b: &Arc<Buffer>) -> Option<Drawn> {
        self.drawn.lock().unwrap().iter().rev().find(|(w, _)| std::ptr::eq(w.as_ptr(), Arc::as_ptr(b))).map(|&(_, d)| d)
    }

    pub(crate) fn get(&self) -> Option<Arc<Buffer>> {
        self.frame.lock().unwrap().clone()
    }

    /// The game or app being recorded, when following them and one shows.
    pub(crate) fn recorded_app(&self) -> Option<String> {
        self.app.lock().unwrap().clone()
    }

    /// Nothing's being recorded: the away screen (or black) is up.
    pub(crate) fn is_waiting(&self) -> bool {
        self.waiting.load(Ordering::Relaxed)
    }

    /// Keep every picture from now on, for [`Self::drain`].
    pub(crate) fn queue_arrivals(&self) {
        self.arrivals.lock().unwrap().queue.get_or_insert_with(VecDeque::new);
    }

    /// The number of the newest change of the picture.
    pub(crate) fn seq(&self) -> u64 {
        self.arrivals.lock().unwrap().seq
    }

    /// Wait up to `timeout` for a change after `seen`.
    pub(crate) fn wait(&self, seen: u64, timeout: Duration) {
        let a = self.arrivals.lock().unwrap();
        let _ = self.arrived.wait_timeout_while(a, timeout, |a| a.seq <= seen);
    }

    /// The pictures that came up since the last call, oldest first.
    pub(crate) fn drain(&self) -> Vec<Arrival> {
        self.arrivals.lock().unwrap().queue.as_mut().map_or_else(Vec::new, |q| q.drain(..).collect())
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
                    // On the host clock, when the screen showed it.
                    let at = unsafe { sample.presentation_time_stamp().seconds() };
                    let drawn = frame_info(sample).and_then(|(rect, scale, pixels)| drawn(rect, scale, pixels, size_of(&image)));
                    // A new size (a window resized) wakes the thread following
                    // games and apps, to set the stream up for it.
                    if let Some(size) = drawn.and_then(|d| d.shows) {
                        let mut shows = out.shows.lock().unwrap();
                        if *shows != Some(size) {
                            *shows = Some(size);
                            out.resized.notify_all();
                        }
                    }
                    out.latest.set(image, at, drawn);
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
    /// The size (points) of what the frames show, as the newest one says.
    shows: Mutex<Option<(u32, u32)>>,
    /// It changed.
    resized: Condvar,
}

/// The size in points of what a frame shows (a window), from the size it
/// was drawn at in the frame and how much it was scaled to be (see
/// [`frame_info`]).
fn shown_size((w, h): (f64, f64), scale: f64) -> Option<(u32, u32)> {
    (scale > 0.0 && w > 0.0 && h > 0.0).then(|| ((w / scale).round() as u32, (h / scale).round() as u32))
}

/// Where a picture has what it shows: for a frame from the stream, the part
/// of it ScreenCaptureKit drew in (in pixels) and the size of what it drew
/// (a window, in points); for a still (the away screen), all of it.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Drawn {
    at: [u32; 4],
    shows: Option<(u32, u32)>,
}

/// Where a frame `frame` pixels big has what it shows (see [`frame_info`]):
/// drawn in `rect` (the frame's points, `pixels` pixels each), scaled by
/// `scale` to be that big.
fn drawn(rect: CGRect, scale: f64, pixels: f64, (fw, fh): (usize, usize)) -> Option<Drawn> {
    let (fw, fh) = (fw as u32, fh as u32);
    let px = |v: f64| (v * pixels).round().max(0.0) as u32;
    let (x, y) = (px(rect.origin.x).min(fw), px(rect.origin.y).min(fh));
    let (w, h) = (px(rect.size.width).min(fw - x), px(rect.size.height).min(fh - y));
    let shows = shown_size((rect.size.width, rect.size.height), scale)?;
    (w > 0 && h > 0).then_some(Drawn { at: [x, y, w, h], shows: Some(shows) })
}

/// What ScreenCaptureKit says about a frame (its attachments): where in the
/// frame what's recorded was drawn, how much it was scaled to go there, and
/// the display's pixels a point.
fn frame_info(sample: &CMSampleBuffer) -> Option<(CGRect, f64, f64)> {
    use objc2_core_foundation::{CFArray, CFDictionary, CFNumber, CFString, CFType};
    use objc2_screen_capture_kit::{SCStreamFrameInfoContentRect, SCStreamFrameInfoContentScale, SCStreamFrameInfoScaleFactor};
    let all = unsafe { sample.sample_attachments_array(false) }?;
    // One dictionary per sample, keyed by strings.
    let all = unsafe { CFRetained::cast_unchecked::<CFArray<CFDictionary<CFString, CFType>>>(all) };
    let info = all.get(0)?;
    let get = |key: &objc2_foundation::NSString| info.get(unsafe { &*(key as *const objc2_foundation::NSString).cast::<CFString>() });
    let rect = get(unsafe { SCStreamFrameInfoContentRect })?.downcast::<CFDictionary>().ok()?;
    let mut r = CGRect::default();
    if !unsafe { objc2_core_graphics::CGRectMakeWithDictionaryRepresentation(Some(&rect), &mut r) } {
        return None;
    }
    let number = |key| get(key)?.downcast::<CFNumber>().ok()?.as_cgfloat();
    Some((r, number(unsafe { SCStreamFrameInfoContentScale })?, number(unsafe { SCStreamFrameInfoScaleFactor })?))
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

/// A display's size in pixels (on a Retina display, twice its size in
/// points each way: what's recorded).
pub(crate) fn display_pixels(display: &SCDisplay) -> (usize, usize) {
    let mode = CGDisplayCopyDisplayMode(unsafe { display.displayID() });
    let (w, h) = (CGDisplayMode::pixel_width(mode.as_deref()), CGDisplayMode::pixel_height(mode.as_deref()));
    if w == 0 || h == 0 { unsafe { (display.width() as usize, display.height() as usize) } } else { (w, h) }
}

/// What to record and the frame size: a display's own (in `shape`, if
/// there is one), or for games and apps the main display's (so it can't
/// change mid-file, whatever the windows do). A crop doesn't change it.
pub(crate) fn plan(source: &VideoSource, size: crate::VideoSize) -> Result<(Retained<SCShareableContent>, Retained<SCDisplay>, (usize, usize))> {
    let content = shareable_content()?;
    let id = match source {
        VideoSource::Screen { id, .. } => id.as_str(),
        VideoSource::Apps { .. } => "",
    };
    let display = pick_display(&content, id).context("no display to capture")?;
    let (w, h) = display_pixels(&display);
    let (w, h) = size.of(w as u32, h as u32);
    Ok((content, display, (w as usize, h as usize)))
}

/// How the stream puts what it records into the frame for the screen placed
/// as `placed` says, and the part of a display it records (`display`
/// pixels big, while a display's recorded): filling the frame, as the
/// placement says; in a box covering the frame, the part of it showing,
/// over all of the frame (see [`showing`]); in any other box, all of it
/// (it's cut to the box's shape by the compositor, see [`Frames`]).
pub(crate) fn stream_view(placed: &crate::ScreenPlacement, display: Option<(u32, u32)>) -> (crate::Framing, Option<crate::Crop>) {
    // Turned or flipped, all of it (the compositor turns it, and puts it in).
    if placed.transformed() {
        return (crate::Framing::Fit, placed.crop);
    }
    if placed.is_whole() {
        return (placed.framing, placed.crop);
    }
    match display.and_then(|px| showing(placed, px)) {
        Some(part) => (crate::Framing::Stretch, Some(part)),
        None => (crate::Framing::Fit, placed.crop),
    }
}

/// The part of a display `dw`×`dh` pixels big that shows while the screen
/// is stretched over a box covering all of the frame (a screen bigger than
/// the video, its middle or another part showing). The stream records just
/// that, at the frame's size, so it's as sharp as filling the frame is
/// (the compositor would scale up a picture fitted in the frame).
fn showing(placed: &crate::ScreenPlacement, (dw, dh): (u32, u32)) -> Option<crate::Crop> {
    if !placed.covers() || placed.framing != crate::Framing::Stretch || placed.hidden {
        return None;
    }
    let [x, y, w, h] = placed.rect;
    let [cx, cy, cw, ch] = placed.crop.map_or([0, 0, dw & !1, dh & !1], |c| c.rect(dw, dh));
    // Where the frame's edges are on the picture, from 0 to 1 across it.
    let along = |edge: f32, at: f32, size: f32| ((edge - at) / size.max(1e-6)).clamp(0.0, 1.0) as f64;
    let px = |u: f64, from: u32, n: u32| (from as f64 + u * n as f64).round() as u32 & !1;
    let (x0, x1) = (px(along(0.0, x, w), cx, cw), px(along(1.0, x, w), cx, cw));
    let (y0, y1) = (px(along(0.0, y, h), cy, ch), px(along(1.0, y, h), cy, ch));
    Some(crate::Crop { w: x1.saturating_sub(x0).max(2), h: y1.saturating_sub(y0).max(2), at: Some((x0, y0)) })
}

/// Whether the stream's frames are the screen's part of the frame as they
/// are (see [`stream_view`]): it fills the frame, or covers it, stretched
/// over its box, from a display `display` pixels big.
fn as_streamed(placed: &crate::ScreenPlacement, display: Option<(u32, u32)>) -> bool {
    !placed.transformed() && (placed.is_whole() || display.is_some_and(|px| showing(placed, px).is_some()))
}

/// One running capture: its stream, and for games and apps the thread that
/// follows them. Whole screen or games and apps can be switched while it
/// runs: the same stream, its filter swapped (the frame size stays).
pub(crate) struct Picture {
    pub latest: Arc<Latest>,
    stream: Retained<SCStream>,
    _output: Retained<ScreenOutput>,
    out: Arc<Output>,
    follow: Option<(Arc<AtomicBool>, JoinHandle<()>, Arc<Mutex<AppsConfig>>)>,
    /// The display shown while recording the whole screen (by id).
    display: String,
    size: (usize, usize),
    /// The stream's settings as they change: how what's recorded goes into
    /// the frame, for what's shown.
    ctl: Arc<StreamCtl>,
    away: Option<Arc<Buffer>>,
}

/// The stream's settings that change while it runs, kept together so
/// whoever changes one sets the stream up with the rest as they are: how
/// what's recorded goes into the frame (the compositor asks for all of it
/// while the screen is in a box of its own, see [`Frames`]), and what's
/// recorded (a display and the part of it shown, or a window). Shared by
/// the picture, the thread that follows games and apps, and the frames.
pub(crate) struct StreamCtl {
    ctl: Mutex<Ctl>,
    /// Held while the stream is set up: the last one set up is what was
    /// asked for last.
    setting_up: Mutex<()>,
}

/// What the stream shows, and how.
#[derive(Debug, Clone, Copy, PartialEq)]
struct View {
    framing: crate::Framing,
    /// What's recorded, in points.
    content: (f64, f64),
    /// While a display is recorded (not a window): its size in pixels, and
    /// the part of it shown (in them).
    display: Option<(u32, u32)>,
    crop: Option<crate::Crop>,
}

struct Ctl {
    stream: SendStream,
    size: (usize, usize),
    fps: u32,
    /// What's asked for, and what the stream was last set up for (what its
    /// frames show).
    wanted: View,
    set: View,
    /// The frames' changes are being made, on a thread of their own.
    busy: bool,
}

impl View {
    /// Where what's recorded goes in a `size` frame, in pixels: (left, top,
    /// width, height). A window's place comes from its shape (its size in
    /// points).
    fn picture(&self, (w, h): (usize, usize)) -> [u32; 4] {
        let content = match self.display {
            Some(px) => crate::cropped_size(self.crop, px),
            None => (self.content.0.round() as u32, self.content.1.round() as u32),
        };
        crate::placement(content, (w as u32, h as u32), self.framing).1
    }
}

/// `crop` of a display `pixels` big, in its points (it's `content` points
/// big): (left, top, width, height).
fn crop_points(content: (f64, f64), (pw, ph): (u32, u32), crop: Option<crate::Crop>) -> Option<[f64; 4]> {
    let [x, y, cw, ch] = crop?.rect(pw, ph);
    let (sx, sy) = (content.0 / pw.max(1) as f64, content.1 / ph.max(1) as f64);
    Some([x as f64 * sx, y as f64 * sy, cw as f64 * sx, ch as f64 * sy])
}

impl StreamCtl {
    fn new(stream: &Retained<SCStream>, size: (usize, usize), fps: u32, view: View) -> Self {
        let ctl = Ctl { stream: SendStream(stream.clone()), size, fps, wanted: view, set: view, busy: false };
        Self { ctl: Mutex::new(ctl), setting_up: Mutex::new(()) }
    }

    /// The stream set up for what's asked for now (a few milliseconds).
    /// Returns what that was, and whether it could.
    fn set_up(&self) -> (View, Result<()>) {
        let _one = self.setting_up.lock().unwrap();
        let (stream, size, fps, view) = {
            let c = self.ctl.lock().unwrap();
            (c.stream.0.clone(), c.size, c.fps, c.wanted)
        };
        let done = reconfigure(&stream, size, fps, &view);
        if done.is_ok() {
            self.ctl.lock().unwrap().set = view;
        }
        (view, done)
    }

    /// The display's size in pixels, while a display's recorded (not a window).
    pub(crate) fn display(&self) -> Option<(u32, u32)> {
        self.ctl.lock().unwrap().wanted.display
    }

    /// At most `fps` pictures a second from now on (the looks' rate, frame
    /// blending): the stream set up again for it (a few milliseconds).
    fn set_rate(&self, fps: u32) {
        {
            let mut c = self.ctl.lock().unwrap();
            if c.fps == fps {
                return;
            }
            c.fps = fps;
        }
        if let (_, Err(e)) = self.set_up() {
            eprintln!("can't look at the screen {fps} times a second: {e:#}");
        }
    }

    /// Put what's recorded into the frame as `framing` says from now on,
    /// showing `crop` of a display. Called for every frame: a change sets
    /// the stream up again on a thread of its own, so the frames don't wait
    /// for it, and changes made meanwhile (a crop dragged) are made
    /// together, once it's done.
    pub(crate) fn set_view(self: &Arc<Self>, framing: crate::Framing, crop: Option<crate::Crop>) {
        {
            let mut c = self.ctl.lock().unwrap();
            if (c.wanted.framing, c.wanted.crop) == (framing, crop) {
                return;
            }
            (c.wanted.framing, c.wanted.crop) = (framing, crop);
            if c.busy {
                return;
            }
            c.busy = true;
        }
        let me = self.clone();
        thread::spawn(move || loop {
            let (tried, done) = me.set_up();
            if let Err(e) = done {
                eprintln!("can't put the screen into the frame another way: {e:#}");
            }
            let mut c = me.ctl.lock().unwrap();
            if c.wanted == tried {
                c.busy = false;
                break;
            }
        });
    }

    /// A window is recorded from now on, `content` points big.
    fn show_window(&self, content: (f64, f64)) -> Result<()> {
        {
            let mut c = self.ctl.lock().unwrap();
            (c.wanted.content, c.wanted.display) = (content, None);
        }
        self.set_up().1
    }

    /// From now on a window is recorded (set up when it's found).
    fn windows(&self) {
        let mut c = self.ctl.lock().unwrap();
        (c.wanted.display, c.set.display) = (None, None);
    }

    /// Recording a display now, `content` points and `pixels` big, the part
    /// of it shown as before: the stream set up for it.
    fn show_display(&self, content: (f64, f64), pixels: (u32, u32)) -> Result<()> {
        {
            let mut c = self.ctl.lock().unwrap();
            (c.wanted.content, c.wanted.display) = (content, Some(pixels));
        }
        self.set_up().1
    }

    /// Where what's recorded is in the frames the stream makes, in pixels:
    /// (left, top, width, height).
    pub(crate) fn picture_rect(&self) -> [u32; 4] {
        let c = self.ctl.lock().unwrap();
        c.set.picture(c.size)
    }
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
    /// Start capturing `source` at `width`×`height`, `fps`, put into the
    /// frame for the screen placed as `placed` says (see [`stream_view`]).
    /// For a display, waits for its first frame (so a capture that can't see
    /// the screen fails visibly, not with an empty file).
    pub(crate) fn start(
        source: &VideoSource,
        content: &SCShareableContent,
        display: &SCDisplay,
        (width, height): (usize, usize),
        fps: u32,
        away: Option<Arc<Buffer>>,
        placed: crate::ScreenPlacement,
    ) -> Result<Self> {
        let latest = Arc::new(Latest::default());
        let out = Arc::new(Output { latest: latest.clone(), paused: AtomicBool::new(false), frames: AtomicU64::new(0), shows: Mutex::new(None), resized: Condvar::new() });
        let output = ScreenOutput::new(out.clone());
        let pixels = match source {
            VideoSource::Screen { .. } => {
                let (pw, ph) = display_pixels(display);
                Some((pw as u32, ph as u32))
            }
            VideoSource::Apps { .. } => None,
        };
        let (framing, crop) = stream_view(&placed, pixels);
        let view = View { framing, content: points(display), display: pixels, crop };
        let config = stream_config(width, height, fps, &view);
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
        {
            let _one = crate::sck::STARTING.lock();
            await_completion(|h| unsafe { stream.startCaptureWithCompletionHandler(Some(h)) }).map_err(|e| anyhow::anyhow!("couldn't start screen capture: {e}"))?;
        }
        let display_id = match source {
            VideoSource::Screen { id, .. } => id.clone(),
            VideoSource::Apps { .. } => String::new(),
        };
        let _ = content;
        let ctl = Arc::new(StreamCtl::new(&stream, (width, height), fps, view));
        let mut picture = Self { latest, stream, _output: output, out, follow: None, display: display_id, size: (width, height), ctl, away };
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
            Some(config) => picture.start_following(config),
        }
        Ok(picture)
    }

    /// Start following games and apps (from the away screen).
    fn start_following(&mut self, config: AppsConfig) {
        self.ctl.windows();
        let list = Arc::new(Mutex::new(config));
        let stop = Arc::new(AtomicBool::new(false));
        let stream = SendStream(self.stream.clone());
        let (out, latest, list2, stop2, away, size, ctl) = (self.out.clone(), self.latest.clone(), list.clone(), stop.clone(), self.away.clone(), self.size, self.ctl.clone());
        let handle = thread::spawn(move || follow_apps(stream, out, latest, list2, stop2, away, size, ctl));
        self.follow = Some((stop, handle, list));
    }

    fn stop_following(&mut self) {
        if let Some((stop, handle, _)) = self.follow.take() {
            stop.store(true, Ordering::Relaxed);
            let _ = handle.join();
        }
    }

    /// Record something else without restarting: another list of games and
    /// apps, another display, or the whole screen instead of games and apps
    /// (or the other way round). False if it couldn't.
    pub(crate) fn update(&mut self, source: &VideoSource) -> bool {
        match (AppsConfig::of(source), &self.follow) {
            (Some(config), Some((_, _, list))) => {
                *list.lock().unwrap() = config;
                true
            }
            (Some(config), None) => {
                // From the whole screen: the away screen until one of them shows.
                self.out.paused.store(true, Ordering::Relaxed);
                show_away(&self.latest, self.away.as_ref().filter(|_| config.away_when_unfocused), self.size.0, self.size.1);
                self.start_following(config);
                true
            }
            (None, following) => {
                let VideoSource::Screen { id, .. } = source else { return false };
                if following.is_none() && *id == self.display {
                    return true;
                }
                self.stop_following();
                match show_display(&self.stream, &self.ctl, id) {
                    Ok(()) => {
                        self.display = id.clone();
                        *self.latest.app.lock().unwrap() = None;
                        self.latest.waiting.store(false, Ordering::Relaxed);
                        self.out.paused.store(false, Ordering::Relaxed);
                        true
                    }
                    Err(e) => {
                        eprintln!("can't switch to recording the screen: {e:#}");
                        false
                    }
                }
            }
        }
    }

    /// The stream's settings as they change, for the frames made from it.
    pub(crate) fn ctl(&self) -> Arc<StreamCtl> {
        self.ctl.clone()
    }

    /// Hand over at most `fps` pictures a second from now on (the looks'
    /// rate, frame blending changed while it runs).
    pub(crate) fn set_rate(&self, fps: u32) {
        self.ctl.set_rate(fps);
    }

    pub(crate) fn stop(mut self) {
        self.stop_following();
        let _ = await_completion(|h| unsafe { self.stream.stopCaptureWithCompletionHandler(Some(h)) });
    }
}

/// Point the stream at a whole display (by id; the main one if it's gone),
/// the part of it shown as before, put in as the stream's settings say.
fn show_display(stream: &SCStream, ctl: &StreamCtl, id: &str) -> Result<()> {
    let content = shareable_content()?;
    let display = pick_display(&content, id).context("no display to capture")?;
    let filter = unsafe { SCContentFilter::initWithDisplay_excludingWindows(SCContentFilter::alloc(), &display, &NSArray::new()) };
    await_completion(|h| unsafe { stream.updateContentFilter_completionHandler(&filter, Some(h)) }).map_err(|e| anyhow::anyhow!("{e}"))?;
    let (pw, ph) = display_pixels(&display);
    ctl.show_display(points(&display), (pw as u32, ph as u32))
}

/// A display's size in points, what the stream's source rect is in.
fn points(display: &SCDisplay) -> (f64, f64) {
    unsafe { (display.width() as f64, display.height() as f64) }
}

/// Set the stream up again for what it now shows, as `view` says.
fn reconfigure(stream: &SCStream, (width, height): (usize, usize), fps: u32, view: &View) -> Result<()> {
    let config = stream_config(width, height, fps, view);
    await_completion(|h| unsafe { stream.updateConfiguration_completionHandler(&config, Some(h)) }).map_err(|e| anyhow::anyhow!("{e}"))
}

struct SendStream(Retained<SCStream>);
unsafe impl Send for SendStream {}

/// The stream's settings for a `width`×`height` frame at `fps`. What's
/// recorded (a display, or a window, `view.content` points big) goes in as
/// `view.framing` says: its middle, cut to the frame's shape (Fill); all of
/// it, with black around (Fit); or all of it, stretched. A display's crop:
/// only that part of it, put in the same way.
fn stream_config(width: usize, height: usize, fps: u32, view: &View) -> Retained<SCStreamConfiguration> {
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};
    let (framing, content) = (view.framing, Some(view.content));
    let crop = view.display.and_then(|px| crop_points(view.content, px, view.crop));
    unsafe {
        let config = SCStreamConfiguration::new();
        config.setWidth(width);
        config.setHeight(height);
        // At most a picture every 0.9 of a frame's time (`fps` is the looks'
        // rate when blending). The screen's pictures come right at a frame's
        // time from a game at the frame rate, and asked for at most one a
        // frame's time, ScreenCaptureKit left out one that came a hair early:
        // measured on a 120 Hz display changing at every refresh, 106-110 of
        // 120 a second at 120 fps; at 60 fps 56-57 of 60, a 25 ms gap about
        // six times a second; 81% of blended frames (60 fps, 2 looks) with
        // both their pictures. At 0.9 it hands over each, and no more (at half
        // a frame's time it handed over twice the pictures at 60 fps).
        config.setMinimumFrameInterval(CMTime::new(9, fps as i32 * 10));
        config.setPixelFormat(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange);
        config.setColorMatrix(kCGDisplayStreamYCbCrMatrix_ITU_R_709_2);
        config.setShowsCursor(true);
        // A window fills the frame as far as its shape allows (up and down),
        // with black around it; stretched, all of the frame.
        config.setScalesToFit(true);
        config.setPreservesAspectRatio(framing != crate::Framing::Stretch);
        // In the middle: on its own, ScreenCaptureKit puts a window at the
        // top left.
        if view.display.is_none() {
            let [x, y, w, h] = view.picture((width, height));
            config.setDestinationRect(CGRect::new(CGPoint::new(x as f64, y as f64), CGSize::new(w as f64, h as f64)));
        }
        // What's used of it: the crop, or all of it.
        let area = crop.or(content.map(|(cw, ch)| [0.0, 0.0, cw, ch]));
        if let Some([x, y, cw, ch]) = area
            && cw > 0.0
            && ch > 0.0
        {
            if framing == crate::Framing::Fill {
                let shape = width as f64 / height.max(1) as f64;
                let (w, h) = if cw / ch > shape { (ch * shape, ch) } else { (cw, cw / shape) };
                config.setSourceRect(CGRect::new(CGPoint::new(x + (cw - w) / 2.0, y + (ch - h) / 2.0), CGSize::new(w, h)));
            } else if crop.is_some() {
                config.setSourceRect(CGRect::new(CGPoint::new(x, y), CGSize::new(cw, ch)));
            }
        }
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
    latest.put(frame);
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
    list: Arc<Mutex<AppsConfig>>,
    stop: Arc<AtomicBool>,
    away: Option<Arc<Buffer>>,
    (width, height): (usize, usize),
    ctl: Arc<StreamCtl>,
) {
    let stream = stream.0;
    // The size (points) of the window the stream is set up for.
    let mut shown: Option<(f64, f64)> = None;
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
        // What the frames showed before this look (see the wait below).
        let seen = *out.shows.lock().unwrap();
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
                            shown = None;
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
        // The stream set up for the window's size, and again when it
        // changes: filling the frame, cut to its shape; fitted, in its middle.
        if let Some((w, _)) = &current
            && let Some(win) = all.iter().find(|x| x.id == *w)
            && shown != Some((win.width, win.height))
        {
            // Not tried again until it changes size.
            shown = Some((win.width, win.height));
            if let Err(e) = ctl.show_window((win.width, win.height)) {
                eprintln!("can't fit the window to the recording: {e:#}");
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
        // Look again in RESCAN, or as soon as a frame shows a new size: a
        // window resized is drawn at the top left of its old place until
        // the stream's set up for it (the window list has its new size by
        // the time its first frame comes).
        let until = Instant::now() + RESCAN;
        let mut shows = out.shows.lock().unwrap();
        while Instant::now() < until && !stop.load(Ordering::Relaxed) && *shows == seen {
            shows = out.resized.wait_timeout(shows, Duration::from_millis(20)).unwrap().0;
        }
    }
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

/// Turns the picture, and the layers over it or under it, into frames to
/// encode, and feeds the preview.
pub(crate) struct Frames {
    latest: Arc<Latest>,
    /// Where the screen's picture goes, and the stream's settings to ask for
    /// all of it while it's in a box (it's cut to the box's shape here).
    screen: crate::SharedScreen,
    ctl: Option<Arc<StreamCtl>>,
    /// The other layers' pictures, followed each frame.
    feeds: Feeds,
    /// The compositor and the buffers it makes frames in.
    gpu: Option<(Compositor, FramePool)>,
    /// The last frame made, and what it was made from, to repeat it when
    /// nothing changed.
    last: Option<(Arc<Buffer>, Made)>,
    preview: crate::mac::preview::Producer,
}

/// What a frame was made from: the screen's picture, where it went, and
/// each layer as it was placed and the picture it showed.
#[derive(PartialEq)]
struct Made {
    screen: usize,
    placed: crate::ScreenPlacement,
    layers: Vec<(crate::layers::Layer, Option<(usize, u64)>)>,
}

impl Frames {
    /// `ctl`: the stream's settings, when it's ours to change. `layers`:
    /// what's drawn, bottom first (the screen's own picture where `Main`
    /// is), read every frame; displays and windows in it are captured at
    /// `fps`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(latest: Arc<Latest>, layers: SharedLayers, screen: crate::SharedScreen, ctl: Option<Arc<StreamCtl>>, width: usize, height: usize, fps: u32, generation: u64) -> Result<Self> {
        let gpu = match Compositor::new().and_then(|g| Ok((g, FramePool::new(width, height)?))) {
            Ok(g) => Some(g),
            // The screen still records, as ScreenCaptureKit makes it.
            Err(e) => {
                eprintln!("no compositor: {e:#}");
                None
            }
        };
        Ok(Self { latest, screen, ctl, feeds: Feeds::new(layers, fps), gpu, last: None, preview: crate::mac::preview::Producer::new(generation) })
    }

    /// The frame to encode now, if there's a picture yet.
    pub(crate) fn next(&mut self) -> Option<Arc<Buffer>> {
        let screen = self.latest.get()?;
        Some(self.make(screen))
    }

    /// The frame to encode from `screen` (the newest picture, or a blend of
    /// several): as is when it fills the frame and nothing else shows, else
    /// made by the compositor, every layer where it's placed, in order.
    pub(crate) fn make(&mut self, screen: Arc<Buffer>) -> Arc<Buffer> {
        let placed = *self.screen.lock().unwrap();
        // Filling the frame (or covering it), ScreenCaptureKit puts the
        // picture in as the placement says; in a box, all of it, cut to the
        // box's shape here. It cuts out the crop either way.
        let display = self.ctl.as_ref().and_then(|c| c.display());
        let mut fit_window = false;
        if let Some(ctl) = &self.ctl {
            let (framing, crop) = stream_view(&placed, display);
            ctl.set_view(framing, crop);
            fit_window = display.is_none() && framing == crate::Framing::Fit;
        }
        let streamed = if self.ctl.is_some() { as_streamed(&placed, display) } else { placed.is_whole() && !placed.transformed() };
        let layers = self.feeds.update();
        let (w, h) = size_of(&screen.0);
        let set = self.ctl.as_ref().map_or([0, 0, w as u32, h as u32], |c| c.picture_rect());
        let (picture, goes) = screen_place(self.latest.drawn(&screen), fit_window, set, (w as u32, h as u32));
        // Nothing but the screen shows, filling the frame: its own buffer.
        let alone = layers.iter().all(|(l, shown)| match &l.source {
            LayerSource::Main => true,
            _ => !l.shown() || shown.is_none(),
        });
        let main = layers.iter().any(|(l, _)| matches!(l.source, LayerSource::Main));
        let frame = match &self.gpu {
            _ if alone && main && streamed && !placed.hidden && picture == goes => {
                self.last = None;
                screen
            }
            None => screen,
            Some((gpu, pool)) => {
                // A picture by its buffer (a camera's comes in an Arc of its
                // own each time) and its number (a pool's buffers come back).
                let buffer = |b: &Buffer| &*b.0 as *const objc2_core_video::CVPixelBuffer as usize;
                let made = Made {
                    screen: buffer(&screen),
                    placed,
                    layers: layers.iter().map(|(l, s)| (l.clone(), s.as_ref().map(|s| (buffer(&s.picture), s.version)))).collect(),
                };
                match &self.last {
                    // Nothing changed: the same frame again (the encoder holds it anyway).
                    Some((b, m)) if *m == made => b.clone(),
                    _ => {
                        let draws = draws(&layers, &screen.0, &placed, streamed.then_some(goes), picture, (w as u32, h as u32));
                        match pool.take().and_then(|out| gpu.compose(&draws, &out, pool).map(|()| out)) {
                            Ok(out) => {
                                let b = Arc::new(Buffer(out));
                                self.last = Some((b.clone(), made));
                                b
                            }
                            Err(e) => {
                                eprintln!("compositor: {e:#}");
                                screen
                            }
                        }
                    }
                }
            }
        };
        self.preview.offer(&frame, self.latest.waiting.load(Ordering::Relaxed), self.latest.app.lock().unwrap().clone());
        frame
    }
}

/// What the compositor draws for `layers`, bottom first, in a `frame`-size
/// frame: the screen's picture (`screen`, at `picture` in it) where `Main`
/// is, as `placed` says (`streamed`: where it goes, see [`screen_rects`]);
/// each other layer that shows where it's placed (a webcam filling its box
/// with its own shape; the rest as a display is, on any pixel).
fn draws<'a>(
    layers: &'a [(crate::layers::Layer, Option<super::layers::Shown>)],
    screen: &'a objc2_core_video::CVPixelBuffer,
    placed: &crate::ScreenPlacement,
    streamed: Option<[u32; 4]>,
    picture: [u32; 4],
    frame: (u32, u32),
) -> Vec<Draw<'a>> {
    let mut out = Vec::with_capacity(layers.len());
    let edges = |[x, y, w, h]: [u32; 4]| [x as f32, y as f32, (x + w) as f32, (y + h) as f32];
    for (l, shown) in layers {
        match &l.source {
            LayerSource::Main => {
                if let Some(r) = screen_rects(placed, streamed, picture, frame) {
                    out.push(r.draw(screen));
                }
            }
            LayerSource::Camera { placement: p, .. } => {
                let Some(s) = shown else { continue };
                let (cw, ch) = size_of(&s.picture.0);
                if let Some((src, dest)) = super::gpu::camera_rects(*p, (cw as u32, ch as u32), frame) {
                    out.push(Draw::cover(&s.picture.0, src, dest).turned(p.turns, p.flip_h, p.flip_v));
                }
            }
            LayerSource::Display { placement: p, .. }
            | LayerSource::Window { placement: p, .. }
            | LayerSource::Picture { placement: p, .. }
            | LayerSource::Colour { placement: p, .. }
            | LayerSource::Browser { placement: p, .. }
            | LayerSource::Animation { placement: p, .. }
            | LayerSource::Video { placement: p, .. } => {
                let Some(s) = shown.as_ref().filter(|_| !p.hidden) else { continue };
                let (pw, ph) = size_of(&s.picture.0);
                let (src, dest) = p.place_on((pw as u32, ph as u32), frame, false);
                if src[2] == 0 || src[3] == 0 || dest[2] == 0 || dest[3] == 0 {
                    continue;
                }
                let d = Draw { bt601: s.bt601, ..Draw::blended(&s.picture.0, edges(src), edges(dest), s.alpha) };
                out.push(d.turned(p.turns, p.flip_h, p.flip_v));
            }
        }
    }
    out
}

/// Where the screen's picture is drawn from and to, for the compositor, in
/// a `frame`-size frame whose stream put the picture at `picture`: while
/// the stream's frames are the screen's part of the frame (it fills the
/// frame, or covers it), drawn where it goes (`streamed`), copied when
/// that's where it is (a window just resized isn't yet, see
/// [`screen_place`]); else cut to its box's shape and drawn there, as much
/// of it as is in the frame. `None` when it's hidden.
pub(crate) fn screen_rects(placed: &crate::ScreenPlacement, streamed: Option<[u32; 4]>, picture: [u32; 4], frame: (u32, u32)) -> Option<super::gpu::ScreenRects> {
    if placed.hidden {
        return None;
    }
    let edges = |[x, y, w, h]: [u32; 4]| [x as f32, y as f32, (x + w) as f32, (y + h) as f32];
    if let Some(goes) = streamed {
        return Some(super::gpu::ScreenRects { src: edges(picture), dest: edges(goes), copy: goes == picture, turns: 0, flip_h: false, flip_v: false });
    }
    let [px, py, pw, ph] = picture;
    let ([sx, sy, sw, sh], dest) = placed.place_turned((pw, ph), frame);
    Some(super::gpu::ScreenRects { src: edges([px + sx, py + sy, sw, sh]), dest: edges(dest), copy: false, turns: placed.turns, flip_h: placed.flip_h, flip_v: placed.flip_v })
}

/// Where the screen's picture is in a `frame`-size frame from the stream,
/// and where it goes: as ScreenCaptureKit says it drew it (`drawn`), for a
/// window fitted in the frame (`fit_window`) or a still; else where the
/// stream was set up to put it (`set`). A window resized is drawn at the
/// top left of its old place until the stream's set up for its new size (a
/// few frames): it goes where its new size puts it, in the middle.
fn screen_place(drawn: Option<Drawn>, fit_window: bool, set: [u32; 4], frame: (u32, u32)) -> ([u32; 4], [u32; 4]) {
    match drawn {
        Some(Drawn { at, shows: None }) => (at, at),
        Some(Drawn { at, shows: Some(size) }) if fit_window => {
            let goes = crate::placement(size, frame, crate::Framing::Fit).1;
            // Where it's set up to go, give or take the stream's rounding.
            let there = at.iter().zip(goes).all(|(&a, g)| a.abs_diff(g) <= 4);
            (at, if there { at } else { goes })
        }
        _ => (set, set),
    }
}

/// A buffer's size.
pub(crate) fn size_of(b: &CVPixelBuffer) -> (usize, usize) {
    (CVPixelBufferGetWidth(b), CVPixelBufferGetHeight(b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Crop, Framing, ScreenPlacement};

    /// The stream records as the placement says filling the frame, all of
    /// the screen for a box (the compositor puts it there), and just the
    /// part showing for a box covering the frame, so it's as sharp.
    #[test]
    fn what_the_stream_records() {
        let display = Some((3024, 1964));
        let fill = ScreenPlacement::whole(Framing::Fill);
        assert_eq!(stream_view(&fill, display), (Framing::Fill, None));
        let boxed = ScreenPlacement { rect: [0.1, 0.1, 0.5, 0.5], framing: Framing::Stretch, ..fill };
        assert_eq!(stream_view(&boxed, display), (Framing::Fit, None));
        assert!(!as_streamed(&boxed, display));
        // A 16:10 display covering a 16:9 frame, moved down to its top: its
        // top 3024×1700, the frame's shape.
        let over = ScreenPlacement { rect: [0.0, 0.0, 1.0, 1964.0 / 1700.0], ..boxed };
        assert_eq!(stream_view(&over, display), (Framing::Stretch, Some(Crop { w: 3024, h: 1700, at: Some((0, 0)) })));
        assert!(as_streamed(&over, display));
        // In the middle, cropped: the middle of the crop.
        let crop = Some(Crop { w: 2000, h: 1300, at: Some((100, 200)) });
        let middle = ScreenPlacement { rect: [0.0, -0.1, 1.0, 1.2], crop, ..boxed };
        assert_eq!(stream_view(&middle, display), (Framing::Stretch, Some(Crop { w: 2000, h: 1084, at: Some((100, 308)) })));
        // A window (no display): the compositor's.
        assert_eq!(stream_view(&over, None), (Framing::Fit, None));
        assert!(!as_streamed(&over, None));
        // Fitted in its box, the picture doesn't cover the frame.
        assert_eq!(stream_view(&ScreenPlacement { framing: Framing::Fit, ..over }, display).0, Framing::Fit);
    }

    /// A window that isn't the frame's shape goes in the middle of it, as
    /// on Windows and Linux; ScreenCaptureKit alone puts it at the top left.
    #[test]
    fn a_window_goes_in_the_middle() {
        let window = View { framing: Framing::Fit, content: (800.0, 1000.0), display: None, crop: None };
        assert_eq!(window.picture((1662, 1080)), [398, 0, 864, 1080]);
        let r = unsafe { stream_config(1662, 1080, 60, &window).destinationRect() };
        assert_eq!((r.origin.x, r.origin.y, r.size.width, r.size.height), (398.0, 0.0, 864.0, 1080.0));
        // Filled or stretched, all of the frame.
        assert_eq!(View { framing: Framing::Fill, ..window }.picture((1662, 1080)), [0, 0, 1662, 1080]);
        assert_eq!(View { framing: Framing::Stretch, ..window }.picture((1662, 1080)), [0, 0, 1662, 1080]);
        // A display is put in as before: where it goes isn't asked for.
        let display = View { content: (1662.0, 1080.0), display: Some((3324, 2160)), ..window };
        assert_eq!(display.picture((1662, 1080)), [0, 0, 1662, 1080]);
        let r = unsafe { stream_config(1662, 1080, 60, &display).destinationRect() };
        assert_eq!((r.size.width, r.size.height), (0.0, 0.0));
    }

    /// Where ScreenCaptureKit says it drew a window, measured: a 500×732
    /// window resized to 860×552, its first frame still made for the old
    /// size (fitted in 736×1080 at 592, 0, so drawn at its top), then once
    /// the stream's set up for the new one.
    #[test]
    fn a_resized_window_goes_where_its_new_size_puts_it() {
        use objc2_core_foundation::{CGPoint, CGRect, CGSize};
        let rect = |x, y, w, h| CGRect::new(CGPoint::new(x, y), CGSize::new(w, h));
        let frame = (1920, 1080);
        let before = drawn(rect(296.0, 0.0, 368.0, 538.8), 0.7360, 2.0, (1920, 1080)).unwrap();
        assert_eq!(before, Drawn { at: [592, 0, 736, 1078], shows: Some((500, 732)) });
        // Where it was set up to go, give or take the rounding: as it is.
        let set = [592, 0, 736, 1080];
        assert_eq!(screen_place(Some(before), true, set, frame), ([592, 0, 736, 1078], [592, 0, 736, 1078]));
        let first = drawn(rect(296.0, 0.0, 368.0, 236.2), 0.4279, 2.0, (1920, 1080)).unwrap();
        assert_eq!(first, Drawn { at: [592, 0, 736, 472], shows: Some((860, 552)) });
        assert_eq!(screen_place(Some(first), true, set, frame), ([592, 0, 736, 472], [118, 0, 1682, 1080]));
        let after = drawn(rect(59.0, 0.0, 841.0, 539.8), 0.9779, 2.0, (1920, 1080)).unwrap();
        assert_eq!(screen_place(Some(after), true, [118, 0, 1682, 1080], frame), ([118, 0, 1682, 1080], [118, 0, 1682, 1080]));
        // Where it is, it's copied; the first frame is moved there.
        let placed = ScreenPlacement::whole(Framing::Fit);
        assert!(screen_rects(&placed, Some([592, 0, 736, 1078]), [592, 0, 736, 1078], frame).unwrap().copy);
        let moved = screen_rects(&placed, Some([118, 0, 1682, 1080]), [592, 0, 736, 472], frame).unwrap();
        assert_eq!((moved.src, moved.dest, moved.copy), ([592.0, 0.0, 1328.0, 472.0], [118.0, 0.0, 1800.0, 1080.0], false));
        // A still (the away screen) is all of the frame; a display, or a
        // window filling the frame, where the stream was set up to put it.
        let still = Drawn { at: [0, 0, 1920, 1080], shows: None };
        assert_eq!(screen_place(Some(still), true, set, frame), ([0, 0, 1920, 1080], [0, 0, 1920, 1080]));
        assert_eq!(screen_place(Some(first), false, set, frame), (set, set));
        assert_eq!(screen_place(None, true, set, frame), (set, set));
    }

    /// Where each picture has what it shows stays with it, for a while: it's
    /// made into a frame a little after it comes up. The away screen is all
    /// of the frame; a blend of several pictures isn't known.
    #[test]
    fn pictures_keep_where_they_have_what_they_show() {
        let latest = Latest::default();
        let drawn = |x| Drawn { at: [x, 0, 64, 36], shows: Some((100, 50)) };
        latest.set(still_buffer(None, 128, 72).unwrap().0, 0.0, Some(drawn(2)));
        let first = latest.get().unwrap();
        latest.set(still_buffer(None, 128, 72).unwrap().0, 0.1, Some(drawn(4)));
        let second = latest.get().unwrap();
        assert_eq!((latest.drawn(&first), latest.drawn(&second)), (Some(drawn(2)), Some(drawn(4))));
        let away = Arc::new(still_buffer(None, 128, 72).unwrap());
        latest.put(away.clone());
        assert_eq!(latest.drawn(&away), Some(Drawn { at: [0, 0, 128, 72], shows: None }));
        assert_eq!(latest.drawn(&Arc::new(still_buffer(None, 128, 72).unwrap())), None);
        // Only the newest are kept.
        for i in 0..8 {
            latest.set(still_buffer(None, 128, 72).unwrap().0, 0.2 + i as f64, Some(drawn(6)));
        }
        assert_eq!(latest.drawn(&second), None);
    }
}
