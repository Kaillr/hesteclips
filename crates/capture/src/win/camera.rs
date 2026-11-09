//! Webcams, with Media Foundation's source reader: it decodes whatever the
//! camera sends (MJPEG, YUY2, NV12) into BGRA, which is uploaded to the GPU
//! for the compositor (`d3d::Converter`).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use windows::Win32::Media::DirectShow::{IAMCameraControl, IAMVideoProcAmp};
use windows::Win32::Media::KernelStreaming::{
    IKsControl, KSIDENTIFIER, KSIDENTIFIER_0_0, KSPROPERTY_DESCRIPTION, KSPROPERTY_MEMBERSHEADER, KSPROPERTY_TYPE_BASICSUPPORT, KSPROPERTY_TYPE_DEFAULTVALUES, KSPROPERTY_TYPE_GET, KSPROPERTY_TYPE_SET, KSPROPERTY_VIDEOPROCAMP_POWERLINE_FREQUENCY,
    KSPROPERTY_VIDEOPROCAMP_S, PROPSETID_VIDCAP_VIDEOPROCAMP,
};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::core::Interface;

use super::d3d::{Gpu, Latest};
use super::h264::take_activates;
use super::system::com_init;
use crate::Device;
use crate::webcam::{Status, set_status_of};

const STREAM: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

/// Cameras, by symbolic link (stable while plugged in), with their names.
pub fn list_cameras() -> Vec<Device> {
    com_init();
    let _ = super::system::mf_startup();
    devices().into_iter().filter_map(|a| Some(Device { id: string(&a, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK)?, name: string(&a, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME)? })).collect()
}

fn devices() -> Vec<IMFActivate> {
    unsafe {
        let mut attrs = None;
        if MFCreateAttributes(&mut attrs, 1).is_err() {
            return Vec::new();
        }
        let Some(attrs) = attrs else { return Vec::new() };
        if attrs.SetGUID(&MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID).is_err() {
            return Vec::new();
        }
        let mut list = std::ptr::null_mut();
        let mut count = 0;
        if MFEnumDeviceSources(&attrs, &mut list, &mut count).is_err() {
            return Vec::new();
        }
        take_activates(list, count)
    }
}

fn string(a: &IMFActivate, key: &windows::core::GUID) -> Option<String> {
    unsafe {
        let mut p = windows::core::PWSTR::null();
        let mut len = 0;
        a.GetAllocatedString(key, &mut p, &mut len).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }
}

