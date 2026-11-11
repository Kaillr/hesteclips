//! Webcams on Linux: Video4Linux devices, streamed straight from the camera
//! (V4L2's memory-mapped buffers) and decoded here to BGRA: MJPEG (the
//! bigger sizes) with libjpeg-turbo where the system has it, else
//! zune-jpeg; raw YUYV, UYVY, NV12 or YU12 (the small ones) converted
//! directly. Where a camera can't be streamed so, ffmpeg reads it instead.
//! An ffmpeg process took ~4x the CPU for 1080p MJPEG (decoding, converting
//! and piping each 8 MB picture): 44% of a core at 30 fps against 10%.
//!
//! Cameras are listed and their formats found with the V4L2 ioctls directly.
//! A camera's id is its `/dev/v4l/by-id` path where it has one, which stays
//! the same across replugging (`/dev/videoN` numbers don't).
//!
//! A webcam stays open for as long as it's set up ([`keep_open`]), previewed
//! and recorded or not, as on the other platforms; several can be open at
//! once, each its own status, formats and pictures.

use std::io::Read;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};

use super::image::CameraPicture;
use crate::Device;
use crate::webcam::{Format, Status, set_formats, set_status_of};

/// A camera that's open, delivering pictures.
pub(crate) struct CameraFeed {
    pub device: String,
    pub format: Option<Format>,
    frame: Mutex<Option<Arc<CameraPicture>>>,
    /// The newest picture's number (`PICTURES`), so a reader can tell a new one.
    seq: std::sync::atomic::AtomicU64,
    /// Counts its pictures that differ from the one before. With a long
    /// exposure a camera keeps sending frames at its rate but repeats
    /// pictures in them (the very same bytes): this counts what's new, its
    /// real rate.
    new_pictures: std::sync::atomic::AtomicU64,
    stop: AtomicBool,
}

/// The cameras kept open.
static OPEN: Mutex<Vec<Arc<CameraFeed>>> = Mutex::new(Vec::new());

