//! Webcams on Linux: Video4Linux devices, read through ffmpeg, which decodes
//! whatever the camera sends (MJPEG at the bigger sizes, raw YUYV at the
//! small ones) to BGRA.
//!
//! Cameras are listed and their formats found with the V4L2 ioctls directly.
//! A camera's id is its `/dev/v4l/by-id` path where it has one, which stays
//! the same across replugging (`/dev/videoN` numbers don't).
//!
//! A webcam stays open for as long as it's set up ([`keep_open`]), previewed
//! and recorded or not, as on the other platforms.

use std::io::Read;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

use super::image::CameraPicture;
use crate::Device;
use crate::webcam::{Format, Status, set_formats, set_status};

/// The camera that's open, delivering pictures.
pub(crate) struct CameraFeed {
    pub device: String,
    pub format: Option<Format>,
    frame: Mutex<Option<Arc<CameraPicture>>>,
    stop: AtomicBool,
}

static CURRENT: Mutex<Option<Arc<CameraFeed>>> = Mutex::new(None);

impl CameraFeed {
    fn latest(&self) -> Option<Arc<CameraPicture>> {
        self.frame.lock().unwrap().clone()
    }

    /// The open camera, if it's this one in this format.
    fn current(device: &str, format: Option<Format>) -> Option<Arc<CameraFeed>> {
        CURRENT.lock().unwrap().clone().filter(|f| f.device == device && f.format == format && !f.stop.load(Ordering::Relaxed))
    }
}

/// Cameras that capture video, by stable path.
pub fn list_cameras() -> Vec<Device> {
    let mut cams: Vec<(String, Device)> = Vec::new();
    let Ok(entries) = std::fs::read_dir("/dev") else { return Vec::new() };
    let by_id = stable_names();
    for e in entries.flatten() {
        let node = e.file_name().to_string_lossy().into_owned();
        if !node.starts_with("video") {
            continue;
        }
        let path = Path::new("/dev").join(&node);
        let Some(caps) = query_caps(&path) else { continue };
        if !caps.captures {
            continue;
        }
        let id = by_id.iter().find(|(_, real)| *real == path).map_or_else(|| path.display().to_string(), |(link, _)| link.display().to_string());
        cams.push((node, Device { id, name: caps.card }));
    }
    // /dev/video0 before /dev/video10.
    cams.sort_by_key(|(n, _)| n.trim_start_matches("video").parse::<u32>().unwrap_or(u32::MAX));
    cams.into_iter().map(|(_, d)| d).collect()
}

/// `/dev/v4l/by-id` links and the devices they point at.
fn stable_names() -> Vec<(PathBuf, PathBuf)> {
    let Ok(entries) = std::fs::read_dir("/dev/v4l/by-id") else { return Vec::new() };
    entries.flatten().filter_map(|e| Some((e.path(), std::fs::canonicalize(e.path()).ok()?))).collect()
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
    let feed = Arc::new(CameraFeed { device, format, frame: Mutex::new(None), stop: AtomicBool::new(false) });
    *current = Some(feed.clone());
    thread::spawn(move || run(feed));
}