/// One frame from the camera: BGRA rows, top to bottom, no padding.
pub(crate) struct Frame {
    pub seq: u64,
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

/// A camera, kept open for as long as a webcam is set up — not just while
/// something previews or records it. Closing a camera can reset what was set
/// in its own settings window (exposure, focus…), and some, like the C922, do.
/// Captures and previews take its newest frame ([`CameraFeed::latest`]) and
/// upload it to their own GPU.
pub(crate) struct CameraFeed {
    pub device: String,
    pub format: Option<crate::webcam::Format>,
    frame: Mutex<Option<Arc<Frame>>>,
    /// Its settings, while it's open.
    settings: Mutex<Option<Arc<Settings>>>,
    /// How many of its pictures were new, not repeats of the one before (as
    /// a camera sends while its exposure is longer than a frame): its real
    /// frame rate.
    new_pictures: AtomicU64,
    stop: AtomicBool,
}

/// The cameras kept open.
static OPEN: Mutex<Vec<Arc<CameraFeed>>> = Mutex::new(Vec::new());

/// Pictures from any camera so far, numbering them: going on from camera to
/// camera and format to format, so a reader waiting for one newer than the
/// last it had (the Camera page) gets the next camera's at once.
static PICTURES: AtomicU64 = AtomicU64::new(0);

impl CameraFeed {
    pub(crate) fn latest(&self) -> Option<Arc<Frame>> {
        self.frame.lock().unwrap().clone()
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// Camera `device`, if it's open (in whatever format).
    fn of(device: &str) -> Option<Arc<CameraFeed>> {
        OPEN.lock().unwrap().iter().find(|f| f.device == device && !f.stopped()).cloned()
    }

    fn start(device: String, format: Option<crate::webcam::Format>) -> Arc<CameraFeed> {
        let feed = Arc::new(CameraFeed { device, format, frame: Mutex::new(None), settings: Mutex::new(None), new_pictures: AtomicU64::new(0), stop: AtomicBool::new(false) });
        let run_feed = feed.clone();
        thread::spawn(move || run(run_feed));
        feed
    }

    /// Close it: its thread does, and says it's off.
    fn close(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Keep cameras `want` (device, format) open, and close the rest. Opening
/// and closing happen in the background; asking for what's already open
/// does nothing. A camera asked for twice is opened once, in the first
/// format asked for.
pub fn keep_open(want: Vec<(String, Option<crate::webcam::Format>)>) {
    let mut open = OPEN.lock().unwrap();
    let mut kept: Vec<Arc<CameraFeed>> = Vec::new();
    for (device, format) in want {
        if kept.iter().any(|f| f.device == device) {
            continue;
        }
        match open.iter().position(|f| f.device == device && f.format == format) {
            Some(i) => kept.push(open.remove(i)),
            None => kept.push(CameraFeed::start(device, format)),
        }
    }
    // The rest close (another format of one kept too: its new one's opening).
    for old in open.drain(..) {
        old.close();
    }
    *open = kept;
}

/// Open camera `device` in `format`, if it isn't: the others stay as they are.
fn open_too(device: &str, format: Option<crate::webcam::Format>) {
    let mut open = OPEN.lock().unwrap();
    if open.iter().any(|f| f.device == device && f.format == format) {
        return;
    }
    open.retain(|f| {
        let other = f.device != device;
        if !other {
            f.close();
        }
        other
    });
    open.push(CameraFeed::start(device.to_owned(), format));
}

/// Camera `device`'s newest picture, if it's open and has one newer than
/// picture `newer_than`: shrunk to at most `max_width` wide (each pixel the
/// one under it), in RGBA, for showing it on its own (the webcam's crop page).
pub(crate) fn picture(device: &str, max_width: u32, newer_than: u64) -> Option<crate::webcam::CameraPreview> {
    let feed = CameraFeed::of(device)?;
    let f = feed.latest().filter(|f| f.seq > newer_than)?;
    let (sw, sh) = (f.width as usize, f.height as usize);
    if sw == 0 || sh == 0 || f.bgra.len() < sw * sh * 4 {
        return None;
    }
    let scale = (max_width as f64 / sw as f64).min(1.0);
    let (w, h) = (((sw as f64 * scale).round() as usize).max(1), ((sh as f64 * scale).round() as usize).max(1));
    let mut rgba = vec![255u8; w * h * 4];
    for oy in 0..h {
        let sy = ((oy * 2 + 1) * sh / (h * 2)).min(sh - 1);
        for ox in 0..w {
            let sx = ((ox * 2 + 1) * sw / (w * 2)).min(sw - 1);
            let from = &f.bgra[(sy * sw + sx) * 4..][..3];
            let to = &mut rgba[(oy * w + ox) * 4..][..3];
            (to[0], to[1], to[2]) = (from[2], from[1], from[0]);
        }
    }
    Some(crate::webcam::CameraPreview { width: w as u32, height: h as u32, rgba, seq: f.seq, new_pictures: feed.new_pictures.load(Ordering::Relaxed) })
}

/// Open the camera and keep reading it until told to stop. If it can't be
/// opened (unplugged, in use) or stops, try again every few seconds.
fn run(feed: Arc<CameraFeed>) {
    com_init();
    // What it says of itself, unless it's been closed: a camera closed and
    // opened again in another format says so through its new one.
    let say = |s: Status| {
        if !feed.stopped() {
            set_status_of(&feed.device, s);
        }
    };
    while !feed.stopped() {
        say(Status::Opening);
        let (reader, source, width, height) = match open_reader(&feed.device, feed.format) {
            Ok(r) => r,
            Err(e) => {
                say(Status::Unavailable(format!("{e:#}")));
                wait_or_stop(&feed, Duration::from_secs(3));
                continue;
            }
        };
        // Its settings, and what was set for it before put back on (it
        // forgets when it's unplugged).
        *feed.settings.lock().unwrap() = Some(Arc::new(Settings { amp: source.cast().ok(), cam: source.cast().ok(), ks: source.cast().ok() }));
        drop(source);
        crate::webcam::apply_remembered(&feed.device);
        say(Status::Live { width, height });
        let mut rows = Vec::new();
        let mut last_print = None;
        while !feed.stopped() {
            let mut flags = 0u32;
            let mut sample = None;
            // Blocks until the camera's next frame.
            if let Err(e) = unsafe { reader.ReadSample(STREAM, 0, None, Some(&mut flags), None, Some(&mut sample)) } {
                say(Status::Unavailable(read_error(&e)));
                break;
            }
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                say(Status::Unavailable("the camera stopped".into()));
                break;
            }
            let Some(sample) = sample else { continue };
            match read_frame(&sample, width, height, &mut rows) {
                Ok(()) => {
                    let seq = PICTURES.fetch_add(1, Ordering::Relaxed) + 1;
                    let print = crate::webcam::fingerprint(&rows);
                    if last_print != Some(print) {
                        feed.new_pictures.fetch_add(1, Ordering::Relaxed);
                        last_print = Some(print);
                    }
                    let bgra = std::mem::take(&mut rows);
                    *feed.frame.lock().unwrap() = Some(Arc::new(Frame { seq, width, height, bgra }));
                }
                Err(e) => eprintln!("webcam frame: {e:#}"),
            }
        }
        *feed.settings.lock().unwrap() = None;
        drop(reader);
        if !feed.stopped() {
            wait_or_stop(&feed, Duration::from_secs(3));
        }
    }
    // Off, unless it's open again by now (in another format).
    if CameraFeed::of(&feed.device).is_none() {
        set_status_of(&feed.device, Status::Off);
    }
}

/// What's wrong when a camera that opened gives no pictures, in words: one
/// another app has (or took over) is said to be in use, not in Media
/// Foundation's words ("Hardware MFT failed to start streaming due to lack
/// of hardware resources", the C922's while another app streams it).
fn read_error(e: &windows::core::Error) -> String {
    /// `MF_E_VIDEO_RECORDING_DEVICE_INUSE`, which the windows crate doesn't name.
    const IN_USE: windows::core::HRESULT = windows::core::HRESULT(0xC00D3EA2_u32 as _);
    let code = e.code();
    if code == MF_E_HW_MFT_FAILED_START_STREAMING || code == IN_USE || code == MF_E_VIDEO_RECORDING_DEVICE_PREEMPTED {
        return "another app is using the camera. Close it there, and it comes back here by itself".to_owned();
    }
    e.message()
}

fn wait_or_stop(feed: &CameraFeed, d: Duration) {
    let until = std::time::Instant::now() + d;
    while std::time::Instant::now() < until && !feed.stopped() {
        thread::sleep(Duration::from_millis(50));
    }
}

/// A webcam layer of a running capture: the newest picture of its camera
/// (kept open by the app), turned as placed, in a GPU texture for the
/// compositor. Another format, the webcam switched off and on, a dropout:
/// it follows along without the capture restarting.
pub(crate) struct CameraLayer {
    device: String,
    feed: Option<Arc<CameraFeed>>,
    /// Square, as big as a camera picture can be turned either way: any
    /// format fits, and only its top-left `content` is drawn.
    pub latest: Arc<Latest>,
    /// The picture last uploaded, and how it was turned.
    last: (u64, u8),
    turned: Vec<u8>,
}

/// The biggest camera picture side drawn, turned or not: 1080p either way
/// fits; a bigger picture is cut.
const MAX_SIDE: u32 = 1920;

impl CameraLayer {
    /// For camera `device`, opening it in `format` if nothing has (the app
    /// keeps it open itself), unless it's switched off (`hidden`): then it
    /// waits for the app to.
    pub(crate) fn new(gpu: &Gpu, device: &str, format: Option<crate::webcam::Format>, hidden: bool) -> Result<Self> {
        if !hidden && CameraFeed::of(device).is_none() {
            open_too(device, format);
        }
        Ok(Self { device: device.to_owned(), feed: None, latest: Latest::new(gpu, MAX_SIDE, MAX_SIDE)?, last: (0, 0), turned: Vec::new() })
    }

    /// Upload the camera's newest frame, turned `turns` quarters clockwise,
    /// if it has one we haven't (or it's turned another way now).
    pub(crate) fn pull(&mut self, turns: u8) {
        if self.feed.as_ref().is_none_or(|f| f.stopped()) {
            self.feed = CameraFeed::of(&self.device);
            self.last = (0, 0);
        }
        let Some(f) = self.feed.as_ref().and_then(|feed| feed.latest()) else { return };
        let turns = turns % 4;
        if (f.seq, turns) == self.last {
            return;
        }
        self.last = (f.seq, turns);
        if turns == 0 {
            self.latest.upload_bgra(f.bgra.as_ptr(), f.width * 4, f.width, f.height);
            return;
        }
        let (w, h) = turn_bgra(&f.bgra, f.width, f.height, turns, &mut self.turned);
        self.latest.upload_bgra(self.turned.as_ptr(), w * 4, w, h);
    }
}

/// `src` (`w`×`h` BGRA rows) turned `turns` quarters clockwise into `out`;
/// returns the new size.
fn turn_bgra(src: &[u8], w: u32, h: u32, turns: u8, out: &mut Vec<u8>) -> (u32, u32) {
    let (w, h) = (w as usize, h as usize);
    let (ow, oh) = if turns % 2 == 1 { (h, w) } else { (w, h) };
    out.resize(ow * oh * 4, 0);
    let px = |x: usize, y: usize| &src[(y * w + x) * 4..][..4];
    for oy in 0..oh {
        let row = &mut out[oy * ow * 4..][..ow * 4];
        for ox in 0..ow {
            // Where this output pixel was in the camera's picture.
            let (x, y) = match turns {
                1 => (oy, h - 1 - ox),
                2 => (w - 1 - ox, h - 1 - oy),
                _ => (w - 1 - oy, ox),
            };
            row[ox * 4..ox * 4 + 4].copy_from_slice(px(x, y));
        }
    }
    (ow as u32, oh as u32)
}

#[cfg(test)]
mod tests {
    #[test]
    fn turns_a_picture() {
        // 2x1: red, blue. Clockwise it's 1x2: red on top.
        let src = [0, 0, 255, 255, 255, 0, 0, 255];
        let mut out = Vec::new();
        assert_eq!(super::turn_bgra(&src, 2, 1, 1, &mut out), (1, 2));
        assert_eq!(out, [0, 0, 255, 255, 255, 0, 0, 255]);
        assert_eq!(super::turn_bgra(&src, 2, 1, 3, &mut out), (1, 2));
        assert_eq!(out, [255, 0, 0, 255, 0, 0, 255, 255]);
        assert_eq!(super::turn_bgra(&src, 2, 1, 2, &mut out), (2, 1));
        assert_eq!(out, [255, 0, 0, 255, 0, 0, 255, 255]);
    }
}

/// A source reader for `device` delivering BGRA, in `format` if the camera
/// offers it, else the best one. Also publishes the formats it offers.
fn open_reader(device: &str, format: Option<crate::webcam::Format>) -> Result<(IMFSourceReader, IMFMediaSource, u32, u32)> {
    com_init();
    super::system::mf_startup()?;
    let activate = devices()
        .into_iter()
        .find(|a| string(a, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK).as_deref() == Some(device))
        .context("the camera isn't connected")?;
    unsafe {
        let source: IMFMediaSource = activate.ActivateObject().map_err(|e| anyhow!("couldn't open the camera ({}) — another app may be using it", e.message()))?;
        let mut attrs = None;
        MFCreateAttributes(&mut attrs, 1)?;
        let attrs = attrs.context("no attributes")?;
        // Decode and convert whatever the camera sends into what we ask for.
        attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)?;
        let reader = MFCreateSourceReaderFromMediaSource(&source, &attrs)?;

        // Every format the camera offers. Without a choice (or if the chosen one
        // is gone), the biggest picture up to 1080p that does at least 30 fps.
        let mut offered: Vec<(crate::webcam::Format, IMFMediaType)> = Vec::new();
        for i in 0.. {
            let t = match reader.GetNativeMediaType(STREAM, i) {
                Ok(t) => t,
                Err(e) if e.code() == MF_E_NO_MORE_TYPES => break,
                Err(e) => return Err(e.into()),
            };
            let size = t.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
            let rate = t.GetUINT64(&MF_MT_FRAME_RATE).unwrap_or(0);
            let f = crate::webcam::Format {
                width: (size >> 32) as u32,
                height: size as u32,
                fps_num: (rate >> 32) as u32,
                fps_den: (rate as u32).max(1),
            };
            if f.width > 0 && f.height > 0 && f.fps_num > 0 {
                offered.push((f, t));
            }
        }
        let mut list: Vec<crate::webcam::Format> = offered.iter().map(|(f, _)| *f).collect();
        list.sort_by(|a, b| (b.width * b.height, b.fps()).partial_cmp(&(a.width * a.height, a.fps())).unwrap());
        // The same size and rate comes once per pixel format (MJPEG, YUY2, NV12),
        // sometimes as a different fraction (30/1, 10000000/333333): one entry
        // per thing you'd tell apart.
        list.dedup_by(|a, b| a.label() == b.label());
        crate::webcam::set_formats(device, list);
        let chosen = format
            .and_then(|want| offered.iter().find(|(f, _)| *f == want).or_else(|| offered.iter().find(|(f, _)| f.label() == want.label())))
            .or_else(|| offered.iter().max_by_key(|(f, _)| f.auto_rank()));
        let (chosen, native) = chosen.context("the camera offers no usable format")?;
        let (width, height) = (chosen.width, chosen.height);
        reader.SetCurrentMediaType(STREAM, None, native)?;

        let out = MFCreateMediaType()?;
        out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
        reader.SetCurrentMediaType(STREAM, None, &out).map_err(|e| anyhow!("the camera's pictures can't be converted ({})", e.message()))?;
        reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
        reader.SetStreamSelection(STREAM, true)?;
        Ok((reader, source, width, height))
    }
}

/// One frame as tight BGRA rows, top to bottom, into `out`.
fn read_frame(sample: &IMFSample, width: u32, height: u32, out: &mut Vec<u8>) -> Result<()> {
    let row = width as usize * 4;
    out.resize(row * height as usize, 0);
    unsafe {
        let buffer = sample.GetBufferByIndex(0)?;
        if let Ok(b2) = buffer.cast::<IMF2DBuffer>() {
            // A 2D buffer knows its pitch, which is negative for bottom-up RGB.
            let mut scan0 = std::ptr::null_mut();
            let mut pitch = 0i32;
            b2.Lock2D(&mut scan0, &mut pitch)?;
            for y in 0..height as usize {
                let src = scan0.offset(y as isize * pitch as isize);
                std::ptr::copy_nonoverlapping(src, out[y * row..].as_mut_ptr(), row);
            }
            b2.Unlock2D()?;
        } else {
            let mut ptr = std::ptr::null_mut();
            let mut len = 0u32;
            buffer.Lock(&mut ptr, None, Some(&mut len))?;
            if (len as usize) < row * height as usize {
                buffer.Unlock()?;
                bail!("short camera frame");
            }
            // MF's plain RGB32 is bottom-up.
            for y in 0..height as usize {
                let src = ptr.add((height as usize - 1 - y) * row);
                std::ptr::copy_nonoverlapping(src, out[y * row..].as_mut_ptr(), row);
            }
            buffer.Unlock()?;
        }
    }
    Ok(())
}

/// An open camera's settings: DirectShow's picture settings (brightness,
/// white balance…) and camera settings (exposure, focus, zoom…), which Media
/// Foundation's camera source also has, and the driver's own properties for
/// what those don't list. Changes apply to the camera, in every app, as its
/// driver's own window does them.
struct Settings {
    amp: Option<IAMVideoProcAmp>,
    cam: Option<IAMCameraControl>,
    ks: Option<IKsControl>,
}

// SAFETY: Media Foundation's camera source is free-threaded, and it's only
// used from threads in the multithreaded apartment (see `in_mta`).
unsafe impl Send for Settings {}
unsafe impl Sync for Settings {}

/// Which a setting is in.
#[derive(Clone, Copy)]
enum Of {
    Amp,
    Cam,
    /// A picture setting DirectShow's list stops short of, asked of the
    /// driver itself (`PROPSETID_VIDCAP_VIDEOPROCAMP`).
    Ks,
}

/// A setting, by its id (as V4L2 numbers it, see `webcam::cid`), with its
/// automatic mode's switch, if the app offers one.
struct Setting {
    id: u32,
    of: Of,
    /// Its `VideoProcAmpProperty`, `CameraControlProperty`, or
    /// `KSPROPERTY_VIDCAP_VIDEOPROCAMP`.
    prop: i32,
    name: &'static str,
    auto: Option<(u32, &'static str)>,
}

/// Ids for settings V4L2 has none of.
const COLOUR: u32 = 0x0800_0006;
const ROLL: u32 = 0x0800_0102;

const SETTINGS: &[Setting] = {
    use crate::webcam::cid::*;
    const fn amp(id: u32, prop: i32, name: &'static str) -> Setting {
        Setting { id, of: Of::Amp, prop, name, auto: None }
    }
    const fn cam(id: u32, prop: i32, name: &'static str) -> Setting {
        Setting { id, of: Of::Cam, prop, name, auto: None }
    }
    &[
        amp(BRIGHTNESS, 0, "Brightness"),
        amp(CONTRAST, 1, "Contrast"),
        amp(HUE, 2, "Hue"),
        amp(SATURATION, 3, "Saturation"),
        amp(SHARPNESS, 4, "Sharpness"),
        amp(GAMMA, 5, "Gamma"),
        amp(COLOUR, 6, "Colour"),
        Setting { auto: Some((AUTO_WHITE_BALANCE, "White balance, automatic")), ..amp(WHITE_BALANCE, 7, "White balance") },
        amp(BACKLIGHT, 8, "Backlight compensation"),
        amp(GAIN, 9, "Gain"),
        Setting { of: Of::Ks, ..amp(POWER_LINE, KSPROPERTY_VIDEOPROCAMP_POWERLINE_FREQUENCY.0, "Power line frequency") },
        cam(PAN, 0, "Pan"),
        cam(TILT, 1, "Tilt"),
        cam(ROLL, 2, "Roll"),
        cam(ZOOM, 3, "Zoom"),
        Setting { auto: Some((AUTO_EXPOSURE, "Exposure, automatic")), ..cam(EXPOSURE, 4, "Exposure") },
        cam(IRIS, 5, "Iris"),
        Setting { auto: Some((AUTO_FOCUS, "Focus, automatic")), ..cam(FOCUS, 6, "Focus") },
    ]
};

/// `VideoProcAmp_Flags_Auto` and `_Manual` (the camera's the same).
const AUTO: i32 = 1;
const MANUAL: i32 = 2;

impl Settings {
    /// min, max, step, default, and whether it can be automatic or set.
    fn range(&self, s: &Setting) -> Option<(i32, i32, i32, i32, i32)> {
        let (mut min, mut max, mut step, mut default, mut caps) = (0, 0, 0, 0, 0);
        unsafe {
            match s.of {
                Of::Amp => self.amp.as_ref()?.GetRange(s.prop, &mut min, &mut max, &mut step, &mut default, &mut caps),
                Of::Cam => self.cam.as_ref()?.GetRange(s.prop, &mut min, &mut max, &mut step, &mut default, &mut caps),
                Of::Ks => return self.ks_range(s.prop),
            }
        }
        .ok()?;
        (max >= min).then_some((min, max, step, default, caps))
    }

