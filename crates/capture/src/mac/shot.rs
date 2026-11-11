//! Screenshots on macOS (`crate::Recorder::screenshot`): every pixel of the
//! display recorded, or of the window of the game or app recorded, as it
//! shows, the cursor too (as the recording has it). A capture of its own,
//! whether or not one runs: the recording's pictures come from
//! ScreenCaptureKit already scaled to its size and shape.
//!
//! Taken with ScreenCaptureKit's screenshots (`SCScreenshotManager`, what
//! Apple has for a single picture, from macOS 14), or on macOS 13 as the
//! first picture of a stream of its own.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send};
use objc2_core_foundation::CFRetained;
use objc2_core_media::CMSampleBuffer;
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress, kCVPixelFormatType_32BGRA,
};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{SCContentFilter, SCDisplay, SCScreenshotManager, SCStream, SCStreamConfiguration, SCStreamOutput, SCStreamOutputType, SCWindow};

use super::video::{Latest, display_pixels, pick_display, shareable_content};
use super::windows;
use crate::{PendingShot, StillImage, VideoSource};

/// What a screenshot of `video` is of: the display recorded, or the window of
/// the game or app recorded (by `running`'s capture, if one runs, else the
/// listed one in front, or the first that shows). Then taken: it's the
/// picture up now, read once [`PendingShot::finish`] is called.
pub(crate) fn take(video: &VideoSource, running: Option<&Latest>) -> Result<PendingShot> {
    let content = shareable_content()?;
    let (filter, (w, h)) = match video {
        VideoSource::Screen { id, .. } => {
            let display = pick_display(&content, id).context("no display to take")?;
            let filter = unsafe { SCContentFilter::initWithDisplay_excludingWindows(SCContentFilter::alloc(), &display, &NSArray::new()) };
            (filter, display_pixels(&display))
        }
        VideoSource::Apps { ids, .. } => {
            let showing = "none of the games and apps you record is showing";
            let app = match running {
                Some(latest) if latest.is_waiting() => bail!(showing),
                Some(latest) => latest.recorded_app().or_else(|| listed_app(ids)),
                None => listed_app(ids),
            }
            .context(showing)?;
            let all = windows::windows(false);
            let win = windows::find_app_window(&app, &all).filter(|w| w.on_screen).context(showing)?;
            let window: Retained<SCWindow> = unsafe { content.windows() }.iter().find(|w| unsafe { w.windowID() } == win.id).context("its window went away")?;
            let filter = unsafe { SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), &window) };
            let frame = unsafe { window.frame() };
            let scale = window_scale(&filter, &content, frame);
            let px = |points: f64| ((points * scale).round() as usize).max(2);
            (filter, (px(frame.size.width), px(frame.size.height)))
        }
    };
    let config = config(w, h);
    let picture = if objc2::available!(macos = 14.0) { screenshot(&filter, &config) } else { first_picture(&filter, &config)? };
    Ok(PendingShot::new(move || {
        let buffer = picture.recv_timeout(Duration::from_secs(5)).map_err(|_| anyhow::anyhow!("the screen didn't give a picture"))??;
        still(&buffer.0)
    }))
}

/// The listed app to take a picture of while nothing records: the one in
/// front, else the first whose window shows.
fn listed_app(ids: &[String]) -> Option<String> {
    let all = windows::windows(false);
    let shows = |id: &str| windows::find_app_window(id, &all).is_some_and(|w| w.on_screen);
    let front = windows::frontmost().and_then(|(_, front)| ids.iter().find(|a| a.eq_ignore_ascii_case(&front)).cloned()).filter(|a| shows(a));
    front.or_else(|| ids.iter().find(|a| shows(a)).cloned())
}

/// How many pixels a point of `frame` (a window, in points) is: as
/// ScreenCaptureKit has it for the filter (macOS 14), else as the display
/// under the window's middle has it.
fn window_scale(filter: &SCContentFilter, content: &objc2_screen_capture_kit::SCShareableContent, frame: objc2_core_foundation::CGRect) -> f64 {
    if objc2::available!(macos = 14.0) {
        let scale = unsafe { filter.pointPixelScale() } as f64;
        if scale >= 1.0 {
            return scale;
        }
    }
    let (cx, cy) = (frame.origin.x + frame.size.width / 2.0, frame.origin.y + frame.size.height / 2.0);
    let displays = unsafe { content.displays() };
    let on = displays.iter().find(|d| {
        let f = unsafe { d.frame() };
        cx >= f.origin.x && cx < f.origin.x + f.size.width && cy >= f.origin.y && cy < f.origin.y + f.size.height
    });
    on.or_else(|| displays.iter().next()).map_or(1.0, |d: Retained<SCDisplay>| display_pixels(&d).0 as f64 / unsafe { d.width() }.max(1) as f64)
}

