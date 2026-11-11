//! Webcams, with AVFoundation's capture session: the camera delivers NV12
//! frames in IOSurfaces (its own format, converted on the media engine when
//! it sends something else), which the compositor opens as Metal textures
//! without a copy (`super::gpu`).
//!
//! A webcam stays open for as long as it's set up ([`keep_open`]), previewed
//! and recorded or not, as on Windows; several can be open at once, each in a
//! session of its own. macOS has no driver settings window to open
//! (`open_settings` does nothing); its camera controls live in Control
//! Centre's Video Effects.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send};
use objc2_av_foundation::{
    AVAuthorizationStatus, AVCaptureConnection, AVCaptureDevice, AVCaptureDeviceDiscoverySession, AVCaptureDeviceFormat,
    AVCaptureDeviceInput, AVCaptureDevicePosition, AVCaptureDeviceTypeContinuityCamera, AVCaptureDeviceTypeExternal,
    AVCaptureDeviceTypeBuiltInWideAngleCamera, AVCaptureOutput, AVCaptureSession, AVCaptureVideoDataOutput,
    AVCaptureVideoDataOutputSampleBufferDelegate, AVMediaTypeVideo,
};
use objc2_core_foundation::CFRetained;
use objc2_core_media::{CMSampleBuffer, CMTime, CMVideoFormatDescriptionGetDimensions};
use objc2_core_video::{CVPixelBuffer, kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferPixelFormatTypeKey, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange};
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSObject, NSObjectProtocol, NSString};

use crate::Device;
use crate::webcam::{Format, Status, set_formats, set_status_of};

/// One camera picture: NV12 (studio range, BT.709) in an IOSurface.
pub(crate) struct Frame {
    pub seq: u64,
    pub buffer: CFRetained<CVPixelBuffer>,
}
// CoreVideo buffers are thread-safe reference-counted objects; nothing
// writes to one after the camera hands it over.
unsafe impl Send for Frame {}
unsafe impl Sync for Frame {}

/// A camera that's open, delivering pictures.
pub(crate) struct CameraFeed {
    pub device: String,
    pub format: Option<Format>,
    frame: Mutex<Option<Arc<Frame>>>,
    seq: AtomicU64,
    stop: AtomicBool,
}

/// The cameras kept open, each in the format asked for.
static OPEN: Mutex<Vec<Arc<CameraFeed>>> = Mutex::new(Vec::new());

/// Pictures from any camera so far, numbering them: going on from camera to
/// camera and format to format, so a reader waiting for one newer than the
/// last it had (the crop page) gets the next camera's at once.
static PICTURES: AtomicU64 = AtomicU64::new(0);

impl CameraFeed {
    /// The newest picture.
    pub(crate) fn latest(&self) -> Option<Arc<Frame>> {
        self.frame.lock().unwrap().clone()
    }

    /// Camera `device`, open: in `format` if it's open in that, else in
    /// whatever it's open in (another format's on its way).
    pub(crate) fn of(device: &str, format: Option<Format>) -> Option<Arc<CameraFeed>> {
        let open = OPEN.lock().unwrap();
        let live = || open.iter().filter(|f| f.device == device && !f.stop.load(Ordering::Relaxed));
        live().find(|f| f.format == format).or_else(|| live().next()).cloned()
    }

    /// Say how it's doing, while it's still wanted (one let go of says
    /// nothing: the one that takes its place, or nothing, has the say).
    fn status(&self, s: Status) {
        if !self.stop.load(Ordering::Relaxed) {
            set_status_of(&self.device, s);
        }
    }
}

/// Cameras: built in, external (USB) and Continuity (an iPhone), by unique id.
pub fn list_cameras() -> Vec<Device> {
    discovery().iter().map(|d| unsafe { Device { id: d.uniqueID().to_string(), name: d.localizedName().to_string() } }).collect()
}

