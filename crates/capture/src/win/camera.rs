//! Webcams, with Media Foundation's source reader: it decodes whatever the
//! camera sends (MJPEG, YUY2, NV12) into BGRA, which is uploaded to the GPU
//! for the compositor (`d3d::Converter`).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

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

/// A running camera, filling its own `Latest` texture.
pub(crate) struct Camera {
    pub latest: Arc<Latest>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Camera {
    /// Open camera `device` (a symbolic link) and start delivering frames.
    /// Returns once its format is settled (the texture's size depends on it).
    pub(crate) fn open(gpu: &Gpu, device: &str, format: Option<crate::webcam::Format>) -> Result<Self> {
        set_status(Status::Opening);
        let opened = (|| {
            let (reader, width, height) = open_reader(device, format)?;
            let latest = Latest::new(gpu, width, height)?;
            Ok::<_, anyhow::Error>((reader, latest, width, height))
        })();
        let (reader, latest, width, height) = match opened {
            Ok(o) => o,
            Err(e) => {
                set_status(Status::Unavailable(format!("{e:#}")));
                return Err(e);
            }
        };
        let stop = Arc::new(AtomicBool::new(false));
        let (stop2, latest2) = (stop.clone(), latest.clone());
        let reader = super::d3d::Shared(reader);
        let thread = thread::spawn(move || {
            com_init();
            let reader = reader;
            set_status(Status::Live { width, height });
            let mut row_buf = Vec::new();
            while !stop2.load(Ordering::Relaxed) {
                let mut flags = 0u32;
                let mut sample = None;
                // Blocks until the camera's next frame.
                if let Err(e) = unsafe { reader.0.ReadSample(STREAM, 0, None, Some(&mut flags), None, Some(&mut sample)) } {
                    set_status(Status::Unavailable(e.message()));
                    break;
                }
                if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                    set_status(Status::Unavailable("the camera stopped".into()));
                    break;
                }
                if let Some(sample) = sample {
                    if let Err(e) = upload(&sample, &latest2, width, height, &mut row_buf) {
                        eprintln!("webcam frame: {e:#}");
                    }
                }
            }
        });
        Ok(Self { latest, stop, thread: Some(thread) })
    }
}

impl Drop for Camera {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        set_status(Status::Off);
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
        let auto_key = |f: &crate::webcam::Format| {
            let fits = f.width * f.height <= 1920 * 1080;
            (fits && f.fps() >= 29.5, fits, f.width * f.height, (f.fps() * 100.0) as u32)
        };
        let chosen = format
            .and_then(|want| offered.iter().find(|(f, _)| *f == want).or_else(|| offered.iter().find(|(f, _)| f.label() == want.label())))
            .or_else(|| offered.iter().max_by_key(|(f, _)| auto_key(f)));
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

/// Copy one frame to the camera's texture.
fn upload(sample: &IMFSample, latest: &Latest, width: u32, height: u32, rows: &mut Vec<u8>) -> Result<()> {
    unsafe {
        let buffer = sample.GetBufferByIndex(0)?;
        let row = width as usize * 4;
        if let Ok(b2) = buffer.cast::<IMF2DBuffer>() {
            // A 2D buffer knows its pitch, which is negative for bottom-up RGB.
            let mut scan0 = std::ptr::null_mut();
            let mut pitch = 0i32;
            b2.Lock2D(&mut scan0, &mut pitch)?;
            if pitch > 0 {
                latest.upload_bgra(scan0, pitch as u32, width, height);
            } else {
                rows.resize(row * height as usize, 0);
                for y in 0..height as usize {
                    let src = scan0.offset(y as isize * pitch as isize);
                    std::ptr::copy_nonoverlapping(src, rows[y * row..].as_mut_ptr(), row);
                }
                latest.upload_bgra(rows.as_ptr(), row as u32, width, height);
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
            rows.resize(row * height as usize, 0);
            for y in 0..height as usize {
                let src = ptr.add((height as usize - 1 - y) * row);
                std::ptr::copy_nonoverlapping(src, rows[y * row..].as_mut_ptr(), row);
            }
            buffer.Unlock()?;
            latest.upload_bgra(rows.as_ptr(), row as u32, width, height);
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