/// A picture `w`×`h` big, in sRGB (what the file's taken as), BGRA.
fn config(w: usize, h: usize) -> Retained<SCStreamConfiguration> {
    unsafe {
        let config = SCStreamConfiguration::new();
        config.setWidth(w);
        config.setHeight(h);
        config.setPixelFormat(kCVPixelFormatType_32BGRA);
        config.setColorSpaceName(objc2_core_graphics::kCGColorSpaceSRGB);
        config.setShowsCursor(true);
        config.setScalesToFit(true);
        // A window as it shows, without the shadow round it (as recorded).
        config.setIgnoreShadowsSingleWindow(true);
        config.setQueueDepth(3);
        config
    }
}

/// A CoreVideo buffer handed from ScreenCaptureKit's queue.
struct SendBuffer(CFRetained<CVPixelBuffer>);
unsafe impl Send for SendBuffer {}

/// ScreenCaptureKit's screenshot of `filter` (macOS 14 on).
fn screenshot(filter: &SCContentFilter, config: &SCStreamConfiguration) -> Receiver<Result<SendBuffer>> {
    let (tx, rx) = mpsc::channel();
    let handler = RcBlock::new(move |sample: *mut CMSampleBuffer, err: *mut NSError| {
        let picture = match unsafe { sample.as_ref() }.and_then(|s| unsafe { s.image_buffer() }) {
            Some(buffer) => Ok(SendBuffer(buffer)),
            None => Err(anyhow::anyhow!("{}", unsafe { err.as_ref() }.map_or_else(|| "no picture".to_owned(), |e| e.localizedDescription().to_string()))),
        };
        let _ = tx.send(picture);
    });
    unsafe { SCScreenshotManager::captureSampleBufferWithFilter_configuration_completionHandler(filter, config, Some(&handler)) };
    rx
}

/// The first picture of a stream of `filter` (macOS 13, which has no
/// screenshots of ScreenCaptureKit's): the stream stops once it has it.
fn first_picture(filter: &SCContentFilter, config: &SCStreamConfiguration) -> Result<Receiver<Result<SendBuffer>>> {
    let (tx, rx) = mpsc::channel();
    let (got_tx, got) = mpsc::channel::<SendBuffer>();
    let output = ShotOutput::new(Arc::new(Mutex::new(Some(got_tx))));
    let stream = unsafe { SCStream::initWithFilter_configuration_delegate(SCStream::alloc(), filter, config, None) };
    let queue = DispatchQueue::new("hesteclips.screenshot", None);
    unsafe { stream.addStreamOutput_type_sampleHandlerQueue_error(ProtocolObject::from_ref(&*output), SCStreamOutputType::Screen, Some(&queue)) }
        .map_err(|e| anyhow::anyhow!("can't take the screen: {}", e.localizedDescription()))?;
    crate::sck::start_capture(&stream)?;
    /// The stream, and what it hands its pictures to (kept as long as it).
    struct Running {
        stream: Retained<SCStream>,
        _output: Retained<ShotOutput>,
    }
    unsafe impl Send for Running {}
    let running = Running { stream, _output: output };
    std::thread::spawn(move || {
        let running = running;
        let picture = got.recv_timeout(Duration::from_secs(3)).map_err(|_| anyhow::anyhow!("the screen didn't give a picture"));
        crate::sck::stop_capture(&running.stream);
        let _ = tx.send(picture);
    });
    Ok(rx)
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSObject))]
    #[ivars = Arc<Mutex<Option<Sender<SendBuffer>>>>]
    struct ShotOutput;

    unsafe impl NSObjectProtocol for ShotOutput {}

    unsafe impl SCStreamOutput for ShotOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output(&self, _stream: &SCStream, sample: &CMSampleBuffer, kind: SCStreamOutputType) {
            // Status frames (nothing changed) carry no picture.
            if kind == SCStreamOutputType::Screen
                && let Some(buffer) = unsafe { sample.image_buffer() }
                && let Some(tx) = self.ivars().lock().unwrap().take()
            {
                let _ = tx.send(SendBuffer(buffer));
            }
        }
    }
);