fn discovery() -> Retained<NSArray<AVCaptureDevice>> {
    unsafe {
        let types = NSArray::from_slice(&[AVCaptureDeviceTypeBuiltInWideAngleCamera, AVCaptureDeviceTypeExternal, AVCaptureDeviceTypeContinuityCamera]);
        AVCaptureDeviceDiscoverySession::discoverySessionWithDeviceTypes_mediaType_position(&types, AVMediaTypeVideo, AVCaptureDevicePosition::Unspecified).devices()
    }
}

/// Keep these cameras (device, format) open, and close the rest. Doesn't
/// wait: each opens (and closes) on a thread of its own.
pub fn keep_open(want: Vec<(String, Option<Format>)>) {
    let mut open = OPEN.lock().unwrap();
    open.retain(|f| {
        let keep = want.iter().any(|(d, fm)| f.device == *d && f.format == *fm);
        if !keep {
            // Its thread closes it; it's off unless another format of it is
            // asked for (that one's status takes over).
            f.stop.store(true, Ordering::Relaxed);
            if !want.iter().any(|(d, _)| *d == f.device) {
                set_status_of(&f.device, Status::Off);
            }
        }
        keep
    });
    for (device, format) in want {
        if open.iter().any(|f| f.device == device && f.format == format) {
            continue;
        }
        let feed = Arc::new(CameraFeed { device, format, frame: Mutex::new(None), seq: AtomicU64::new(0), stop: AtomicBool::new(false) });
        open.push(feed.clone());
        thread::spawn(move || run(feed));
    }
}

/// Keep camera `device` (in `format`) open too, if it isn't: for a capture
/// that draws a camera the app hasn't opened (it keeps them open itself).
fn ensure_open(device: &str, format: Option<Format>) {
    let mut want: Vec<(String, Option<Format>)> = OPEN.lock().unwrap().iter().filter(|f| !f.stop.load(Ordering::Relaxed)).map(|f| (f.device.clone(), f.format)).collect();
    if !want.iter().any(|(d, _)| d == device) {
        want.push((device.to_owned(), format));
        keep_open(want);
    }
}

/// Open the camera and keep it open until told to stop, reopening it when it
/// goes away (unplugged, taken by another app) and comes back.
fn run(feed: Arc<CameraFeed>) {
    while !feed.stop.load(Ordering::Relaxed) {
        feed.status(Status::Opening);
        let opened = open_session(&feed);
        let (session, _delegate, (width, height)) = match opened {
            Ok(s) => s,
            Err(e) => {
                feed.status(Status::Unavailable(format!("{e:#}")));
                wait_or_stop(&feed, Duration::from_secs(3));
                continue;
            }
        };
        feed.status(Status::Live { width, height });
        // The session runs on its own; watch for it stopping. Unplugged, a
        // session can go on "running" with no pictures, and a camera plugged
        // back in is a new device it never picks up: no pictures for a few
        // seconds, or the camera gone, and it's opened again.
        let device = unsafe { AVCaptureDevice::deviceWithUniqueID(&NSString::from_str(&feed.device)) };
        let (mut seen, mut since) = (feed.seq.load(Ordering::Relaxed), std::time::Instant::now());
        while !feed.stop.load(Ordering::Relaxed) && unsafe { session.isRunning() } {
            thread::sleep(Duration::from_millis(100));
            let now = feed.seq.load(Ordering::Relaxed);
            if now != seen {
                (seen, since) = (now, std::time::Instant::now());
            }
            let gone = device.as_ref().is_none_or(|d| unsafe { !d.isConnected() });
            if gone || since.elapsed() > Duration::from_secs(3) {
                break;
            }
        }
        unsafe { session.stopRunning() };
        *feed.frame.lock().unwrap() = None;
        if !feed.stop.load(Ordering::Relaxed) {
            feed.status(Status::Unavailable("the camera stopped".into()));
            wait_or_stop(&feed, Duration::from_secs(3));
        }
    }
}

fn wait_or_stop(feed: &CameraFeed, d: Duration) {
    let until = std::time::Instant::now() + d;
    while std::time::Instant::now() < until && !feed.stop.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(50));
    }
}