    /// Its value, and whether it's automatic now.
    fn get(&self, s: &Setting) -> Option<(i32, bool)> {
        let (mut value, mut flags) = (0, 0);
        unsafe {
            match s.of {
                Of::Amp => self.amp.as_ref()?.Get(s.prop, &mut value, &mut flags),
                Of::Cam => self.cam.as_ref()?.Get(s.prop, &mut value, &mut flags),
                Of::Ks => {
                    let got = self.ks_property(s.prop, KSPROPERTY_TYPE_GET, 0)?;
                    (value, flags) = (got.Value, got.Flags as i32);
                    Ok(())
                }
            }
        }
        .ok()?;
        Some((value, flags & AUTO != 0))
    }

    fn set(&self, s: &Setting, value: i32, flags: i32) -> Result<()> {
        unsafe {
            match s.of {
                Of::Amp => self.amp.as_ref().context("the camera has no picture settings")?.Set(s.prop, value, flags),
                Of::Cam => self.cam.as_ref().context("the camera has no camera settings")?.Set(s.prop, value, flags),
                Of::Ks => {
                    self.ks_property(s.prop, KSPROPERTY_TYPE_SET, value).context("the camera didn't take it")?;
                    Ok(())
                }
            }
        }
        .map_err(|e| anyhow!("the camera didn't take it ({})", e.message()))
    }

