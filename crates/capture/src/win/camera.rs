//! Webcams, with Media Foundation's source reader: it decodes whatever the
//! camera sends (MJPEG, YUY2, NV12) into BGRA, which is uploaded to the GPU
//! for the compositor (`d3d::Converter`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::core::Interface;

use super::d3d::{Gpu, Latest};
use super::h264::take_activates;
use super::system::com_init;
use crate::Device;
use crate::webcam::{Status, set_status};

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

/// The camera, kept open for as long as a webcam is set up — not just while
/// something previews or records it. Closing a camera can reset what was set
/// in its own settings window (exposure, focus…), and some, like the C922, do.
/// Captures and previews take its newest frame ([`CameraFeed::latest`]) and
/// upload it to their own GPU.
pub(crate) struct CameraFeed {
    pub device: String,
    pub format: Option<crate::webcam::Format>,
    frame: Mutex<Option<Arc<Frame>>>,
    stop: AtomicBool,
}

/// The one open camera.
static CURRENT: Mutex<Option<Arc<CameraFeed>>> = Mutex::new(None);

impl CameraFeed {
    pub(crate) fn latest(&self) -> Option<Arc<Frame>> {
        self.frame.lock().unwrap().clone()
    }

    /// The open camera, if it's `device` in `format`.
    pub(crate) fn current(device: &str, format: Option<crate::webcam::Format>) -> Option<Arc<CameraFeed>> {
        CURRENT.lock().unwrap().clone().filter(|f| f.device == device && f.format == format)
    }
}

/// Keep camera `want` (device, format) open, or none. Opening and closing
/// happen in the background; asking for what's already open does nothing.
pub fn keep_open(want: Option<(String, Option<crate::webcam::Format>)>) {
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
    let feed = Arc::new(CameraFeed { device, format, frame: Mutex::new(None), stop: AtomicBool::new(false) });
    *current = Some(feed.clone());
    thread::spawn(move || run(feed));
}

/// Open the camera and keep reading it until told to stop. If it can't be
/// opened (unplugged, in use) or stops, try again every few seconds.
fn run(feed: Arc<CameraFeed>) {
    com_init();
    let mut seq = 0u64;
    while !feed.stop.load(Ordering::Relaxed) {
        set_status(Status::Opening);
        let (reader, width, height) = match open_reader(&feed.device, feed.format) {
            Ok(r) => r,
            Err(e) => {
                set_status(Status::Unavailable(format!("{e:#}")));
                wait_or_stop(&feed, Duration::from_secs(3));
                continue;
            }
        };
        set_status(Status::Live { width, height });
        let mut rows = Vec::new();
        while !feed.stop.load(Ordering::Relaxed) {
            let mut flags = 0u32;
            let mut sample = None;
            // Blocks until the camera's next frame.
            if let Err(e) = unsafe { reader.ReadSample(STREAM, 0, None, Some(&mut flags), None, Some(&mut sample)) } {
                set_status(Status::Unavailable(e.message()));
                break;
            }
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                set_status(Status::Unavailable("the camera stopped".into()));
                break;
            }
            let Some(sample) = sample else { continue };
            match read_frame(&sample, width, height, &mut rows) {
                Ok(()) => {
                    seq += 1;
                    let bgra = std::mem::take(&mut rows);
                    *feed.frame.lock().unwrap() = Some(Arc::new(Frame { seq, width, height, bgra }));
                }
                Err(e) => eprintln!("webcam frame: {e:#}"),
            }
        }
        drop(reader);
        if !feed.stop.load(Ordering::Relaxed) {
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

/// The webcam layer of a running capture: the newest picture of whichever
/// camera the app keeps open, turned as placed, in a GPU texture for the
/// compositor. A new camera or format, the webcam switched off and on, a
/// dropout: it follows along without the capture restarting.
pub(crate) struct CameraLayer {
    feed: Option<Arc<CameraFeed>>,
    placement: crate::webcam::SharedPlacement,
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
    /// For webcam `w`, opening it if nothing has (the app keeps it open
    /// itself), unless it's switched off: then it waits for the app to.
    pub(crate) fn new(gpu: &Gpu, w: &crate::webcam::Webcam) -> Result<Self> {
        let hidden = w.placement.lock().unwrap().is_hidden();
        if !hidden && CameraFeed::current(&w.device, w.format).is_none() {
            keep_open(Some((w.device.clone(), w.format)));
        }
        Ok(Self { feed: None, placement: w.placement.clone(), latest: Latest::new(gpu, MAX_SIDE, MAX_SIDE)?, last: (0, 0), turned: Vec::new() })
    }

    /// Upload the camera's newest frame, if it has one we haven't (or it's
    /// turned another way now).
    pub(crate) fn pull(&mut self) {
        if self.feed.as_ref().is_none_or(|f| f.stop.load(Ordering::Relaxed)) {
            self.feed = CURRENT.lock().unwrap().clone().filter(|f| !f.stop.load(Ordering::Relaxed));
            self.last = (0, 0);
        }
        let Some(f) = self.feed.as_ref().and_then(|feed| feed.latest()) else { return };
        let turns = self.placement.lock().unwrap().turns % 4;
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
fn open_reader(device: &str, format: Option<crate::webcam::Format>) -> Result<(IMFSourceReader, u32, u32)> {
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
        Ok((reader, width, height))
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