/// Camera access: asks the first time (and waits for the answer).
fn ensure_access() -> Result<()> {
    let media = unsafe { AVMediaTypeVideo }.context("no video media type")?;
    match unsafe { AVCaptureDevice::authorizationStatusForMediaType(media) } {
        AVAuthorizationStatus::Authorized => Ok(()),
        AVAuthorizationStatus::NotDetermined => {
            let (tx, rx) = std::sync::mpsc::channel();
            let handler = block2::RcBlock::new(move |granted: objc2::runtime::Bool| {
                let _ = tx.send(granted.as_bool());
            });
            unsafe { AVCaptureDevice::requestAccessForMediaType_completionHandler(media, &handler) };
            match rx.recv_timeout(Duration::from_secs(120)) {
                Ok(true) => Ok(()),
                _ => bail!("camera access wasn't allowed — allow it in System Settings → Privacy & Security → Camera"),
            }
        }
        _ => bail!("camera access is off — allow it in System Settings → Privacy & Security → Camera"),
    }
}

/// The camera's formats as the app offers them, biggest first, and each
/// one's device format with the frame duration to ask for.
fn formats_of(device: &AVCaptureDevice) -> Vec<(Format, Retained<AVCaptureDeviceFormat>, CMTime)> {
    let mut out = Vec::new();
    for f in unsafe { device.formats() }.iter() {
        let dims = unsafe { CMVideoFormatDescriptionGetDimensions(&f.formatDescription()) };
        for range in unsafe { f.videoSupportedFrameRateRanges() }.iter() {
            // The fastest rate of each range (cameras list e.g. 1-30 fps).
            let d = unsafe { range.minFrameDuration() };
            if d.value <= 0 || d.timescale <= 0 || dims.width <= 0 || dims.height <= 0 {
                continue;
            }
            let format = Format { width: dims.width as u32, height: dims.height as u32, fps_num: d.timescale as u32, fps_den: d.value as u32 };
            out.push((format, f.clone(), d));
        }
    }
    out.sort_by(|a, b| (b.0.width * b.0.height, b.0.fps()).partial_cmp(&(a.0.width * a.0.height, a.0.fps())).unwrap());
    out.dedup_by(|a, b| a.0.label() == b.0.label());
    out
}

/// A running capture session for the feed's camera, with the delegate it
/// calls (kept alive with it) and the picture size.
fn open_session(feed: &Arc<CameraFeed>) -> Result<(Retained<AVCaptureSession>, Retained<CameraOutput>, (u32, u32))> {
    ensure_access()?;
    let device = unsafe { AVCaptureDevice::deviceWithUniqueID(&NSString::from_str(&feed.device)) }.context("the camera isn't connected")?;
    let offered = formats_of(&device);
    set_formats(&feed.device, offered.iter().map(|(f, ..)| *f).collect());
    // The format asked for, else the biggest up to 1080p that does at least 30 fps.
    let chosen = feed
        .format
        .and_then(|want| offered.iter().find(|(f, ..)| *f == want).or_else(|| offered.iter().find(|(f, ..)| f.label() == want.label())))
        .or_else(|| offered.iter().max_by_key(|(f, ..)| f.auto_rank()));
    let (chosen, device_format, duration) = chosen.context("the camera offers no usable format")?;

    unsafe {
        let session = AVCaptureSession::new();
        session.beginConfiguration();
        let input = AVCaptureDeviceInput::deviceInputWithDevice_error(&device).map_err(|e| anyhow!("couldn't open the camera: {}", e.localizedDescription()))?;
        if !session.canAddInput(&input) {
            bail!("the camera is in use");
        }
        session.addInput(&input);
        let output = AVCaptureVideoDataOutput::new();
        // NV12 in IOSurfaces: what the compositor reads without converting.
        let settings = NSDictionary::<NSString, objc2::runtime::AnyObject>::from_retained_objects(
            &[&*cf_key(kCVPixelBufferPixelFormatTypeKey), &*cf_key(kCVPixelBufferIOSurfacePropertiesKey)],
            &[
                Retained::into_super(Retained::into_super(Retained::into_super(NSNumber::new_u32(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange)))),
                Retained::into_super(Retained::into_super(NSDictionary::<NSString, objc2::runtime::AnyObject>::new())),
            ],
        );
        output.setVideoSettings(Some(&settings));
        output.setAlwaysDiscardsLateVideoFrames(true);
        let delegate = CameraOutput::new(feed.clone());
        let queue = DispatchQueue::new("hesteclips.camera", None);
        output.setSampleBufferDelegate_queue(Some(ProtocolObject::from_ref(&*delegate)), Some(&queue));
        if !session.canAddOutput(&output) {
            bail!("the camera's pictures can't be read");
        }
        session.addOutput(&output);
        // The format, set after adding the input (adding it resets it).
        device.lockForConfiguration().map_err(|e| anyhow!("couldn't set up the camera: {}", e.localizedDescription()))?;
        device.setActiveFormat(device_format);
        device.setActiveVideoMinFrameDuration(*duration);
        device.setActiveVideoMaxFrameDuration(*duration);
        session.commitConfiguration();
        session.startRunning();
        device.unlockForConfiguration();
        if !session.isRunning() {
            bail!("the camera didn't start (another app may be using it)");
        }
        Ok((session, delegate, (chosen.width, chosen.height)))
    }
}