/// Open the camera and keep it open until told to stop, reopening it when it
/// goes away (unplugged, taken by another app) and comes back.
fn run(feed: Arc<CameraFeed>) {
    while !feed.stop.load(Ordering::Relaxed) {
        set_status(Status::Opening);
        if let Err(e) = stream(&feed) {
            if feed.stop.load(Ordering::Relaxed) {
                break;
            }
            set_status(Status::Unavailable(format!("{e:#}")));
            let until = std::time::Instant::now() + Duration::from_secs(3);
            while std::time::Instant::now() < until && !feed.stop.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// Read pictures until told to stop or the camera fails.
fn stream(feed: &CameraFeed) -> Result<()> {
    let path = std::fs::canonicalize(&feed.device).map_err(|_| anyhow!("not connected"))?;
    let offered = formats_of(&path)?;
    set_formats(&feed.device, offered.iter().map(|(f, _)| *f).collect());
    let (format, pixel) = match feed.format {
        Some(want) => offered.iter().find(|(f, _)| *f == want).copied().ok_or_else(|| anyhow!("the camera doesn't offer {}", want.label()))?,
        None => offered.iter().max_by_key(|(f, _)| f.auto_rank()).copied().ok_or_else(|| anyhow!("the camera offers no picture format"))?,
    };
    let (w, h) = (format.width, format.height);
    let mut child = super::ffmpeg::ffmpeg()
        .args(["-f", "v4l2", "-input_format", pixel, "-video_size", &format!("{w}x{h}")])
        .args(["-framerate", &format!("{}/{}", format.fps_num, format.fps_den.max(1))])
        .arg("-i")
        .arg(&path)
        .args(["-f", "rawvideo", "-pix_fmt", "bgra", "pipe:1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("couldn't run ffmpeg for the webcam")?;
    let mut out = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let errors = thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });
    let size = w as usize * h as usize * 4;
    let mut live = false;
    // The picture before last's memory, once nobody holds it, for the next one.
    let mut spare: Option<Vec<u8>> = None;
    let result = loop {
        if feed.stop.load(Ordering::Relaxed) {
            break Ok(());
        }
        let mut buf = spare.take().unwrap_or_default();
        buf.resize(size, 0);
        if out.read_exact(&mut buf).is_err() {
            break Err(anyhow!("the camera stopped"));
        }
        let old = feed.frame.lock().unwrap().replace(Arc::new(CameraPicture { width: w, height: h, bgra: buf }));
        spare = old.and_then(|a| Arc::try_unwrap(a).ok()).map(|p| p.bgra);
        if !live {
            set_status(Status::Live { width: w, height: h });
            live = true;
        }
    };
    let _ = child.kill();
    let _ = child.wait();
    let said = errors.join().unwrap_or_default();
    match result {
        Err(e) if !said.trim().is_empty() => {
            let busy = said.contains("Device or resource busy");
            if busy { bail!("in use by another app") } else { Err(e.context(said.lines().last().unwrap_or_default().to_owned())) }
        }
        r => r,
    }
}

/// What a running capture draws: the newest picture of whichever camera
/// the app keeps open. A new camera or format, the webcam switched off and
/// on, a dropout: it follows along without the capture restarting.
pub(crate) struct CameraLayer {
    feed: Option<Arc<CameraFeed>>,
}

impl CameraLayer {
    /// Opens webcam `w` if nothing has yet (the app keeps it open itself)
    /// and it's switched on.
    pub(crate) fn new(w: &crate::webcam::Webcam) -> Self {
        let hidden = w.placement.lock().unwrap().is_hidden();
        if !hidden && CameraFeed::current(&w.device, w.format).is_none() {
            keep_open(Some((w.device.clone(), w.format)));
        }
        Self { feed: None }
    }

    /// The newest picture, if a camera is delivering.
    pub(crate) fn latest(&mut self) -> Option<Arc<CameraPicture>> {
        if self.feed.as_ref().is_none_or(|f| f.stop.load(Ordering::Relaxed)) {
            self.feed = CURRENT.lock().unwrap().clone().filter(|f| !f.stop.load(Ordering::Relaxed));
        }
        self.feed.as_ref()?.latest()
    }
}

// ---------------------------------------------------------------------------
// V4L2
// ---------------------------------------------------------------------------

/// `struct v4l2_capability`.
#[repr(C)]
struct Capability {
    driver: [u8; 16],
    card: [u8; 32],
    bus_info: [u8; 32],
    version: u32,
    capabilities: u32,
    device_caps: u32,
    reserved: [u32; 3],
}

/// `struct v4l2_fmtdesc`.
#[repr(C)]
struct FmtDesc {
    index: u32,
    kind: u32,
    flags: u32,
    description: [u8; 32],
    pixelformat: u32,
    mbus_code: u32,
    reserved: [u32; 3],
}

/// `struct v4l2_frmsizeenum`, for discrete sizes.
#[repr(C)]
struct FrameSize {
    index: u32,
    pixel_format: u32,
    kind: u32,
    /// Discrete: width, height. Stepwise: min/max/step width, min/max/step height.
    sizes: [u32; 6],
    reserved: [u32; 2],
}

/// `struct v4l2_frmivalenum`, for discrete intervals.
#[repr(C)]
struct FrameInterval {
    index: u32,
    pixel_format: u32,
    width: u32,
    height: u32,
    kind: u32,
    /// Discrete: numerator, denominator (seconds a frame).
    fractions: [u32; 6],
    reserved: [u32; 2],
}

const fn iorw(nr: u64, size: usize, read_only: bool) -> u64 {
    let dir: u64 = if read_only { 2 } else { 3 };
    (dir << 30) | ((size as u64) << 16) | ((b'V' as u64) << 8) | nr
}
const VIDIOC_QUERYCAP: u64 = iorw(0, std::mem::size_of::<Capability>(), true);
const VIDIOC_ENUM_FMT: u64 = iorw(2, std::mem::size_of::<FmtDesc>(), false);
const VIDIOC_ENUM_FRAMESIZES: u64 = iorw(74, std::mem::size_of::<FrameSize>(), false);
const VIDIOC_ENUM_FRAMEINTERVALS: u64 = iorw(75, std::mem::size_of::<FrameInterval>(), false);
const V4L2_CAP_VIDEO_CAPTURE: u32 = 0x1;
const V4L2_CAP_DEVICE_CAPS: u32 = 0x8000_0000;
const V4L2_BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
const V4L2_FRMSIZE_TYPE_DISCRETE: u32 = 1;
const V4L2_FRMIVAL_TYPE_DISCRETE: u32 = 1;

fn ioctl<T>(fd: &std::fs::File, request: u64, arg: &mut T) -> bool {
    // SAFETY: `arg` is the request's own struct, laid out as the kernel's.
    unsafe { libc::ioctl(fd.as_raw_fd(), request as _, arg as *mut T) == 0 }
}

fn open(path: &Path) -> Option<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new().read(true).write(true).custom_flags(libc::O_NONBLOCK).open(path).ok()
}

struct Caps {
    card: String,
    captures: bool,
}

fn query_caps(path: &Path) -> Option<Caps> {
    let fd = open(path)?;
    // SAFETY: plain integers and byte arrays; all-zero is valid.
    let mut cap: Capability = unsafe { std::mem::zeroed() };
    if !ioctl(&fd, VIDIOC_QUERYCAP, &mut cap) {
        return None;
    }
    let caps = if cap.capabilities & V4L2_CAP_DEVICE_CAPS != 0 { cap.device_caps } else { cap.capabilities };
    let card = String::from_utf8_lossy(&cap.card).trim_end_matches('\0').trim().to_owned();
    Some(Caps { card, captures: caps & V4L2_CAP_VIDEO_CAPTURE != 0 })
}

/// The sizes and rates a camera offers, biggest first, each with the
/// ffmpeg input format to ask for: MJPEG where the camera offers that size
/// and rate compressed (raw is limited by USB bandwidth), raw otherwise.
fn formats_of(path: &Path) -> Result<Vec<(Format, &'static str)>> {
    let fd = open(path).ok_or_else(|| anyhow!("not connected"))?;
    let mut found: Vec<(Format, &'static str)> = Vec::new();
    for index in 0.. {
        // SAFETY: as above.
        let mut desc: FmtDesc = unsafe { std::mem::zeroed() };
        desc.index = index;
        desc.kind = V4L2_BUF_TYPE_VIDEO_CAPTURE;
        if !ioctl(&fd, VIDIOC_ENUM_FMT, &mut desc) {
            break;
        }
        let pixel = match &desc.pixelformat.to_le_bytes() {
            b"MJPG" | b"JPEG" => "mjpeg",
            b"YUYV" => "yuyv422",
            b"NV12" => "nv12",
            b"YU12" => "yuv420p",
            b"UYVY" => "uyvy422",
            _ => continue, // H.264 cameras and the like: not decoded here
        };
        for (w, h) in sizes(&fd, desc.pixelformat) {
            for (num, den) in intervals(&fd, desc.pixelformat, w, h) {
                // An interval of num/den seconds is den/num frames a second.
                let format = Format { width: w, height: h, fps_num: den, fps_den: num.max(1) };
                match found.iter_mut().find(|(f, _)| *f == format) {
                    Some(existing) if pixel == "mjpeg" => existing.1 = pixel,
                    Some(_) => {}
                    None => found.push((format, pixel)),
                }
            }
        }
    }
    if found.is_empty() {
        bail!("the camera offers no picture format we can read");
    }
    found.sort_by_key(|(f, _)| std::cmp::Reverse((f.width * f.height, (f.fps() * 100.0) as u32)));
    Ok(found)
}

fn sizes(fd: &std::fs::File, pixel: u32) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    for index in 0..64 {
        // SAFETY: as above.
        let mut s: FrameSize = unsafe { std::mem::zeroed() };
        s.index = index;
        s.pixel_format = pixel;
        if !ioctl(fd, VIDIOC_ENUM_FRAMESIZES, &mut s) {
            break;
        }
        if s.kind == V4L2_FRMSIZE_TYPE_DISCRETE {
            out.push((s.sizes[0], s.sizes[1]));
        } else {
            // A range: the largest, and the common sizes inside it.
            let (max_w, max_h) = (s.sizes[1], s.sizes[4]);
            out.push((max_w, max_h));
            for (w, h) in [(1920, 1080), (1280, 720), (640, 480)] {
                if w < max_w && h < max_h && w >= s.sizes[0] && h >= s.sizes[3] {
                    out.push((w, h));
                }
            }
            break;
        }
    }
    out
}

fn intervals(fd: &std::fs::File, pixel: u32, width: u32, height: u32) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    for index in 0..32 {
        // SAFETY: as above.
        let mut i: FrameInterval = unsafe { std::mem::zeroed() };
        i.index = index;
        i.pixel_format = pixel;
        i.width = width;
        i.height = height;
        if !ioctl(fd, VIDIOC_ENUM_FRAMEINTERVALS, &mut i) {
            break;
        }
        if i.kind == V4L2_FRMIVAL_TYPE_DISCRETE {
            out.push((i.fractions[0], i.fractions[1]));
        } else {
            // A range: its fastest (the shortest interval).
            out.push((i.fractions[0], i.fractions[1]));
            break;
        }
    }
    if out.is_empty() {
        out.push((1, 30));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_numbers_match_the_kernel() {
        assert_eq!(VIDIOC_QUERYCAP, 0x8068_5600);
        assert_eq!(VIDIOC_ENUM_FMT, 0xC040_5602);
        assert_eq!(VIDIOC_ENUM_FRAMESIZES, 0xC02C_564A);
        assert_eq!(VIDIOC_ENUM_FRAMEINTERVALS, 0xC034_564B);
    }
}
