//! Webcams, with AVFoundation's capture session: the camera delivers NV12
//! frames in IOSurfaces (its own format, converted on the media engine when
//! it sends something else), which the compositor opens as Metal textures
//! without a copy (`super::gpu`).
//!
//! A webcam stays open for as long as it's set up ([`keep_open`]), previewed
//! and recorded or not, as on Windows. macOS has no driver settings window to
//! open (`open_settings` does nothing); its camera controls live in Control
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
use crate::webcam::{Format, Status, set_formats, set_status};

/// One camera picture: NV12 (studio range, BT.709) in an IOSurface.
pub(crate) struct Frame {
    pub seq: u64,
    pub buffer: CFRetained<CVPixelBuffer>,
}
// CoreVideo buffers are thread-safe reference-counted objects; nothing
// writes to one after the camera hands it over.
unsafe impl Send for Frame {}
unsafe impl Sync for Frame {}

/// The camera that's open, delivering pictures.
pub(crate) struct CameraFeed {
    pub device: String,
    pub format: Option<Format>,
    frame: Mutex<Option<Arc<Frame>>>,
    seq: AtomicU64,
    stop: AtomicBool,
}

static CURRENT: Mutex<Option<Arc<CameraFeed>>> = Mutex::new(None);

impl CameraFeed {
    /// The newest picture.
    pub(crate) fn latest(&self) -> Option<Arc<Frame>> {
        self.frame.lock().unwrap().clone()
    }

    /// The open camera, if it's this one in this format.
    pub(crate) fn current(device: &str, format: Option<Format>) -> Option<Arc<CameraFeed>> {
        CURRENT.lock().unwrap().clone().filter(|f| f.device == device && f.format == format && !f.stop.load(Ordering::Relaxed))
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

/// Keep this camera (device, format) open, or close it with `None`. Doesn't wait.
pub fn keep_open(want: Option<(String, Option<Format>)>) {
    let mut current = CURRENT.lock().unwrap();
    let same = match (&*current, &want) {
        (Some(f), Some((d, fm))) => f.device == *d && f.format == *fm,
        (None, None) => true,
        _ => false,
    };
    if same {
        return;
    }
    if let Some(old) = current.take() {
        old.stop.store(true, Ordering::Relaxed); // its thread closes it
    }
    let Some((device, format)) = want else {
        set_status(Status::Off);
        return;
    };
    let feed = Arc::new(CameraFeed { device, format, frame: Mutex::new(None), seq: AtomicU64::new(0), stop: AtomicBool::new(false) });
    *current = Some(feed.clone());
    thread::spawn(move || run(feed));
}

/// Open the camera and keep it open until told to stop, reopening it when it
/// goes away (unplugged, taken by another app) and comes back.
fn run(feed: Arc<CameraFeed>) {
    while !feed.stop.load(Ordering::Relaxed) {
        set_status(Status::Opening);
        let opened = open_session(&feed);
        let (session, _delegate, (width, height)) = match opened {
            Ok(s) => s,
            Err(e) => {
                set_status(Status::Unavailable(format!("{e:#}")));
                wait_or_stop(&feed, Duration::from_secs(3));
                continue;
            }
        };
        set_status(Status::Live { width, height });
        // The session runs on its own; watch for it stopping.
        while !feed.stop.load(Ordering::Relaxed) && unsafe { session.isRunning() } {
            thread::sleep(Duration::from_millis(100));
        }
        unsafe { session.stopRunning() };
        if !feed.stop.load(Ordering::Relaxed) {
            set_status(Status::Unavailable("the camera stopped".into()));
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
    let auto_key = |f: &Format| {
        let fits = f.width * f.height <= 1920 * 1080;
        (fits && f.fps() >= 29.5, fits, f.width * f.height, (f.fps() * 100.0) as u32)
    };
    let chosen = feed
        .format
        .and_then(|want| offered.iter().find(|(f, ..)| *f == want).or_else(|| offered.iter().find(|(f, ..)| f.label() == want.label())))
        .or_else(|| offered.iter().max_by_key(|(f, ..)| auto_key(f)));
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
                let seq = feed.seq.fetch_add(1, Ordering::Relaxed) + 1;
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

/// What a running capture draws: the open camera's newest picture, picking
/// the camera up again if it's reopened (a new format, or after a dropout).
pub(crate) struct CameraLayer {
    feed: Option<Arc<CameraFeed>>,
    device: String,
    format: Option<Format>,
}

impl CameraLayer {
    pub(crate) fn new(w: &crate::webcam::Webcam) -> Self {
        let hidden = w.placement.lock().unwrap().is_hidden();
        if !hidden && CameraFeed::current(&w.device, w.format).is_none() {
            keep_open(Some((w.device.clone(), w.format)));
        }
        Self { feed: CameraFeed::current(&w.device, w.format), device: w.device.clone(), format: w.format }
    }

    /// The newest picture, if the camera is delivering.
    pub(crate) fn latest(&mut self) -> Option<Arc<Frame>> {
        if self.feed.as_ref().is_none_or(|f| f.stop.load(Ordering::Relaxed)) {
            self.feed = CameraFeed::current(&self.device, self.format);
        }
        self.feed.as_ref()?.latest()
    }
}