/// A CoreFoundation string constant as an NSString key (toll-free bridged).
fn cf_key(key: &objc2_core_foundation::CFString) -> Retained<NSString> {
    unsafe { Retained::retain(key as *const _ as *mut NSString) }.expect("a string constant")
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSObject))]
    #[ivars = Arc<CameraFeed>]
    struct CameraOutput;

    unsafe impl NSObjectProtocol for CameraOutput {}

    unsafe impl AVCaptureVideoDataOutputSampleBufferDelegate for CameraOutput {
        #[unsafe(method(captureOutput:didOutputSampleBuffer:fromConnection:))]
        fn did_output(&self, _output: &AVCaptureOutput, sample: &CMSampleBuffer, _connection: &AVCaptureConnection) {
            let feed = self.ivars();
            if let Some(buffer) = unsafe { sample.image_buffer() } {
                feed.seq.fetch_add(1, Ordering::Relaxed);
                let seq = PICTURES.fetch_add(1, Ordering::Relaxed) + 1;
                *feed.frame.lock().unwrap() = Some(Arc::new(Frame { seq, buffer }));
            }
        }
    }
);

impl CameraOutput {
    fn new(feed: Arc<CameraFeed>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(feed);
        unsafe { msg_send![super(this), init] }
    }
}

/// What a running capture draws for a webcam: the newest picture of camera
/// `device`, as the app keeps it open. A new format, the webcam switched off
/// and on, a dropout: it follows along without the capture restarting.
pub(crate) struct CameraLayer {
    device: String,
    format: Option<Format>,
    feed: Option<Arc<CameraFeed>>,
}

impl CameraLayer {
    /// Opens camera `device` if nothing has yet (the app keeps it open
    /// itself) and it shows.
    pub(crate) fn new(device: &str, format: Option<Format>, hidden: bool) -> Self {
        if !hidden && CameraFeed::of(device, format).is_none() {
            ensure_open(device, format);
        }
        Self { device: device.to_owned(), format, feed: None }
    }

    /// The newest picture, if the camera is delivering.
    pub(crate) fn latest(&mut self) -> Option<Arc<Frame>> {
        if self.feed.as_ref().is_none_or(|f| f.stop.load(Ordering::Relaxed)) {
            self.feed = CameraFeed::of(&self.device, self.format);
        }
        self.feed.as_ref()?.latest()
    }
}