/// Numbers every camera picture, whichever camera or format it's from: a
/// new format's pictures carry on from the last ones, not from 0 (a reader
/// waiting for one newer than the last it had would wait for as many again).
static PICTURES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl CameraFeed {
    fn latest(&self) -> Option<Arc<CameraPicture>> {
        self.frame.lock().unwrap().clone()
    }

    /// Camera `device`'s feed, if it's open: in `format` if it's open in
    /// that, else in any.
    fn of(device: &str, format: Option<Format>) -> Option<Arc<CameraFeed>> {
        let open = OPEN.lock().unwrap();
        let live = || open.iter().filter(|f| f.device == device && !f.stop.load(Ordering::Relaxed));
        live().find(|f| f.format == format).or_else(|| live().next()).cloned()
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

/// Keep these cameras (device, format) open, and close the rest. Doesn't wait.
pub fn keep_open(want: Vec<(String, Option<Format>)>) {
    let mut open = OPEN.lock().unwrap();
    let wanted = |f: &CameraFeed| want.iter().any(|(d, fm)| f.device == *d && f.format == *fm);
    for f in open.iter().filter(|f| !wanted(f)) {
        f.stop.store(true, Ordering::Relaxed); // its thread closes it
        // Closed, not opened again in another format: off.
        if !want.iter().any(|(d, _)| *d == f.device) {
            set_status_of(&f.device, Status::Off);
        }
    }
    open.retain(|f| wanted(f));
    for (device, format) in want {
        if !open.iter().any(|f| f.device == device && f.format == format) {
            open.push(start(device, format));
        }
    }
}

/// Open camera `device` in `format` if it isn't open (in any format),
/// leaving the others as they are: for a capture whose webcam the app
/// doesn't keep open itself.
pub(crate) fn ensure_open(device: &str, format: Option<Format>) {
    let mut open = OPEN.lock().unwrap();
    if !open.iter().any(|f| f.device == device && !f.stop.load(Ordering::Relaxed)) {
        open.push(start(device.to_owned(), format));
    }
}

/// A feed of camera `device` in `format`, opened on a thread of its own.
fn start(device: String, format: Option<Format>) -> Arc<CameraFeed> {
    let feed = Arc::new(CameraFeed { device, format, frame: Mutex::new(None), seq: Default::default(), new_pictures: Default::default(), stop: AtomicBool::new(false) });
    // The same camera's thread before (another format) is let go of first
    // (its ffmpeg ends after its next picture): opened while that still
    // holds it, a camera is busy.
    let mut running = RUNNING.lock().unwrap();
    running.retain(|(_, t)| !t.is_finished());
    let before = running.iter().position(|(d, _)| *d == feed.device).map(|at| running.remove(at).1);
    let feed2 = feed.clone();
    running.push((feed.device.clone(), thread::spawn(move || {
        if let Some(t) = before {
            let _ = t.join();
        }
        run(feed2)
    })));
    feed
}

/// Each camera's thread opened last, to wait for before opening it again.
static RUNNING: Mutex<Vec<(String, thread::JoinHandle<()>)>> = Mutex::new(Vec::new());

/// Open the camera and keep it open until told to stop, reopening it when it
/// goes away (unplugged, taken by another app) and comes back.
fn run(feed: Arc<CameraFeed>) {
    while !feed.stop.load(Ordering::Relaxed) {
        set_status_of(&feed.device, Status::Opening);
        if let Err(e) = stream(&feed) {
            if feed.stop.load(Ordering::Relaxed) {
                break;
            }
            set_status_of(&feed.device, Status::Unavailable(format!("{e:#}")));
            let until = std::time::Instant::now() + Duration::from_secs(3);
            while std::time::Instant::now() < until && !feed.stop.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// Read pictures until told to stop or the camera fails: straight from the
/// camera, or through ffmpeg where it can't be streamed so.
fn stream(feed: &CameraFeed) -> Result<()> {
    let path = std::fs::canonicalize(&feed.device).map_err(|_| anyhow!("not connected"))?;
    let offered = formats_of(&path)?;
    set_formats(&feed.device, offered.iter().map(|(f, _)| *f).collect());
    let (format, pixel) = match feed.format {
        Some(want) => offered.iter().find(|(f, _)| *f == want).copied().ok_or_else(|| anyhow!("the camera doesn't offer {}", want.label()))?,
        None => offered.iter().max_by_key(|(f, _)| f.auto_rank()).copied().ok_or_else(|| anyhow!("the camera offers no picture format"))?,
    };
    if std::env::var_os("HESTECLIPS_WEBCAM_FFMPEG").is_none() {
        match Stream::open(&path, format, pixel) {
            Ok(s) => match stream_direct(feed, s)? {
                Direct::Done => return Ok(()),
                Direct::Unreadable => eprintln!("webcam: its pictures can't be read here; through ffmpeg"),
            },
            Err(Opening::Busy) => bail!("in use by another app"),
            Err(Opening::Failed(e)) => eprintln!("webcam: can't stream it directly ({e:#}); through ffmpeg"),
        }
    }
    stream_ffmpeg(feed, &path, format, pixel)
}

/// Hand `bgra` (a `w`×`h` picture) to the feed's readers, and its memory
/// before last back for the next.
struct Delivery {
    live: bool,
    spare: Option<Vec<u8>>,
    last_print: Option<u64>,
}

impl Delivery {
    fn new() -> Self {
        Self { live: false, spare: None, last_print: None }
    }

    /// Memory for the next picture of `size` bytes.
    fn buffer(&mut self, size: usize) -> Vec<u8> {
        let mut buf = self.spare.take().unwrap_or_default();
        buf.resize(size, 0);
        buf
    }

    fn deliver(&mut self, feed: &CameraFeed, w: u32, h: u32, buf: Vec<u8>) {
        let print = crate::webcam::fingerprint(&buf);
        if self.last_print != Some(print) {
            feed.new_pictures.fetch_add(1, Ordering::Relaxed);
            self.last_print = Some(print);
        }
        let seq = PICTURES.fetch_add(1, Ordering::Relaxed) + 1;
        let old = feed.frame.lock().unwrap().replace(Arc::new(CameraPicture { width: w, height: h, bgra: buf, seq }));
        feed.seq.store(seq, Ordering::Release);
        self.spare = old.and_then(|a| Arc::try_unwrap(a).ok()).map(|p| p.bgra);
        if !self.live {
            // Open: what was set for this camera before goes back on (it
            // forgets when it's unplugged).
            crate::webcam::apply_remembered(&feed.device);
            if !feed.stop.load(Ordering::Relaxed) {
                set_status_of(&feed.device, Status::Live { width: w, height: h });
            }
            self.live = true;
        }
    }
}

/// How long a camera can go without a picture before it's taken to have
/// stopped (a dark room's long exposures are well under).
const SILENT: Duration = Duration::from_secs(5);

/// How reading straight from the camera ended.
enum Direct {
    /// Told to stop.
    Done,
    /// Not one of its first pictures could be decoded here (a JPEG of a
    /// kind ours don't read): ffmpeg reads it then, as it did before.
    Unreadable,
}

/// Pictures that can't be decoded before giving up on decoding here.
const UNREADABLE: u32 = 30;

/// Read pictures straight from the camera, decoding them here.
fn stream_direct(feed: &CameraFeed, mut s: Stream) -> Result<Direct> {
    let (w, h) = (s.width, s.height);
    let mut decoder = Decoder::new(s.pixel, w, h, s.bytes_per_line);
    let mut delivery = Delivery::new();
    let mut heard = std::time::Instant::now();
    let mut bad = 0u32;
    while !feed.stop.load(Ordering::Relaxed) {
        let mut buf = delivery.buffer(w as usize * h as usize * 4);
        // Short waits, to notice being told to stop.
        let got = s.next(Duration::from_millis(100), |data| decoder.decode(data, &mut buf))?;
        match got {
            None => {
                if heard.elapsed() > SILENT {
                    bail!("the camera stopped");
                }
                delivery.spare = Some(buf);
            }
            Some(Err(e)) => {
                // A broken picture now and then (a USB hiccup): skipped.
                heard = std::time::Instant::now();
                bad += 1;
                if bad.is_power_of_two() {
                    eprintln!("webcam: {bad} pictures it sent couldn't be read: {e:#}");
                }
                if !delivery.live && bad >= UNREADABLE {
                    return Ok(Direct::Unreadable);
                }
                delivery.spare = Some(buf);
            }
            Some(Ok(())) => {
                heard = std::time::Instant::now();
                delivery.deliver(feed, w, h, buf);
            }
        }
    }
    Ok(Direct::Done)
}

/// Read pictures through ffmpeg, which decodes them to BGRA.
fn stream_ffmpeg(feed: &CameraFeed, path: &Path, format: Format, pixel: &str) -> Result<()> {
    let (w, h) = (format.width, format.height);
    let mut child = super::ffmpeg::ffmpeg()
        .args(["-f", "v4l2", "-input_format", pixel, "-video_size", &format!("{w}x{h}")])
        .args(["-framerate", &format!("{}/{}", format.fps_num, format.fps_den.max(1))])
        .arg("-i")
        .arg(path)
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
    let mut delivery = Delivery::new();
    let result = loop {
        if feed.stop.load(Ordering::Relaxed) {
            break Ok(());
        }
        let mut buf = delivery.buffer(size);
        if out.read_exact(&mut buf).is_err() {
            break Err(anyhow!("the camera stopped"));
        }
        delivery.deliver(feed, w, h, buf);
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

/// What a running capture draws for a webcam layer: the newest picture of
/// its camera, as the app keeps it open. Another format, the webcam
/// switched off and on, a dropout: it follows along without the capture
/// restarting.
pub(crate) struct CameraLayer {
    device: String,
    format: Option<Format>,
    feed: Option<Arc<CameraFeed>>,
}

impl CameraLayer {
    /// Opens the camera if nothing has yet (the app keeps it open itself)
    /// and it shows.
    pub(crate) fn new(device: &str, format: Option<Format>, hidden: bool) -> Self {
        if !hidden {
            ensure_open(device, format);
        }
        Self { device: device.to_owned(), format, feed: None }
    }

    /// The newest picture, if the camera is delivering.
    pub(crate) fn latest(&mut self) -> Option<Arc<CameraPicture>> {
        if self.feed.as_ref().is_none_or(|f| f.stop.load(Ordering::Relaxed)) {
            self.feed = CameraFeed::of(&self.device, self.format);
        }
        self.feed.as_ref()?.latest()
    }
}

// ---------------------------------------------------------------------------
// Streaming
// ---------------------------------------------------------------------------

/// Why a camera couldn't be streamed.
enum Opening {
    /// Another app has it.
    Busy,
    Failed(anyhow::Error),
}

/// A camera streaming into buffers it shares with us (V4L2 memory mapping):
/// each picture is read where the camera put it, then handed back.
struct Stream {
    fd: std::fs::File,
    buffers: Vec<(*mut libc::c_void, usize)>,
    width: u32,
    height: u32,
    pixel: u32,
    bytes_per_line: u32,
}

impl Stream {
    /// Start `format` streaming, in `pixel` (ffmpeg's name for it).
    fn open(path: &Path, format: Format, pixel: &str) -> Result<Self, Opening> {
        let failed = |what: &str| Opening::Failed(anyhow!("{what}: {}", std::io::Error::last_os_error()));
        let fourcc = match pixel {
            "mjpeg" => u32::from_le_bytes(*b"MJPG"),
            "yuyv422" => u32::from_le_bytes(*b"YUYV"),
            "uyvy422" => u32::from_le_bytes(*b"UYVY"),
            "nv12" => u32::from_le_bytes(*b"NV12"),
            "yuv420p" => u32::from_le_bytes(*b"YU12"),
            other => return Err(Opening::Failed(anyhow!("{other} isn't read here"))),
        };
        let fd = open(path).ok_or_else(|| failed("opening it"))?;
        // SAFETY: plain integers and byte arrays; all-zero is valid.
        let mut f: V4l2Format = unsafe { std::mem::zeroed() };
        f.kind = V4L2_BUF_TYPE_VIDEO_CAPTURE;
        f.pix.width = format.width;
        f.pix.height = format.height;
        f.pix.pixelformat = fourcc;
        if !ioctl(&fd, VIDIOC_S_FMT, &mut f) {
            return Err(if std::io::Error::last_os_error().raw_os_error() == Some(libc::EBUSY) { Opening::Busy } else { failed("its picture format") });
        }
        // A camera that says it does the format, then picks another (MJPG
        // asked, JPEG given) is read as what it gives, if that's read here.
        let pixel = f.pix.pixelformat;
        if !matches!(&pixel.to_le_bytes(), b"MJPG" | b"JPEG" | b"YUYV" | b"UYVY" | b"NV12" | b"YU12") {
            return Err(Opening::Failed(anyhow!("it gives {} instead", String::from_utf8_lossy(&pixel.to_le_bytes()))));
        }
        let mut s = Self { fd, buffers: Vec::new(), width: f.pix.width, height: f.pix.height, pixel, bytes_per_line: f.pix.bytesperline };
        // The rate: seconds a frame, the inverse of frames a second.
        // SAFETY: as above.
        let mut parm: StreamParm = unsafe { std::mem::zeroed() };
        parm.kind = V4L2_BUF_TYPE_VIDEO_CAPTURE;
        parm.numerator = format.fps_den.max(1);
        parm.denominator = format.fps_num;
        if !ioctl(&s.fd, VIDIOC_S_PARM, &mut parm) {
            eprintln!("webcam: it won't take {} fps: {}", format.fps(), std::io::Error::last_os_error());
        }
        let mut req = RequestBuffers { count: 4, kind: V4L2_BUF_TYPE_VIDEO_CAPTURE, memory: V4L2_MEMORY_MMAP, capabilities: 0, flags: 0, reserved: [0; 3] };
        if !ioctl(&s.fd, VIDIOC_REQBUFS, &mut req) || req.count == 0 {
            return Err(failed("buffers"));
        }
        for index in 0..req.count {
            // SAFETY: as above.
            let mut b: V4l2Buffer = unsafe { std::mem::zeroed() };
            b.index = index;
            b.kind = V4L2_BUF_TYPE_VIDEO_CAPTURE;
            b.memory = V4L2_MEMORY_MMAP;
            if !ioctl(&s.fd, VIDIOC_QUERYBUF, &mut b) {
                return Err(failed("a buffer"));
            }
            // SAFETY: the buffer the camera just described, read only.
            let map = unsafe { libc::mmap(std::ptr::null_mut(), b.length as usize, libc::PROT_READ, libc::MAP_SHARED, s.fd.as_raw_fd(), b.offset as u32 as libc::off_t) };
            if map == libc::MAP_FAILED {
                return Err(failed("mapping a buffer"));
            }
            s.buffers.push((map, b.length as usize));
            if !ioctl(&s.fd, VIDIOC_QBUF, &mut b) {
                return Err(failed("queueing a buffer"));
            }
        }
        let mut kind = V4L2_BUF_TYPE_VIDEO_CAPTURE as libc::c_int;
        if !ioctl(&s.fd, VIDIOC_STREAMON, &mut kind) {
            return Err(if std::io::Error::last_os_error().raw_os_error() == Some(libc::EBUSY) { Opening::Busy } else { failed("starting it") });
        }
        Ok(s)
    }

    /// The next picture's bytes (waiting at most `wait`) handed to `f`, and
    /// then back to the camera; `None` if none came, or it came broken.
    fn next<R>(&mut self, wait: Duration, f: impl FnOnce(&[u8]) -> R) -> Result<Option<R>> {
        let mut poll = libc::pollfd { fd: self.fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd.
        let ready = unsafe { libc::poll(&mut poll, 1, wait.as_millis() as libc::c_int) };
        if ready < 0 {
            let e = std::io::Error::last_os_error();
            return if e.kind() == std::io::ErrorKind::Interrupted { Ok(None) } else { Err(anyhow!("waiting for the camera: {e}")) };
        }
        if ready == 0 {
            return Ok(None);
        }
        if poll.revents & (libc::POLLERR | libc::POLLHUP) != 0 {
            bail!("the camera went away");
        }
        // SAFETY: as above.
        let mut b: V4l2Buffer = unsafe { std::mem::zeroed() };
        b.kind = V4L2_BUF_TYPE_VIDEO_CAPTURE;
        b.memory = V4L2_MEMORY_MMAP;
        if !ioctl(&self.fd, VIDIOC_DQBUF, &mut b) {
            let e = std::io::Error::last_os_error();
            return if e.raw_os_error() == Some(libc::EAGAIN) { Ok(None) } else { Err(anyhow!("the camera stopped: {e}")) };
        }
        let (map, len) = self.buffers[b.index as usize];
        let got = (b.flags & V4L2_BUF_FLAG_ERROR == 0).then(|| {
            // SAFETY: the camera's buffer, ours until it's queued again.
            f(unsafe { std::slice::from_raw_parts(map as *const u8, (b.bytesused as usize).min(len)) })
        });
        if !ioctl(&self.fd, VIDIOC_QBUF, &mut b) {
            bail!("the camera won't take a buffer back: {}", std::io::Error::last_os_error());
        }
        Ok(got)
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        let mut kind = V4L2_BUF_TYPE_VIDEO_CAPTURE as libc::c_int;
        ioctl(&self.fd, VIDIOC_STREAMOFF, &mut kind);
        for (map, len) in &self.buffers {
            // SAFETY: our mappings, not read after this.
            unsafe { libc::munmap(*map, *len) };
        }
    }
}

// SAFETY: the mappings are only read by the thread that has the stream.
unsafe impl Send for Stream {}

/// Turns a camera's pictures into B, G, R, A.
enum Decoder {
    Jpeg(Option<Turbo>),
    /// Two pixels in four bytes: Y0 U Y1 V (YUYV) or U Y0 V Y1 (UYVY).
    Packed { y: usize, u: usize, v: usize, w: u32, h: u32, stride: usize },
    /// A Y plane, then U, V pairs at half the size each way (NV12), or a U
    /// plane and a V plane (YU12).
    Planar { interleaved: bool, w: u32, h: u32, stride: usize },
}

impl Decoder {
    fn new(pixel: u32, w: u32, h: u32, bytes_per_line: u32) -> Self {
        let stride = |bpp: u32| (bytes_per_line as usize).max((w * bpp) as usize);
        match &pixel.to_le_bytes() {
            b"YUYV" => Self::Packed { y: 0, u: 1, v: 3, w, h, stride: stride(2) },
            b"UYVY" => Self::Packed { y: 1, u: 0, v: 2, w, h, stride: stride(2) },
            b"NV12" => Self::Planar { interleaved: true, w, h, stride: stride(1) },
            b"YU12" => Self::Planar { interleaved: false, w, h, stride: stride(1) },
            _ => Self::Jpeg(Turbo::load()),
        }
    }

    /// Decode `data` into `out` (`w`×`h`×4 bytes).
    fn decode(&mut self, data: &[u8], out: &mut [u8]) -> Result<()> {
        match self {
            Self::Jpeg(Some(turbo)) => turbo.decode(data, out),
            Self::Jpeg(None) => {
                use zune_core::colorspace::ColorSpace;
                use zune_core::options::DecoderOptions;
                // zune-jpeg fills in the standard Huffman tables webcams
                // leave out only in pictures marked as MJPEG (APP0 "AVI1").
                let marked;
                let data = if data.len() < 2 || data.windows(5).take(64).any(|w| w == b"AVI1\0") {
                    data
                } else {
                    marked = [&data[..2], &[0xFF, 0xE0, 0, 7, b'A', b'V', b'I', b'1', 0][..], &data[2..]].concat();
                    &marked
                };
                let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::BGRA);
                let mut d = zune_jpeg::JpegDecoder::new_with_options(zune_core::bytestream::ZCursor::new(data), options);
                d.decode_into(out).map_err(|e| anyhow!("{e:?}"))
            }
            &mut Self::Packed { y, u, v, w, h, stride } => {
                let (w, h) = (w as usize, h as usize);
                ensure!(data.len() >= stride * (h - 1) + w * 2, "short picture");
                for (row, dst) in out.chunks_exact_mut(w * 4).take(h).enumerate() {
                    let src = &data[row * stride..row * stride + w * 2];
                    for (pair, px) in src.chunks_exact(4).zip(dst.chunks_exact_mut(8)) {
                        let (cb, cr) = (pair[u], pair[v]);
                        px[..4].copy_from_slice(&bt601(pair[y], cb, cr));
                        px[4..].copy_from_slice(&bt601(pair[y + 2], cb, cr));
                    }
                }
                Ok(())
            }
            &mut Self::Planar { interleaved, w, h, stride } => {
                let (w, h) = (w as usize, h as usize);
                let luma = stride * h;
                let chroma = if interleaved { stride * h / 2 } else { 2 * (stride / 2) * (h / 2) };
                ensure!(data.len() >= luma + chroma, "short picture");
                for (row, dst) in out.chunks_exact_mut(w * 4).take(h).enumerate() {
                    for (x, px) in dst.chunks_exact_mut(4).enumerate() {
                        let luma_at = data[row * stride + x];
                        let (cb, cr) = if interleaved {
                            let at = luma + (row / 2) * stride + (x / 2) * 2;
                            (data[at], data[at + 1])
                        } else {
                            let at = (row / 2) * (stride / 2) + x / 2;
                            (data[luma + at], data[luma + (stride / 2) * (h / 2) + at])
                        };
                        px.copy_from_slice(&bt601(luma_at, cb, cr));
                    }
                }
                Ok(())
            }
        }
    }
}

/// One pixel of BT.601, studio range (what raw camera pictures are), as B,
/// G, R, A: as ffmpeg converted them.
fn bt601(y: u8, cb: u8, cr: u8) -> [u8; 4] {
    let (c, d, e) = (298 * (y as i32 - 16), cb as i32 - 128, cr as i32 - 128);
    let clip = |v: i32| (v >> 8).clamp(0, 255) as u8;
    [clip(c + 516 * d + 128), clip(c - 100 * d - 208 * e + 128), clip(c + 409 * e + 128), 255]
}

/// libjpeg-turbo's decoder (TurboJPEG, opened at run time): about twice as
/// fast as zune-jpeg on a webcam's pictures (3.5 against 6.5 ms for 1080p).
struct Turbo {
    handle: *mut libc::c_void,
    decompress: Decompress,
    destroy: Destroy,
    error: ErrorText,
}

/// `tjDecompress2`, `tjDestroy`, `tjGetErrorStr2` and `tjInitDecompress`.
type Decompress = unsafe extern "C" fn(*mut libc::c_void, *const u8, libc::c_ulong, *mut u8, libc::c_int, libc::c_int, libc::c_int, libc::c_int, libc::c_int) -> libc::c_int;
type Destroy = unsafe extern "C" fn(*mut libc::c_void) -> libc::c_int;
type ErrorText = unsafe extern "C" fn(*mut libc::c_void) -> *const libc::c_char;
type Init = unsafe extern "C" fn() -> *mut libc::c_void;

/// TurboJPEG's B, G, R, A, and its faster (still exact enough) inverse DCT.
const TJPF_BGRA: libc::c_int = 8;
const TJFLAG_FASTDCT: libc::c_int = 2048;

impl Turbo {
    fn load() -> Option<Self> {
        // SAFETY: a library name and symbol names; the symbols are
        // TurboJPEG's 2.x API (also in 3.x), cast to their C signatures.
        unsafe {
            let lib = libc::dlopen(c"libturbojpeg.so.0".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
            if lib.is_null() {
                return None;
            }
            let sym = |name: &std::ffi::CStr| libc::dlsym(lib, name.as_ptr());
            let (init, decompress, destroy, error) = (sym(c"tjInitDecompress"), sym(c"tjDecompress2"), sym(c"tjDestroy"), sym(c"tjGetErrorStr2"));
            if [init, decompress, destroy, error].iter().any(|p| p.is_null()) {
                return None;
            }
            let handle = std::mem::transmute::<*mut libc::c_void, Init>(init)();
            if handle.is_null() {
                return None;
            }
            Some(Self {
                handle,
                decompress: std::mem::transmute::<*mut libc::c_void, Decompress>(decompress),
                destroy: std::mem::transmute::<*mut libc::c_void, Destroy>(destroy),
                error: std::mem::transmute::<*mut libc::c_void, ErrorText>(error),
            })
        }
    }

    fn decode(&mut self, data: &[u8], out: &mut [u8]) -> Result<()> {
        // The size it says, before writing anything.
        let (w, h) = jpeg_size(data).context("no picture size in it")?;
        ensure!(w as usize * h as usize * 4 == out.len(), "a {w}x{h} picture");
        // SAFETY: a decoder of ours, the whole JPEG, and room for all of it.
        let r = unsafe { (self.decompress)(self.handle, data.as_ptr(), data.len() as libc::c_ulong, out.as_mut_ptr(), w as libc::c_int, (w * 4) as libc::c_int, h as libc::c_int, TJPF_BGRA, TJFLAG_FASTDCT) };
        if r != 0 {
            // SAFETY: its error text, a C string.
            let why = unsafe { std::ffi::CStr::from_ptr((self.error)(self.handle)) }.to_string_lossy().into_owned();
            // A warning (a picture cut short at the end, say) still decoded it.
            if !why.contains("Premature end") && !why.contains("extraneous") {
                bail!("{why}");
            }
        }
        Ok(())
    }
}

impl Drop for Turbo {
    fn drop(&mut self) {
        // SAFETY: our decoder, not used after this.
        unsafe { (self.destroy)(self.handle) };
    }
}

// SAFETY: only used by the thread that has it.
unsafe impl Send for Turbo {}

/// A JPEG's width and height, from its frame header (SOF).
fn jpeg_size(data: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2;
    while i + 9 < data.len() {
        if data[i] != 0xFF {
            return None;
        }
        let marker = data[i + 1];
        let len = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
        if matches!(marker, 0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF) {
            let h = u16::from_be_bytes([data[i + 5], data[i + 6]]) as u32;
            let w = u16::from_be_bytes([data[i + 7], data[i + 8]]) as u32;
            return Some((w, h));
        }
        i += 2 + len;
    }
    None
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

/// `struct v4l2_pix_format`.
#[repr(C)]
struct PixFormat {
    width: u32,
    height: u32,
    pixelformat: u32,
    field: u32,
    bytesperline: u32,
    sizeimage: u32,
    colorspace: u32,
    private: u32,
    flags: u32,
    encoding: u32,
    quantization: u32,
    xfer_func: u32,
}

/// `struct v4l2_format`, for capture: its union at 8 (it holds pointers).
#[repr(C)]
struct V4l2Format {
    kind: u32,
    padding: u32,
    pix: PixFormat,
    rest: [u8; 152],
}

/// `struct v4l2_streamparm`, for capture.
#[repr(C)]
struct StreamParm {
    kind: u32,
    capability: u32,
    capturemode: u32,
    /// Seconds a frame: numerator / denominator.
    numerator: u32,
    denominator: u32,
    extendedmode: u32,
    readbuffers: u32,
    reserved: [u32; 4],
    rest: [u8; 160],
}

/// `struct v4l2_requestbuffers`.
#[repr(C)]
struct RequestBuffers {
    count: u32,
    kind: u32,
    memory: u32,
    capabilities: u32,
    flags: u8,
    reserved: [u8; 3],
}

/// `struct v4l2_buffer` (64-bit): `offset` is the union `m`, whose first
/// 4 bytes are a memory-mapped buffer's offset.
#[repr(C)]
struct V4l2Buffer {
    index: u32,
    kind: u32,
    bytesused: u32,
    flags: u32,
    field: u32,
    padding: u32,
    timestamp: [i64; 2],
    timecode: [u32; 4],
    sequence: u32,
    memory: u32,
    offset: u64,
    length: u32,
    reserved2: u32,
    request_fd: u32,
    padding2: u32,
}

const fn iow(nr: u64, size: usize) -> u64 {
    (1 << 30) | ((size as u64) << 16) | ((b'V' as u64) << 8) | nr
}
const VIDIOC_S_FMT: u64 = iorw(5, std::mem::size_of::<V4l2Format>(), false);
const VIDIOC_REQBUFS: u64 = iorw(8, std::mem::size_of::<RequestBuffers>(), false);
const VIDIOC_QUERYBUF: u64 = iorw(9, std::mem::size_of::<V4l2Buffer>(), false);
const VIDIOC_QBUF: u64 = iorw(15, std::mem::size_of::<V4l2Buffer>(), false);
const VIDIOC_DQBUF: u64 = iorw(17, std::mem::size_of::<V4l2Buffer>(), false);
const VIDIOC_STREAMON: u64 = iow(18, std::mem::size_of::<libc::c_int>());
const VIDIOC_STREAMOFF: u64 = iow(19, std::mem::size_of::<libc::c_int>());
const VIDIOC_S_PARM: u64 = iorw(22, std::mem::size_of::<StreamParm>(), false);
const V4L2_MEMORY_MMAP: u32 = 1;
const V4L2_BUF_FLAG_ERROR: u32 = 0x40;

/// `struct v4l2_queryctrl`.
#[repr(C)]
struct QueryCtrl {
    id: u32,
    kind: u32,
    name: [u8; 32],
    minimum: i32,
    maximum: i32,
    step: i32,
    default_value: i32,
    flags: u32,
    reserved: [u32; 2],
}

/// `struct v4l2_querymenu` (packed): a menu item's name, or for an integer
/// menu its value.
#[repr(C, packed)]
struct QueryMenu {
    id: u32,
    index: u32,
    name: [u8; 32],
    reserved: u32,
}

/// `struct v4l2_control`.
#[repr(C)]
struct Control {
    id: u32,
    value: i32,
}

const VIDIOC_G_CTRL: u64 = iorw(27, std::mem::size_of::<Control>(), false);
const VIDIOC_S_CTRL: u64 = iorw(28, std::mem::size_of::<Control>(), false);
const VIDIOC_QUERYCTRL: u64 = iorw(36, std::mem::size_of::<QueryCtrl>(), false);
const VIDIOC_QUERYMENU: u64 = iorw(37, std::mem::size_of::<QueryMenu>(), false);
const V4L2_CTRL_FLAG_NEXT_CTRL: u32 = 0x8000_0000;
const V4L2_CTRL_FLAG_DISABLED: u32 = 0x1;
const V4L2_CTRL_FLAG_READ_ONLY: u32 = 0x4;
const V4L2_CTRL_FLAG_INACTIVE: u32 = 0x10;
const V4L2_CTRL_TYPE_INTEGER: u32 = 1;
const V4L2_CTRL_TYPE_BOOLEAN: u32 = 2;
const V4L2_CTRL_TYPE_MENU: u32 = 3;
const V4L2_CTRL_TYPE_INTEGER_MENU: u32 = 9;

/// The controls a camera offers that can be set (exposure, focus, white
/// balance…), in its order, with their values now.
pub(crate) fn controls(device: &str) -> Vec<crate::webcam::CameraControl> {
    use crate::webcam::{CameraControl, ControlKind};
    let Some(fd) = open(Path::new(device)) else { return Vec::new() };
    let mut out = Vec::new();
    let mut id = V4L2_CTRL_FLAG_NEXT_CTRL;
    loop {
        // SAFETY: plain integers and byte arrays; all-zero is valid.
        let mut q: QueryCtrl = unsafe { std::mem::zeroed() };
        q.id = id;
        if !ioctl(&fd, VIDIOC_QUERYCTRL, &mut q) {
            break;
        }
        id = q.id | V4L2_CTRL_FLAG_NEXT_CTRL;
        if q.flags & (V4L2_CTRL_FLAG_DISABLED | V4L2_CTRL_FLAG_READ_ONLY) != 0 {
            continue;
        }
        let kind = match q.kind {
            V4L2_CTRL_TYPE_INTEGER => ControlKind::Int { min: q.minimum as i64, max: q.maximum as i64, step: q.step.max(1) as i64 },
            V4L2_CTRL_TYPE_BOOLEAN => ControlKind::Bool,
            V4L2_CTRL_TYPE_MENU | V4L2_CTRL_TYPE_INTEGER_MENU => {
                let items: Vec<(i64, String)> = (q.minimum..=q.maximum)
                    .filter_map(|index| {
                        // SAFETY: as above.
                        let mut m: QueryMenu = unsafe { std::mem::zeroed() };
                        m.id = q.id;
                        m.index = index as u32;
                        if !ioctl(&fd, VIDIOC_QUERYMENU, &mut m) {
                            return None;
                        }
                        let name = { m.name };
                        let label = if q.kind == V4L2_CTRL_TYPE_INTEGER_MENU {
                            i64::from_le_bytes(name[..8].try_into().unwrap()).to_string()
                        } else {
                            String::from_utf8_lossy(&name).trim_end_matches('\0').trim().to_owned()
                        };
                        Some((index as i64, label))
                    })
                    .collect();
                if items.is_empty() {
                    continue;
                }
                ControlKind::Menu(items)
            }
            // Buttons, strings, classes (headings): not offered.
            _ => continue,
        };
        let mut c = Control { id: q.id, value: 0 };
        let value = if ioctl(&fd, VIDIOC_G_CTRL, &mut c) { c.value as i64 } else { q.default_value as i64 };
        out.push(CameraControl {
            id: q.id,
            name: String::from_utf8_lossy(&q.name).trim_end_matches('\0').trim().to_owned(),
            kind,
            value,
            default: q.default_value as i64,
            inactive: q.flags & V4L2_CTRL_FLAG_INACTIVE != 0,
        });
    }
    out
}

/// The open camera's newest picture, shrunk to at most `max_width` wide, as
/// R, G, B, A; `None` if there's none newer than picture `newer_than`.
pub(crate) fn picture(device: &str, max_width: u32, newer_than: u64) -> Option<crate::webcam::CameraPreview> {
    let feed = CameraFeed::of(device, None)?;
    let seq = feed.seq.load(Ordering::Acquire);
    if seq <= newer_than {
        return None;
    }
    let p = feed.latest()?;
    let scale = (max_width as f64 / p.width as f64).min(1.0);
    let (w, h) = (((p.width as f64 * scale).round() as u32).max(1), ((p.height as f64 * scale).round() as u32).max(1));
    let mut rgba = vec![0u8; w as usize * h as usize * 4];
    super::image::resample(&p.bgra, (p.width as usize, p.height as usize), p.width as usize * 4, true, &mut rgba, (w as usize, h as usize));
    Some(crate::webcam::CameraPreview { width: w, height: h, rgba, seq, new_pictures: feed.new_pictures.load(Ordering::Relaxed) })
}

/// Set a camera's control `id` to `value`; it applies at once, in every app.
pub(crate) fn set_control(device: &str, id: u32, value: i64) -> Result<()> {
    let fd = open(Path::new(device)).context("can't open the camera")?;
    let mut c = Control { id, value: value as i32 };
    if !ioctl(&fd, VIDIOC_S_CTRL, &mut c) {
        bail!("the camera didn't take it: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

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


    /// Switching a camera's format reopens it without it being busy (the
    /// one before let go of first).
    #[test]
    #[ignore = "uses a camera nothing else has open: HESTECLIPS_WEBCAM=<device> cargo test -p capture switches_format -- --ignored"]
    fn switches_format() {
        let device = std::env::var("HESTECLIPS_WEBCAM").unwrap();
        let formats = formats_of(&std::fs::canonicalize(&device).unwrap()).unwrap();
        let (a, b) = (formats[0].0, formats[formats.len() / 2].0);
        let live = |want: (u32, u32)| {
            let t = std::time::Instant::now();
            let mut seen = Vec::new();
            loop {
                let s = crate::webcam::status_of(&device);
                if seen.last() != Some(&s) {
                    seen.push(s.clone());
                }
                if s == (crate::webcam::Status::Live { width: want.0, height: want.1 }) || t.elapsed() > Duration::from_secs(8) {
                    return seen;
                }
                thread::sleep(Duration::from_millis(10));
            }
        };
        keep_open(vec![(device.clone(), Some(a))]);
        let first = live((a.width, a.height));
        keep_open(vec![(device.clone(), Some(b))]);
        let t = std::time::Instant::now();
        let second = live((b.width, b.height));
        eprintln!("{} then {} in {} ms: {first:?} → {second:?}", a.label(), b.label(), t.elapsed().as_millis());
        keep_open(Vec::new());
        thread::sleep(Duration::from_millis(300));
        assert!(!second.iter().any(|s| matches!(s, crate::webcam::Status::Unavailable(_))), "busy on the way");
        assert_eq!(second.last(), Some(&crate::webcam::Status::Live { width: b.width, height: b.height }));
    }

    /// With a long exposure the camera repeats pictures: frames keep coming
    /// at its rate, new pictures far fewer.
    #[test]
    #[ignore = "changes a camera's exposure for a moment: HESTECLIPS_WEBCAM=<device> cargo test -p capture repeats_pictures -- --ignored"]
    fn repeats_pictures_with_a_long_exposure() {
        let device = std::env::var("HESTECLIPS_WEBCAM").unwrap();
        let all = controls(&device);
        let auto = all.iter().find(|c| c.id == 0x9a0901).expect("auto exposure").clone();
        let time = all.iter().find(|c| c.id == 0x9a0902).expect("exposure time").clone();
        keep_open(vec![(device.clone(), None)]);
        thread::sleep(Duration::from_secs(2));
        let feed = CameraFeed::of(&device, None).expect("open");
        let rate = |secs: u64| {
            let (f0, n0) = (PICTURES.load(Ordering::Relaxed), feed.new_pictures.load(Ordering::Relaxed));
            thread::sleep(Duration::from_secs(secs));
            ((PICTURES.load(Ordering::Relaxed) - f0) / secs, (feed.new_pictures.load(Ordering::Relaxed) - n0) / secs)
        };
        set_control(&device, auto.id, 1).unwrap();
        set_control(&device, time.id, 50).unwrap();
        thread::sleep(Duration::from_millis(500));
        let short = rate(2);
        set_control(&device, time.id, 1000).unwrap();
        thread::sleep(Duration::from_millis(500));
        let long = rate(2);
        set_control(&device, time.id, time.value).unwrap();
        set_control(&device, auto.id, auto.value).unwrap();
        keep_open(Vec::new());
        eprintln!("frames, new pictures a second: 5 ms {short:?}, 100 ms {long:?}");
        assert!(long.1 < long.0, "no repeats at 100 ms");
    }

    /// Settings remembered for a camera go back on when it's opened.
    #[test]
    #[ignore = "uses a camera nothing else has open: HESTECLIPS_WEBCAM=<device> cargo test -p capture remembered_settings -- --ignored"]
    fn remembered_settings_go_back_on() {
        let device = std::env::var("HESTECLIPS_WEBCAM").unwrap();
        let b = controls(&device).into_iter().find(|c| c.name.to_lowercase().contains("brightness")).expect("brightness");
        crate::webcam::remember_controls(&device, vec![(b.id, b.default + 7)]);
        keep_open(vec![(device.clone(), None)]);
        let t = std::time::Instant::now();
        while !matches!(crate::webcam::status_of(&device), crate::webcam::Status::Live { .. }) && t.elapsed() < Duration::from_secs(10) {
            thread::sleep(Duration::from_millis(50));
        }
        let now = controls(&device).into_iter().find(|c| c.id == b.id).unwrap().value;
        let picture = picture(&device, 320, 0).map(|p| (p.width, p.height));
        keep_open(Vec::new());
        set_control(&device, b.id, b.default).unwrap();
        eprintln!("brightness after opening: {now} (default {}), picture {picture:?}", b.default);
        assert_eq!(now, b.default + 7);
        assert!(picture.is_some_and(|(w, _)| w == 320));
    }

    /// A camera's settings are listed with their values, and one set reads back.
    #[test]
    #[ignore = "uses a camera: HESTECLIPS_WEBCAM=<device> cargo test -p capture camera_settings -- --ignored"]
    fn camera_settings() {
        let device = std::env::var("HESTECLIPS_WEBCAM").unwrap();
        let all = controls(&device);
        for c in &all {
            eprintln!("{:>10x} {:32} {:?} = {} (default {}){}", c.id, c.name, c.kind, c.value, c.default, if c.inactive { " inactive" } else { "" });
        }
        let b = all.iter().find(|c| c.name.to_lowercase().contains("brightness")).expect("brightness");
        set_control(&device, b.id, b.default + 1).unwrap();
        let now = controls(&device).into_iter().find(|c| c.id == b.id).unwrap().value;
        set_control(&device, b.id, b.value).unwrap();
        assert_eq!(now, b.default + 1);
    }
    #[test]
    fn ioctl_numbers_match_the_kernel() {
        assert_eq!(VIDIOC_QUERYCAP, 0x8068_5600);
        assert_eq!(VIDIOC_ENUM_FMT, 0xC040_5602);
        assert_eq!(VIDIOC_ENUM_FRAMESIZES, 0xC02C_564A);
        assert_eq!(VIDIOC_ENUM_FRAMEINTERVALS, 0xC034_564B);
        // And streaming's, from <linux/videodev2.h>.
        assert_eq!(VIDIOC_S_FMT, 0xC0D0_5605);
        assert_eq!(VIDIOC_REQBUFS, 0xC014_5608);
        assert_eq!(VIDIOC_QUERYBUF, 0xC058_5609);
        assert_eq!(VIDIOC_QBUF, 0xC058_560F);
        assert_eq!(VIDIOC_DQBUF, 0xC058_5611);
        assert_eq!(VIDIOC_STREAMON, 0x4004_5612);
        assert_eq!(VIDIOC_STREAMOFF, 0x4004_5613);
        assert_eq!(VIDIOC_S_PARM, 0xC0CC_5616);
        assert_eq!(std::mem::size_of::<V4l2Buffer>(), 88);
        assert_eq!(std::mem::offset_of!(V4l2Buffer, offset), 64);
        assert_eq!(std::mem::offset_of!(V4l2Format, pix), 8);
        assert_eq!(std::mem::offset_of!(StreamParm, numerator), 12);
    }

    /// A 320x240 test picture as ffmpeg makes it in `pix_fmt`, and as B, G,
    /// R, A by ffmpeg's own conversion (`None` without ffmpeg).
    fn made_by_ffmpeg(pix_fmt: &str, codec: &[&str]) -> Option<(Vec<u8>, Vec<u8>)> {
        let make = |out: &[&str]| {
            std::process::Command::new("ffmpeg")
                .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i", "testsrc2=s=320x240:r=1:d=1"])
                .args(["-frames:v", "1", "-pix_fmt", pix_fmt])
                .args(out)
                .args(["-"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| o.stdout)
        };
        let raw = make(&[codec, &["-f", if codec.is_empty() { "rawvideo" } else { "image2pipe" }]].concat())?;
        let mut child = std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error"])
            .args(if codec.is_empty() { vec!["-f", "rawvideo", "-pix_fmt", pix_fmt, "-video_size", "320x240"] } else { vec!["-f", "mjpeg"] })
            .args(["-i", "-", "-f", "rawvideo", "-pix_fmt", "bgra", "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .ok()?;
        let mut stdin = child.stdin.take()?;
        let input = raw.clone();
        let writer = std::thread::spawn(move || {
            use std::io::Write;
            let _ = stdin.write_all(&input);
        });
        let out = child.wait_with_output().ok()?;
        let _ = writer.join();
        Some((raw, out.stdout))
    }

    fn worst(a: &[u8], b: &[u8]) -> u8 {
        assert_eq!(a.len(), b.len());
        a.iter().zip(b).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0)
    }

    /// Straight from the camera: its biggest MJPEG decodes as ffmpeg decodes
    /// the very same pictures, and what that costs this thread a frame,
    /// against reading the camera through an ffmpeg process as before.
    /// Pictures are only compared in memory, never kept.
    #[test]
    #[ignore = "uses a camera nothing else has open: HESTECLIPS_WEBCAM=<device> cargo test --release -p capture streams_directly -- --ignored --nocapture"]
    fn streams_directly() {
        let device = std::env::var("HESTECLIPS_WEBCAM").unwrap();
        let path = std::fs::canonicalize(&device).unwrap();
        let offered = formats_of(&path).unwrap();
        let (format, pixel) = offered.iter().filter(|(_, p)| *p == "mjpeg").max_by_key(|(f, _)| f.auto_rank()).copied().expect("an MJPEG format");
        let thread_cpu = || {
            // SAFETY: a valid rusage to fill.
            let mut u = unsafe { std::mem::zeroed::<libc::rusage>() };
            unsafe { libc::getrusage(libc::RUSAGE_THREAD, &mut u) };
            (u.ru_utime.tv_sec + u.ru_stime.tv_sec) as f64 + (u.ru_utime.tv_usec + u.ru_stime.tv_usec) as f64 / 1e6
        };
        let Ok(mut s) = Stream::open(&path, format, pixel) else { panic!("can't stream it") };
        let (w, h) = (s.width, s.height);
        let mut decoder = Decoder::new(s.pixel, w, h, s.bytes_per_line);
        eprintln!("{} at {} fps, {}", format.label(), format.fps(), if matches!(decoder, Decoder::Jpeg(Some(_))) { "libjpeg-turbo" } else { "zune-jpeg" });
        let mut out = vec![0u8; w as usize * h as usize * 4];
        let mut kept: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        // A second for the camera to settle, then 90 frames timed.
        let (mut n, mut t0, mut c0) = (0, std::time::Instant::now(), thread_cpu());
        while n < 120 {
            let got = s.next(Duration::from_secs(2), |jpeg| (decoder.decode(jpeg, &mut out).is_ok(), (n % 40 == 39).then(|| jpeg.to_vec()))).unwrap();
            let Some((ok, jpeg)) = got else { continue };
            assert!(ok, "a picture didn't decode");
            if let Some(jpeg) = jpeg {
                kept.push((jpeg, out.clone()));
            }
            n += 1;
            if n == 30 {
                (t0, c0) = (std::time::Instant::now(), thread_cpu());
            }
        }
        let (secs, cpu) = (t0.elapsed().as_secs_f64(), thread_cpu() - c0);
        drop(s);
        eprintln!("direct: {:.1} fps, {:.2} ms CPU a frame ({:.0}% of a core at {} fps)", 90.0 / secs, cpu * 1000.0 / 90.0, cpu * 1000.0 / 90.0 * format.fps() as f64 / 10.0, format.fps());
        for (jpeg, ours) in &kept {
            let mut child = std::process::Command::new("ffmpeg")
                .args(["-hide_banner", "-loglevel", "error", "-f", "mjpeg", "-i", "-", "-f", "rawvideo", "-pix_fmt", "bgra", "-"])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let mut stdin = child.stdin.take().unwrap();
            let input = jpeg.clone();
            let writer = std::thread::spawn(move || {
                use std::io::Write;
                let _ = stdin.write_all(&input);
            });
            let theirs = child.wait_with_output().unwrap().stdout;
            let _ = writer.join();
            let mean = ours.iter().zip(&theirs).map(|(a, b)| a.abs_diff(*b) as u64).sum::<u64>() as f64 / ours.len() as f64;
            eprintln!("a {} KB picture: off from ffmpeg's decoding by {mean:.2} on average", jpeg.len() / 1024);
            assert_eq!(ours.len(), theirs.len());
            assert!(mean < 2.0, "off by {mean:.2}");
        }
        // Through an ffmpeg process, as before.
        let mut child = super::super::ffmpeg::ffmpeg()
            .args(["-f", "v4l2", "-input_format", pixel, "-video_size", &format!("{w}x{h}")])
            .args(["-framerate", &format!("{}/{}", format.fps_num, format.fps_den.max(1))])
            .arg("-i")
            .arg(&path)
            .args(["-f", "rawvideo", "-pix_fmt", "bgra", "pipe:1"])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut pipe = child.stdout.take().unwrap();
        let mut frame = vec![0u8; w as usize * h as usize * 4];
        for _ in 0..30 {
            pipe.read_exact(&mut frame).unwrap();
        }
        let child_cpu = |pid: u32| {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
            let f: Vec<&str> = stat[stat.rfind(')').unwrap() + 2..].split_whitespace().collect();
            (f[11].parse::<f64>().unwrap() + f[12].parse::<f64>().unwrap()) / 100.0
        };
        let (t0, c0, k0) = (std::time::Instant::now(), thread_cpu(), child_cpu(child.id()));
        for _ in 0..90 {
            pipe.read_exact(&mut frame).unwrap();
        }
        let (secs, cpu) = (t0.elapsed().as_secs_f64(), thread_cpu() - c0 + child_cpu(child.id()) - k0);
        let _ = child.kill();
        let _ = child.wait();
        eprintln!("through ffmpeg: {:.1} fps, {:.2} ms CPU a frame ({:.0}% of a core at {} fps)", 90.0 / secs, cpu * 1000.0 / 90.0, cpu * 1000.0 / 90.0 * format.fps() as f64 / 10.0, format.fps());
    }

    /// The webcam's raw formats come out as ffmpeg made them (BT.601,
    /// studio range), give or take rounding.
    #[test]
    fn raw_pictures_match_ffmpeg() {
        for (pix_fmt, fourcc) in [("yuyv422", *b"YUYV"), ("uyvy422", *b"UYVY"), ("nv12", *b"NV12"), ("yuv420p", *b"YU12")] {
            let Some((raw, want)) = made_by_ffmpeg(pix_fmt, &[]) else {
                eprintln!("no ffmpeg: not checked");
                return;
            };
            let mut d = Decoder::new(u32::from_le_bytes(fourcc), 320, 240, 0);
            let mut got = vec![0; 320 * 240 * 4];
            d.decode(&raw, &mut got).unwrap();
            let off = worst(&got, &want);
            let mean = got.iter().zip(&want).map(|(a, b)| a.abs_diff(*b) as u64).sum::<u64>() as f64 / got.len() as f64;
            eprintln!("{pix_fmt}: off by {mean:.2} on average, at most {off}");
            // 4:2:0's colour is half as tall: where it changes sharply
            // ffmpeg blends rows, we don't, so only the average is close.
            if matches!(&fourcc, b"NV12" | b"YU12") {
                assert!(mean < 1.5, "{pix_fmt} off by {mean:.2} on average");
            } else {
                assert!(off <= 3, "{pix_fmt} off by {off}");
            }
        }
    }

    /// MJPEG comes out as ffmpeg decodes it, through libjpeg-turbo and
    /// through zune-jpeg; the same with the Huffman tables left out, as
    /// webcams send it (each decoder new: libjpeg's keeps the tables of the
    /// last picture that had them).
    #[test]
    fn mjpeg_matches_ffmpeg() {
        // With the standard Huffman tables, which webcams leave out.
        let Some((jpeg, want)) = made_by_ffmpeg("yuvj422p", &["-c:v", "mjpeg", "-q:v", "2", "-huffman", "default"]) else {
            eprintln!("no ffmpeg: not checked");
            return;
        };
        // The same picture without its DHT segments.
        let mut bare = jpeg[..2].to_vec();
        let mut i = 2;
        while i + 4 <= jpeg.len() {
            let marker = jpeg[i + 1];
            if marker == 0xDA {
                bare.extend_from_slice(&jpeg[i..]);
                break;
            }
            let len = u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]) as usize;
            if marker != 0xC4 {
                bare.extend_from_slice(&jpeg[i..i + 2 + len]);
            }
            i += 2 + len;
        }
        assert!(bare.len() < jpeg.len(), "the test picture had Huffman tables to leave out");
        for name in ["libjpeg-turbo", "zune-jpeg"] {
            for (what, data) in [("with tables", &jpeg), ("without", &bare)] {
                let mut d = Decoder::Jpeg(if name == "zune-jpeg" { None } else { Turbo::load() });
                if matches!(d, Decoder::Jpeg(None)) != (name == "zune-jpeg") {
                    eprintln!("{name}: not on this system");
                    break;
                }
                let mut got = vec![0; 320 * 240 * 4];
                d.decode(data, &mut got).unwrap();
                let mean = got.iter().zip(&want).map(|(a, b)| a.abs_diff(*b) as u64).sum::<u64>() as f64 / got.len() as f64;
                eprintln!("{name}, {what}: off by {mean:.2} on average, at most {}", worst(&got, &want));
                assert!(mean < 1.5, "{name} {what}: off by {mean:.2}");
            }
        }
    }
}