    /// Get (`KSPROPERTY_TYPE_GET`) or set picture setting `prop` of the
    /// driver's own.
    fn ks_property(&self, prop: i32, kind: u32, value: i32) -> Option<KSPROPERTY_VIDEOPROCAMP_S> {
        let ks = self.ks.as_ref()?;
        let mut p = KSPROPERTY_VIDEOPROCAMP_S { Value: value, Flags: MANUAL as u32, ..Default::default() };
        p.Property.Anonymous.Anonymous = KSIDENTIFIER_0_0 { Set: PROPSETID_VIDCAP_VIDEOPROCAMP, Id: prop as u32, Flags: kind };
        let mut out = p;
        let size = std::mem::size_of::<KSPROPERTY_VIDEOPROCAMP_S>() as u32;
        let mut got = 0;
        unsafe { ks.KsProperty(&p.Property, size, (&raw mut out).cast(), size, &mut got) }.ok()?;
        Some(out)
    }

    /// The range and default of picture setting `prop` of the driver's own,
    /// as `range` has them: its basic support (a description, a members
    /// header, then the stepping) and its default (the same, then the value).
    fn ks_range(&self, prop: i32) -> Option<(i32, i32, i32, i32, i32)> {
        let ks = self.ks.as_ref()?;
        let ask = |kind: u32| -> Option<Vec<u8>> {
            let mut p = KSIDENTIFIER::default();
            p.Anonymous.Anonymous = KSIDENTIFIER_0_0 { Set: PROPSETID_VIDCAP_VIDEOPROCAMP, Id: prop as u32, Flags: kind };
            let mut buf = vec![0u8; 256];
            let mut got = 0;
            unsafe { ks.KsProperty(&p, std::mem::size_of::<KSPROPERTY_VIDEOPROCAMP_S>() as u32, buf.as_mut_ptr().cast(), buf.len() as u32, &mut got) }.ok()?;
            buf.truncate(got as usize);
            Some(buf)
        };
        let head = std::mem::size_of::<KSPROPERTY_DESCRIPTION>() + std::mem::size_of::<KSPROPERTY_MEMBERSHEADER>();
        let read = |b: &[u8], at: usize| Some(i32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?));
        let support = ask(KSPROPERTY_TYPE_BASICSUPPORT)?;
        // KSPROPERTY_STEPPING_LONG: the step, a reserved word, then the bounds.
        let (step, min, max) = (read(&support, head)?, read(&support, head + 8)?, read(&support, head + 12)?);
        let default = ask(KSPROPERTY_TYPE_DEFAULTVALUES).and_then(|d| read(&d, head)).unwrap_or(min);
        (max >= min).then_some((min, max, step, default, MANUAL))
    }
}

/// Run `f` on a thread of the multithreaded apartment, where the camera was
/// opened (the UI's thread is single-threaded).
fn in_mta<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    thread::scope(|s| {
        s.spawn(|| {
            com_init();
            f()
        })
        .join()
        .unwrap()
    })
}

/// The settings of camera `device`, if it's open.
fn settings_of(device: &str) -> Option<Arc<Settings>> {
    let feed = CameraFeed::of(device)?;
    feed.settings.lock().unwrap().clone()
}

/// An exposure time as DirectShow has it, in powers of two of a second.
fn exposure_label(log2: i32) -> String {
    let secs = 2f64.powi(log2);
    if secs >= 1.0 { format!("{secs:.0} s") } else { format!("{:.1} ms", secs * 1000.0) }
}

/// Whether a camera's exposure range (`min` to `max`) can be DirectShow's,
/// powers of two of a second: from 1/1,000,000 s to 32 s at most. Some
/// drivers give steps of their own instead (the PS3 Eye's: 0 to 255),
/// which read as powers of two were times of 2^200 seconds.
fn exposure_in_log2(min: i32, max: i32) -> bool {
    min >= -20 && max <= 5
}

/// The settings camera `device` offers, with their values now: none until
/// it's open. Automatic modes are switches of their own, before what they
/// take over.
pub(crate) fn controls(device: &str) -> Vec<crate::webcam::CameraControl> {
    use crate::webcam::{CameraControl, ControlKind, cid};
    let Some(settings) = settings_of(device) else { return Vec::new() };
    in_mta(|| {
        let mut out = Vec::new();
        for s in SETTINGS {
            let Some((min, max, step, default, caps)) = settings.range(s) else { continue };
            if caps & MANUAL == 0 {
                continue;
            }
            let (value, auto) = settings.get(s).unwrap_or((default, false));
            let switch = s.auto.filter(|_| caps & AUTO != 0);
            if let Some((id, name)) = switch {
                // Cameras start out automatic.
                out.push(CameraControl { id, name: name.to_owned(), kind: ControlKind::Bool, value: i64::from(auto), default: 1, inactive: false });
            }
            // An exposure that isn't a time: in its own steps, as Linux has
            // such a camera's (`cid::EXPOSURE_STEPS`).
            if s.id == cid::EXPOSURE && !exposure_in_log2(min, max) {
                let kind = ControlKind::Int { min: min.into(), max: max.into(), step: step.max(1).into() };
                out.push(CameraControl { id: cid::EXPOSURE_STEPS, name: "Exposure".to_owned(), kind, value: value.into(), default: default.into(), inactive: switch.is_some() && auto });
                continue;
            }
            let kind = match s.id {
                cid::EXPOSURE => ControlKind::Menu((min..=max).map(|v| (i64::from(v), exposure_label(v))).collect()),
                cid::POWER_LINE => {
                    let names = ["Off", "50 Hz", "60 Hz", "Automatic"];
                    ControlKind::Menu((min..=max).filter_map(|v| Some((i64::from(v), names.get(usize::try_from(v).ok()?)?.to_string()))).collect())
                }
                _ => ControlKind::Int { min: min.into(), max: max.into(), step: step.max(1).into() },
            };
            out.push(CameraControl { id: s.id, name: s.name.to_owned(), kind, value: value.into(), default: default.into(), inactive: switch.is_some() && auto });
        }
        out
    })
}

/// Set camera `device`'s setting `id` (or an automatic mode's switch) to
/// `value`; it applies at once, in every app. One its automatic mode has
/// taken over is refused (as V4L2 does).
pub(crate) fn set_control(device: &str, id: u32, value: i64) -> Result<()> {
    let settings = settings_of(device).context("the camera isn't open")?;
    // Its exposure in its own steps is still its exposure.
    let id = if id == crate::webcam::cid::EXPOSURE_STEPS { crate::webcam::cid::EXPOSURE } else { id };
    let s = SETTINGS.iter().find(|s| s.id == id || s.auto.is_some_and(|(a, _)| a == id)).context("the camera has no such setting")?;
    in_mta(|| {
        let (now, auto) = settings.get(s).context("the camera's setting can't be read")?;
        if s.id != id {
            return settings.set(s, now, if value != 0 { AUTO } else { MANUAL });
        }
        if s.auto.is_some() && auto {
            bail!("it's automatic now");
        }
        settings.set(s, i32::try_from(value).context("out of range")?, MANUAL)
    })
}

/// The camera's own settings window, through its DirectShow filter's property
/// pages (what OBS's "Configure Video" opens). Runs on its own thread, which
/// the window's message loop needs to itself.
pub fn open_camera_settings(device: String, name: String) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Media::DirectShow::{IBaseFilter, ICreateDevEnum};
    use windows::Win32::System::Com::StructuredStorage::IPropertyBag;
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, IMoniker};
    use windows::Win32::System::Ole::{ISpecifyPropertyPages, OleCreatePropertyFrame};
    use windows::Win32::System::Variant::VARIANT;
    use windows::core::{HSTRING, IUnknown, w};

    thread::spawn(move || {
        // Property pages are windows: this thread is a single-threaded apartment.
        let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        let result = (|| -> Result<()> {
            unsafe {
                let devices: ICreateDevEnum = CoCreateInstance(&CLSID_SystemDeviceEnum, None, CLSCTX_INPROC_SERVER)?;
                let mut monikers = None;
                devices.CreateClassEnumerator(&CLSID_VideoInputDeviceCategory, &mut monikers, 0)?;
                let monikers = monikers.context("no cameras")?;
                let read = |bag: &IPropertyBag, key: windows::core::PCWSTR| -> Option<String> {
                    let mut v = VARIANT::default();
                    bag.Read(key, &mut v, None).ok()?;
                    Some((*v.Anonymous.Anonymous).Anonymous.bstrVal.to_string())
                };
                // The camera whose device path is our id (or, failing that, its name).
                let mut by_name = None;
                let mut found = None;
                loop {
                    let mut one = [None];
                    if monikers.Next(&mut one, None).is_err() {
                        break;
                    }
                    let Some(moniker): Option<IMoniker> = one[0].take() else { break };
                    let Ok(bag) = moniker.BindToStorage::<_, _, IPropertyBag>(None, None) else { continue };
                    if read(&bag, w!("DevicePath")).is_some_and(|p| p.eq_ignore_ascii_case(&device)) {
                        found = Some(moniker);
                        break;
                    }
                    if by_name.is_none() && read(&bag, w!("FriendlyName")).as_deref() == Some(name.as_str()) {
                        by_name = Some(moniker);
                    }
                }
                let moniker = found.or(by_name).context("the camera isn't connected")?;
                let filter: IBaseFilter = moniker.BindToObject(None, None)?;
                let pages: ISpecifyPropertyPages = filter.cast().context("this camera has no settings window")?;
                let list = pages.GetPages()?;
                let unknown: Option<IUnknown> = Some(filter.cast()?);
                let shown = OleCreatePropertyFrame(
                    HWND::default(),
                    0,
                    0,
                    &HSTRING::from(name.as_str()),
                    1,
                    &unknown,
                    list.cElems,
                    list.pElems,
                    0,
                    None,
                    None,
                );
                CoTaskMemFree(Some(list.pElems as *const _));
                shown?;
            }
            Ok(())
        })();
        if let Err(e) = result {
            eprintln!("camera settings: {e:#}");
        }
    });
}

#[cfg(test)]
mod exposure_tests {
    /// DirectShow's ranges are powers of two of a second; a driver's own
    /// steps aren't, and aren't shown as times.
    #[test]
    fn an_exposure_in_steps_of_its_own_isnt_a_time() {
        assert!(super::exposure_in_log2(-13, -1));
        assert!(super::exposure_in_log2(-11, 1));
        assert!(!super::exposure_in_log2(0, 255), "the PS3 Eye's");
        assert!(!super::exposure_in_log2(1, 100));
        assert_eq!(super::exposure_label(-5), "31.2 ms");
        assert_eq!(super::exposure_label(1), "2 s");
    }
}