impl ShotOutput {
    fn new(to: Arc<Mutex<Option<Sender<SendBuffer>>>>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(to);
        unsafe { msg_send![super(this), init] }
    }
}

/// A BGRA picture's pixels, opaque.
fn still(buffer: &CVPixelBuffer) -> Result<StillImage> {
    if CVPixelBufferGetPixelFormatType(buffer) != kCVPixelFormatType_32BGRA {
        bail!("the screen gave a picture of another kind");
    }
    let (w, h) = (CVPixelBufferGetWidth(buffer), CVPixelBufferGetHeight(buffer));
    let mut bgra = vec![0u8; w * h * 4];
    unsafe {
        if CVPixelBufferLockBaseAddress(buffer, CVPixelBufferLockFlags::ReadOnly) != 0 {
            bail!("couldn't read the picture");
        }
        let base = CVPixelBufferGetBaseAddress(buffer) as *const u8;
        let stride = CVPixelBufferGetBytesPerRow(buffer);
        if !base.is_null() {
            for row in 0..h {
                bgra[row * w * 4..(row + 1) * w * 4].copy_from_slice(std::slice::from_raw_parts(base.add(row * stride), w * 4));
            }
        }
        CVPixelBufferUnlockBaseAddress(buffer, CVPixelBufferLockFlags::ReadOnly);
    }
    for p in bgra.chunks_exact_mut(4) {
        p[3] = 255;
    }
    Ok(StillImage { width: w as u32, height: h as u32, bgra })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A screenshot of the display, at its own pixels, both ways it's
    /// taken (macOS 14's screenshots, and a stream's first picture as on
    /// macOS 13). Needs Screen Recording allowed to the terminal.
    #[test]
    #[ignore = "takes the screen: cargo test -p capture screenshot_of_the_display -- --ignored --nocapture"]
    fn screenshot_of_the_display() {
        let screen = crate::list_screens().into_iter().next().expect("a screen");
        let content = shareable_content().unwrap();
        let display = pick_display(&content, &screen.id).unwrap();
        let (w, h) = display_pixels(&display);
        let video = VideoSource::Screen { id: screen.id.clone(), hook: Default::default(), method: Default::default() };
        let t = std::time::Instant::now();
        let img = take(&video, None).unwrap().finish().unwrap();
        eprintln!("{}×{} in {} ms", img.width, img.height, t.elapsed().as_millis());
        assert_eq!((img.width as usize, img.height as usize), (w, h), "every pixel of the display");
        assert!(img.bgra.chunks_exact(4).all(|p| p[3] == 255), "opaque");
        assert!(img.bgra.chunks_exact(4).any(|p| p[0] != p[1] || p[1] != p[2] || p[0] > 20), "not black");
        // The way macOS 13 takes it.
        let filter = unsafe { SCContentFilter::initWithDisplay_excludingWindows(SCContentFilter::alloc(), &display, &NSArray::new()) };
        let t = std::time::Instant::now();
        let picture = first_picture(&filter, &config(w, h)).unwrap().recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
        let img = still(&picture.0).unwrap();
        eprintln!("a stream's first: {}×{} in {} ms", img.width, img.height, t.elapsed().as_millis());
        assert_eq!((img.width as usize, img.height as usize), (w, h));
        // The app in front's window (the terminal running this), at its
        // own pixels: twice its points on a Retina display.
        let (_, front) = windows::frontmost().expect("an app in front");
        let win = windows::find_app_window(&front, &windows::windows(false)).expect("its window");
        let apps = VideoSource::Apps { ids: vec![front.clone()], away_when_unfocused: false, hook: Default::default() };
        let img = take(&apps, None).unwrap().finish().unwrap();
        let scale = w as f64 / unsafe { display.width() } as f64;
        eprintln!("{front}'s window: {}×{} points, {}×{} pixels", win.width, win.height, img.width, img.height);
        assert_eq!((img.width as f64, img.height as f64), ((win.width * scale).round(), (win.height * scale).round()));
    }
}