/// Camera `device`'s newest picture, if it's open and has one newer than
/// picture `newer_than`: shrunk to at most `max_width` wide (each pixel the
/// one under it), in RGBA, for showing it on its own (its crop, its page).
pub(crate) fn picture(device: &str, max_width: u32, newer_than: u64) -> Option<crate::webcam::CameraPreview> {
    use objc2_core_video::{CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress};
    let feed = CameraFeed::of(device, None)?;
    let f = feed.latest().filter(|f| f.seq > newer_than)?;
    let buf = &f.buffer;
    let (sw, sh) = (CVPixelBufferGetWidth(buf), CVPixelBufferGetHeight(buf));
    if sw == 0 || sh == 0 {
        return None;
    }
    let scale = (max_width as f64 / sw as f64).min(1.0);
    let (w, h) = (((sw as f64 * scale).round() as usize).max(1), ((sh as f64 * scale).round() as usize).max(1));
    let mut rgba = vec![255u8; w * h * 4];
    // NV12, studio range, BT.709 (as the camera's asked to send it).
    unsafe {
        if CVPixelBufferLockBaseAddress(buf, CVPixelBufferLockFlags::ReadOnly) != 0 {
            return None;
        }
        let y = CVPixelBufferGetBaseAddressOfPlane(buf, 0) as *const u8;
        let ys = CVPixelBufferGetBytesPerRowOfPlane(buf, 0);
        let c = CVPixelBufferGetBaseAddressOfPlane(buf, 1) as *const u8;
        let cs = CVPixelBufferGetBytesPerRowOfPlane(buf, 1);
        if !y.is_null() && !c.is_null() {
            for oy in 0..h {
                let sy = ((oy * 2 + 1) * sh / (h * 2)).min(sh - 1);
                for ox in 0..w {
                    let sx = ((ox * 2 + 1) * sw / (w * 2)).min(sw - 1);
                    let at = (sy / 2) * cs + (sx / 2) * 2;
                    let rgb = studio_709(*y.add(sy * ys + sx), *c.add(at), *c.add(at + 1));
                    rgba[(oy * w + ox) * 4..][..3].copy_from_slice(&rgb);
                }
            }
        }
        CVPixelBufferUnlockBaseAddress(buf, CVPixelBufferLockFlags::ReadOnly);
    }
    // Each picture the camera sends is a new one (it doesn't say otherwise).
    Some(crate::webcam::CameraPreview { width: w as u32, height: h as u32, rgba, seq: f.seq, new_pictures: f.seq })
}

/// A pixel's colour from its studio-range BT.709 luma and chroma.
fn studio_709(y: u8, cb: u8, cr: u8) -> [u8; 3] {
    let luma = (y as f32 - 16.0) * (255.0 / 219.0);
    let (cb, cr) = ((cb as f32 - 128.0) * (255.0 / 224.0), (cr as f32 - 128.0) * (255.0 / 224.0));
    let byte = |v: f32| v.round().clamp(0.0, 255.0) as u8;
    [byte(luma + 1.5748 * cr), byte(luma - 0.1873 * cb - 0.4681 * cr), byte(luma + 1.8556 * cb)]
}

/// The newest picture's size, from camera `device` if it's open.
pub(crate) fn frame_size(device: &str) -> Option<(usize, usize)> {
    let f = CameraFeed::of(device, None)?.latest()?;
    Some((objc2_core_video::CVPixelBufferGetWidth(&f.buffer), objc2_core_video::CVPixelBufferGetHeight(&f.buffer)))
}

#[cfg(test)]
mod tests {
    /// Studio range: 16 is black, 235 white; red, green and blue come back.
    #[test]
    fn camera_colours() {
        use super::studio_709;
        assert_eq!(studio_709(16, 128, 128), [0, 0, 0]);
        assert_eq!(studio_709(235, 128, 128), [255, 255, 255]);
        let near = |a: [u8; 3], b: [u8; 3]| a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= 3);
        assert!(near(studio_709(63, 102, 240), [255, 0, 0]), "{:?}", studio_709(63, 102, 240));
        assert!(near(studio_709(173, 42, 26), [0, 255, 0]), "{:?}", studio_709(173, 42, 26));
        assert!(near(studio_709(32, 240, 118), [0, 0, 255]), "{:?}", studio_709(32, 240, 118));
    }
}
