//! Recording with NVIDIA's video encoder (NVENC) directly (`nvenc.c`)
//! instead of through an ffmpeg process. Where the frame is made on the
//! graphics card (`super::gpu_frame`), it's made straight into memory NVENC
//! reads (Vulkan's, shared with CUDA), so no frame comes down to the CPU,
//! through a pipe to another process and back up to the card. Frames made
//! on the CPU (the screen read without the card) are copied up once.
//!
//! The pacer puts each frame in a free slot and goes on; a thread of ours
//! makes every call to the encoder (NVIDIA's driver lets one call wait on
//! another's: a frame handed in while the last one is being taken out
//! waited for it): it hands each frame to the card as it comes, with a few
//! in flight so the encoder never waits for the next, and takes each one's
//! bitstream out when it's done, in order, for the writer. A frame is
//! skipped only when every slot is busy.
//!
//! Needs NVIDIA's driver 470.57 or newer (NVENC API 11.1).

use std::collections::VecDeque;
use std::ffi::{CStr, c_char, c_int, c_void};
use std::os::fd::IntoRawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError};
use std::thread::{self, JoinHandle};

use anyhow::{Result, bail};

use super::gpu_frame::{Nv12Source, Nv12Target, nv12_len};
use crate::mp4file::{EncodedFrame, VideoFrame};
use crate::mp4mux::{AvcConfig, annexb_to_avcc};
use crate::writer::{Command as WriterCommand, Media};

unsafe extern "C" {
    fn hcnv_open(
        uuid: *const u8,
        width: c_int,
        height: c_int,
        fps: c_int,
        kbps: c_int,
        max_kbps: c_int,
        buf_kbits: c_int,
        gop: c_int,
        preset: c_int,
        quality: c_int,
        bframes: *mut c_int,
        code: *mut c_int,
        err: *mut c_char,
        errlen: c_int,
    ) -> *mut c_void;
    fn hcnv_add_shared(e: *mut c_void, fd: c_int, alloc: u64, size: u64, pitch: c_int, err: *mut c_char, errlen: c_int) -> c_int;
    fn hcnv_add_own(e: *mut c_void, pitch: c_int, err: *mut c_char, errlen: c_int) -> c_int;
    fn hcnv_upload(e: *mut c_void, slot: c_int, nv12: *const u8, err: *mut c_char, errlen: c_int) -> c_int;
    fn hcnv_encode(e: *mut c_void, slot: c_int, pts: i64, err: *mut c_char, errlen: c_int) -> c_int;
    fn hcnv_lock(e: *mut c_void, slot: c_int, data: *mut *const u8, len: *mut u32, pts: *mut i64, key: *mut c_int, err: *mut c_char, errlen: c_int) -> c_int;
    fn hcnv_unlock(e: *mut c_void, slot: c_int, locked: c_int);
    fn hcnv_release(e: *mut c_void, slot: c_int);
    fn hcnv_end(e: *mut c_void);
    fn hcnv_close(e: *mut c_void);
}

/// `hcnv_open`'s code for "no NVIDIA driver here".
const NO_DRIVER: c_int = 1;

/// Room for the C side to say what went wrong.
struct Why([c_char; 512]);

impl Why {
    fn new() -> Self {
        Self([0; 512])
    }

    fn buf(&mut self) -> *mut c_char {
        self.0.as_mut_ptr()
    }

    fn room(&self) -> c_int {
        self.0.len() as c_int
    }

    fn text(&self) -> String {
        // SAFETY: zeroed to begin with, and the C side writes terminated strings.
        unsafe { CStr::from_ptr(self.0.as_ptr()) }.to_string_lossy().trim().to_owned()
    }
}

/// The C side's encoder: frames go in on the pacer's thread and come out on
/// ours at the same time, which NVIDIA's synchronous mode allows (each call
/// makes the session's CUDA context current on its own thread).
struct Session(*mut c_void);

// SAFETY: see above; the pointer is only freed on drop, when no one else has it.
unsafe impl Send for Session {}
// SAFETY: as above.
unsafe impl Sync for Session {}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: ours, and every frame has been taken.
        unsafe { hcnv_close(self.0) };
    }
}

/// One frame's memory.
struct Slot {
    index: c_int,
    /// Where the card makes the frame; `None`: CUDA's own memory, the frame
    /// copied into it from ours.
    target: Option<Nv12Target>,
    /// Which picture it holds (`Picture::version`): a still screen isn't
    /// made or copied again.
    holds: Option<u64>,
}

/// How many frames can be in flight: a twentieth of a second's, so a slow
/// moment doesn't skip frames, at least 6, at most 32 (each is a frame's
/// memory on the card, 5.5 MB at 1440p).
fn slots(fps: u32) -> usize {
    (fps as usize / 20).clamp(6, 32)
}

/// NVIDIA's preset for this many pixels a second (1 fastest, 7 best): the
/// slower presets compress better, but one encoder only gets through so
/// many pixels a second with each, and past that frames are skipped. An RTX
/// 4060 Ti's (one encoder, moving pictures) takes ~1 billion a second at P4
/// (1080p 475 fps, 1440p 275) and ~1.3 billion at P2 (1080p 612, 1440p
/// 386); older cards' fewer, so each step leaves room.
fn preset(width: u32, height: u32, fps: u32) -> c_int {
    let rate = width as u64 * height as u64 * fps as u64;
    match rate {
        r if r <= 1920 * 1080 * 144 => 4,
        r if r <= 2560 * 1440 * 165 => 3,
        r if r <= 2560 * 1440 * 240 => 2,
        _ => 1,
    }
}

/// A running encoder.
pub(crate) struct Nvenc {
    /// Slots free for the next frame, and where they come back to.
    free: Receiver<Slot>,
    free_tx: Sender<Slot>,
    /// Frames to encode, to the thread that does.
    frames: Option<SyncSender<(Slot, i64)>>,
    worker: Option<JoinHandle<()>>,
    session: Option<Arc<Session>>,
    /// Encoding stopped working (said once): every frame since is skipped.
    broken: Arc<AtomicBool>,
    /// Bytes in a frame from the CPU.
    nv12_len: usize,
}

impl Nvenc {
    /// Start encoding `width`×`height` frames at `fps` and `kbps` into
    /// `out`: made on the card by `picture`, or (`None`) handed in from the
    /// CPU. For an `export` (not live: frames can take longer) tuned for
    /// quality, with B-frames (frames then reach `out`
    /// in decode order, up to 3 before the frames shown first: see
    /// `Spec::reorder`), for a better picture in as many bits. `Ok(None)`
    /// where there's no NVIDIA driver.
    pub(crate) fn start(width: u32, height: u32, fps: u32, kbps: u32, out: Sender<WriterCommand>, picture: Option<&dyn Nv12Source>, export: bool) -> Result<Option<Self>> {
        // Exports too: slower presets made the same picture (P6 against P4,
        // tuned for quality, within 0.01 dB SSIM) at two thirds the speed.
        let preset = preset(width, height, fps);
        let started = Self::open(width, height, fps, kbps, preset, export, out, picture);
        if let Ok(Some(_)) = &started {
            let from = if picture.is_some() { "made in the card's memory" } else { "copied up from ours" };
            eprintln!("video encoder: NVIDIA NVENC ({width}x{height} at {fps} fps, preset P{preset}, frames {from})");
        }
        started
    }

    #[allow(clippy::too_many_arguments)]
    fn open(width: u32, height: u32, fps: u32, kbps: u32, preset: c_int, export: bool, out: Sender<WriterCommand>, picture: Option<&dyn Nv12Source>) -> Result<Option<Self>> {
        let kbps = kbps.max(100);
        // A file to watch, not to cut clips from: keyframes every few
        // seconds (as x264's), the bitrate free to follow the picture.
        let (gop, max_kbps, buf_kbits) = if export { ((fps * 4).min(600), kbps * 3, kbps * 6) } else { (crate::keyframe_every(fps), kbps * 3 / 2, kbps * 2) };
        let uuid = picture.map(|p| p.gpu().uuid);
        let (mut why, mut code, mut bframes) = (Why::new(), 0, 0);
        // SAFETY: valid arguments, and room for the error.
        let raw = unsafe {
            hcnv_open(
                uuid.as_ref().map_or(std::ptr::null(), |u| u.as_ptr()),
                width as c_int,
                height as c_int,
                fps as c_int,
                kbps as c_int,
                max_kbps as c_int,
                buf_kbits as c_int,
                gop as c_int,
                preset,
                c_int::from(export),
                &mut bframes,
                &mut code,
                why.buf(),
                why.room(),
            )
        };
        if raw.is_null() {
            if code == NO_DRIVER {
                return Ok(None);
            }
            bail!("{}", why.text());
        }
        let session = Arc::new(Session(raw));
        let (free_tx, free) = mpsc::channel();
        // B-frames wait on the card for the frame after them, and their
        // bitstreams come out in another order: more frames are out at once.
        let count = slots(fps).max(IN_FLIGHT + 2 * bframes as usize + 2);
        let (frames, frames_rx) = mpsc::sync_channel(count);
        // Made now, so that dropping it on the way out frees everything in order.
        let mut enc = Self {
            free,
            free_tx: free_tx.clone(),
            frames: Some(frames),
            worker: None,
            session: Some(session.clone()),
            broken: Arc::new(AtomicBool::new(false)),
            nv12_len: nv12_len(width, height),
        };
        // Rows aligned for the encoder where the card makes the frame; as
        // they come where it's copied from ours.
        let pitch = if picture.is_some() { width.next_multiple_of(256) } else { width };
        for _ in 0..count {
            let mut why = Why::new();
            let slot = match picture {
                Some(p) => {
                    let size = (pitch as u64 * height as u64 * 3 / 2).next_multiple_of(4);
                    let (buffer, fd, allocated) = p.gpu().shared_buffer(size)?;
                    // SAFETY: the session is open; the fd is handed over.
                    let index = unsafe { hcnv_add_shared(raw, fd.into_raw_fd(), allocated, size, pitch as c_int, why.buf(), why.room()) };
                    Slot { index, target: Some(p.nv12_target(buffer, pitch, (width, height), None)), holds: None }
                }
                // SAFETY: the session is open.
                None => Slot { index: unsafe { hcnv_add_own(raw, pitch as c_int, why.buf(), why.room()) }, target: None, holds: None },
            };
            let failed = slot.index < 0;
            let _ = free_tx.send(slot);
            if failed {
                bail!("{}", why.text());
            }
        }
        let broken = enc.broken.clone();
        enc.worker = Some(thread::Builder::new().name("nvenc".into()).spawn(move || encode_frames(&session, frames_rx, free_tx, &broken, fps, out))?);
        Ok(Some(enc))
    }

    /// Encode frame `n`, made on the card by `picture` (its picture number
    /// `version`). False if it's skipped: every slot is busy, or encoding
    /// stopped working.
    pub(crate) fn encode_gpu(&self, picture: &dyn Nv12Source, version: u64, n: i64) -> bool {
        let Some(mut slot) = self.slot() else { return false };
        if slot.holds != Some(version) {
            if let Some(target) = &slot.target {
                picture.nv12_to(target);
            }
            slot.holds = Some(version);
        }
        self.hand_in(slot, n)
    }

    /// Encode frame `n`, `nv12` from the CPU (its picture number `version`).
    pub(crate) fn encode_cpu(&self, nv12: &[u8], version: u64, n: i64) -> bool {
        if nv12.len() < self.nv12_len {
            return false;
        }
        let Some(mut slot) = self.slot() else { return false };
        if slot.holds != Some(version) {
            let mut why = Why::new();
            let copied = match &self.session {
                Some(s) if slot.target.is_none() => {
                    // SAFETY: a slot of CUDA's own memory, free; `nv12` is a whole frame.
                    let done = unsafe { hcnv_upload(s.0, slot.index, nv12.as_ptr(), why.buf(), why.room()) };
                    done == 0
                }
                _ => false,
            };
            if !copied {
                stop(&self.broken, &why);
                let _ = self.free_tx.send(slot);
                return false;
            }
            slot.holds = Some(version);
        }
        self.hand_in(slot, n)
    }

    /// A free slot, if encoding still works.
    fn slot(&self) -> Option<Slot> {
        if self.broken.load(Ordering::Relaxed) {
            return None;
        }
        self.free.try_recv().ok()
    }

    fn hand_in(&self, slot: Slot, n: i64) -> bool {
        match &self.frames {
            Some(frames) => frames.send((slot, n)).is_ok(),
            None => false,
        }
    }

    /// Whether encoding stopped working.
    pub(crate) fn failed(&self) -> bool {
        self.broken.load(Ordering::Relaxed)
    }

    /// Encode what's handed in and wait for the last frame.
    pub(crate) fn finish(mut self) {
        drop(self.frames.take());
        if let Some(t) = self.worker.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Nvenc {
    fn drop(&mut self) {
        // The encoding thread finishes first (it has the session), then the
        // session closes, letting go of the slots' memory, and only then is
        // that memory freed.
        drop(self.frames.take());
        if let Some(t) = self.worker.take() {
            let _ = t.join();
        }
        let slots: Vec<Slot> = self.free.try_iter().collect();
        drop(self.session.take());
        drop(slots);
    }
}

/// Frames in flight on the card at most: enough that it always has the
/// next one when it finishes a frame (handing one in takes ~0.1 ms).
const IN_FLIGHT: usize = 3;

fn stop(broken: &AtomicBool, why: &Why) {
    if !broken.swap(true, Ordering::Relaxed) {
        eprintln!("video encoder (NVENC) stopped: {}", why.text());
    }
}

/// A frame handed to the card: its slot, its number, whether its slot's
/// bitstream has been taken out (the next one out when it went in, in the
/// order frames are decoded: with B-frames, maybe another frame's), and
/// whether its own frame has (then its picture is no longer needed).
struct Flying {
    slot: Slot,
    n: i64,
    taken: bool,
    encoded: bool,
}

/// Hand each frame to the card as it comes, and take the bitstreams out
/// when they're done, in order: the oldest when [`IN_FLIGHT`] are being
/// encoded, or when no new frame is waiting. B-frames wait for the frame
/// after them before any of them is encoded. Each frame is passed to the
/// writer as the frame number it went in with, and slots are freed once
/// their bitstream and their frame are both out.
fn encode_frames(session: &Session, frames: Receiver<(Slot, i64)>, free: Sender<Slot>, broken: &AtomicBool, fps: u32, out: Sender<WriterCommand>) {
    let mut flight: VecDeque<Flying> = VecDeque::new();
    // In `flight`: the next bitstream to take out, and the end of those
    // being encoded (the ones after wait for more frames).
    let (mut next_out, mut encoding) = (0usize, 0usize);
    let mut config: Option<AvcConfig> = None;
    let mut writer_gone = false;
    let (mut ended, mut flushed) = (false, false);
    loop {
        let next = if ended {
            None
        } else if next_out == encoding {
            match frames.recv() {
                Ok(f) => Some(f),
                Err(_) => {
                    ended = true;
                    None
                }
            }
        } else {
            match frames.try_recv() {
                Ok(f) => Some(f),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => {
                    ended = true;
                    None
                }
            }
        };
        if let Some((slot, n)) = next {
            if broken.load(Ordering::Relaxed) {
                let _ = free.send(slot);
                continue;
            }
            let mut why = Why::new();
            // SAFETY: a slot with a frame in it, not on the card.
            match unsafe { hcnv_encode(session.0, slot.index, n, why.buf(), why.room()) } {
                0 => {
                    flight.push_back(Flying { slot, n, taken: false, encoded: false });
                    encoding = flight.len();
                }
                1 => flight.push_back(Flying { slot, n, taken: false, encoded: false }),
                _ => {
                    stop(broken, &why);
                    let _ = free.send(slot);
                    continue;
                }
            }
            if encoding - next_out < IN_FLIGHT {
                continue;
            }
        }
        if ended && !flushed {
            // SAFETY: the session is open; frames waiting get encoded.
            unsafe { hcnv_end(session.0) };
            flushed = true;
            encoding = flight.len();
        }
        if next_out == encoding {
            if ended {
                break;
            }
            continue;
        }
        // Take out the oldest bitstream.
        let index = flight[next_out].slot.index;
        next_out += 1;
        let (mut data, mut len, mut pts, mut key) = (std::ptr::null(), 0u32, 0i64, 0);
        let mut why = Why::new();
        // SAFETY: the oldest bitstream, being encoded.
        let locked = unsafe { hcnv_lock(session.0, index, &mut data, &mut len, &mut pts, &mut key, why.buf(), why.room()) } == 0;
        // SAFETY: locked, the bitstream is `len` bytes at `data` until unlocked.
        let frame = locked.then(|| annexb_to_avcc(unsafe { std::slice::from_raw_parts(data, len as usize) }));
        // SAFETY: as above; the bitstream isn't read after this.
        unsafe { hcnv_unlock(session.0, index, c_int::from(locked)) };
        flight[next_out - 1].taken = true;
        // The frame in it is encoded: its picture can go. (Failed: the one
        // that went in with it, so every slot still comes back.)
        let done = if locked { flight.iter().position(|f| f.n == pts && !f.encoded) } else { None }.unwrap_or(next_out - 1);
        // SAFETY: that frame is no longer on the card.
        unsafe { hcnv_release(session.0, flight[done].slot.index) };
        flight[done].encoded = true;
        while flight.front().is_some_and(|f| f.taken && f.encoded) {
            let f = flight.pop_front().expect("a frame");
            let _ = free.send(f.slot);
            next_out -= 1;
            encoding -= 1;
        }
        let Some(frame) = frame else {
            stop(broken, &why);
            continue;
        };
        if frame.data.is_empty() || writer_gone {
            continue;
        }
        if let Some(c) = frame.config {
            config = Some(c);
        }
        let key = frame.idr;
        let media = Media::Video {
            frame: VideoFrame(Arc::new(EncodedFrame { data: Arc::from(frame.data), config: if key { config.clone() } else { None } })),
            pts: pts as f64 / fps as f64,
            key,
        };
        writer_gone = out.send(WriterCommand::Media(media)).is_err();
    }
    // Anything left (encoding failed on the way): its picture goes too.
    for f in flight {
        if !f.encoded {
            // SAFETY: the session is open; nothing more is encoded.
            unsafe { hcnv_release(session.0, f.slot.index) };
        }
        let _ = free.send(f.slot);
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::super::gpu;
    use super::super::gpu_frame::GpuPicture;
    use super::super::image::Canvas;
    use super::super::screen::{Frame, Order};
    use super::*;

    const W: u32 = 640;
    const H: u32 = 360;

    /// A picture that moves: gradients shifting with `i`.
    fn picture(i: u32) -> Frame {
        let mut data = Vec::with_capacity((W * H * 4) as usize);
        for y in 0..H {
            for x in 0..W {
                data.extend_from_slice(&[((x + y) / 2 + i * 3) as u8, (y + i * 2) as u8, (x + i * 4) as u8, 255]);
            }
        }
        Frame { width: W, height: H, stride: W as usize * 4, order: Order::Bgr, data, texture: None }
    }

    /// The frames the writer got, as H.264 (Annex B) for ffmpeg, and their times.
    fn stream(rx: &Receiver<WriterCommand>) -> (Vec<u8>, Vec<(f64, bool)>) {
        let (mut out, mut frames) = (Vec::new(), Vec::new());
        for command in rx.try_iter() {
            let WriterCommand::Media(Media::Video { frame, pts, key }) = command else { continue };
            if let Some(c) = &frame.0.config {
                for nal in [&c.sps, &c.pps] {
                    out.extend_from_slice(&[0, 0, 0, 1]);
                    out.extend_from_slice(nal);
                }
            }
            let mut d = &frame.0.data[..];
            while d.len() >= 4 {
                let n = u32::from_be_bytes([d[0], d[1], d[2], d[3]]) as usize;
                out.extend_from_slice(&[0, 0, 0, 1]);
                out.extend_from_slice(&d[4..4 + n]);
                d = &d[4 + n..];
            }
            frames.push((pts, key));
        }
        (out, frames)
    }

    /// Hand a frame in, waiting while every slot is busy.
    fn hand_in(mut encode: impl FnMut() -> bool) {
        let until = Instant::now() + Duration::from_secs(5);
        while !encode() {
            assert!(Instant::now() < until, "the encoder took no frame for 5 s");
            std::thread::sleep(Duration::from_micros(200));
        }
    }

    /// What NVENC makes of frames from the card and from the CPU is what
    /// went in, give or take a little compression; frame N is at N/fps, the
    /// first a keyframe with the parameter sets.
    #[test]
    #[ignore = "uses NVIDIA's encoder: cargo test -p capture nvenc -- --ignored"]
    fn encodes_what_it_is_given() {
        let card = gpu::gpu().expect("a graphics card");
        let place = crate::ScreenPlacement::whole(crate::Framing::Fit);
        let n = 30u32;
        for on_card in [true, false] {
            let mut pic = GpuPicture::new(card.clone(), W, H, place);
            let (tx, rx) = mpsc::channel();
            let Some(enc) = Nvenc::start(W, H, 60, 20_000, tx, on_card.then_some(&pic), false).unwrap() else {
                eprintln!("no NVIDIA driver: nothing to test");
                return;
            };
            let mut wanted = Vec::new();
            for i in 0..n {
                let frame = picture(i);
                let mut want = Vec::new();
                if on_card {
                    assert!(pic.draw(&frame, i as u64 + 1));
                    pic.nv12(&mut want);
                    hand_in(|| enc.encode_gpu(&pic, i as u64, i as i64));
                } else {
                    let mut canvas = Canvas::new(W, H);
                    canvas.draw_screen(&frame, &place, false);
                    canvas.to_nv12(&mut want);
                    hand_in(|| enc.encode_cpu(&want, i as u64, i as i64));
                }
                wanted.push(want);
            }
            enc.finish();
            let (h264, frames) = stream(&rx);
            assert_eq!(frames.len(), n as usize, "every frame came out");
            assert!(frames[0].1, "the first is a keyframe");
            for (i, (pts, _)) in frames.iter().enumerate() {
                assert!((pts - i as f64 / 60.0).abs() < 1e-9, "frame {i} at {pts}");
            }
            let dir = std::env::temp_dir().join(format!("hc-nvenc-{}-{on_card}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let file = dir.join("out.h264");
            std::fs::write(&file, &h264).unwrap();
            let decoded = std::process::Command::new("ffmpeg")
                .args(["-hide_banner", "-loglevel", "error", "-f", "h264", "-i"])
                .arg(&file)
                .args(["-f", "rawvideo", "-pix_fmt", "nv12", "-"])
                .output()
                .unwrap();
            let _ = std::fs::remove_dir_all(&dir);
            assert!(decoded.status.success(), "ffmpeg reads it: {}", String::from_utf8_lossy(&decoded.stderr));
            let size = nv12_len(W, H);
            assert_eq!(decoded.stdout.len(), size * n as usize);
            for (i, (got, want)) in decoded.stdout.chunks(size).zip(&wanted).enumerate() {
                let off: u64 = got.iter().zip(want).map(|(a, b)| (*a as i32 - *b as i32).unsigned_abs() as u64).sum();
                let mean = off as f64 / size as f64;
                assert!(mean < 2.0, "{} frame {i} off by {mean:.2} on average", if on_card { "card" } else { "CPU" });
            }
            eprintln!("{}: {n} frames match", if on_card { "frames made on the card" } else { "frames from the CPU" });
        }
    }

    /// How many frames a second NVENC takes from the card at each preset,
    /// and the CPU time each costs us: the frames are 8 different pictures
    /// already on the card, as a game's are, scaled and made NV12 there.
    #[test]
    #[ignore = "uses NVIDIA's encoder, takes a minute: cargo test --release -p capture nvenc_throughput -- --ignored --nocapture"]
    fn nvenc_throughput() {
        let card = gpu::gpu().expect("a graphics card");
        let place = crate::ScreenPlacement::whole(crate::Framing::Fit);
        let cpu_time = || {
            let mut u = unsafe { std::mem::zeroed::<libc::rusage>() };
            // SAFETY: a valid rusage to fill.
            unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
            (u.ru_utime.tv_sec + u.ru_stime.tv_sec) as f64 + (u.ru_utime.tv_usec + u.ru_stime.tv_usec) as f64 / 1e6
        };
        for (w, h) in [(1920u32, 1080u32), (2560, 1440)] {
            // Pictures on the card, as the screen's arrive.
            let textures: Vec<Frame> = (0..8)
                .map(|k| {
                    let t = card.device.create_texture(&wgpu::TextureDescriptor {
                        label: None,
                        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format: wgpu::TextureFormat::Bgra8Unorm,
                        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                        view_formats: &[],
                    });
                    let data: Vec<u8> = (0..w * h).flat_map(|p| {
                        let (x, y) = (p % w, p / w);
                        [((x + y) / 4 + k * 9) as u8, (y / 2 + k * 5) as u8, ((x ^ y) / 8 + k * 3) as u8, 255]
                    }).collect();
                    card.queue.write_texture(
                        t.as_image_copy(),
                        &data,
                        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: Some(h) },
                        wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                    );
                    Frame { width: w, height: h, stride: 0, order: Order::Bgr, data: Vec::new(), texture: Some(t) }
                })
                .collect();
            for preset in [1, 2, 3, 4] {
                let mut pic = GpuPicture::new(card.clone(), w, h, place);
                let (tx, rx) = mpsc::channel();
                let Some(enc) = Nvenc::open(w, h, 540, 50_000, preset, false, tx, Some(&pic)).unwrap() else {
                    eprintln!("no NVIDIA driver");
                    return;
                };
                let frames = 1620u32;
                let (t0, c0) = (Instant::now(), cpu_time());
                let mut waited = 0u32;
                for i in 0..frames {
                    assert!(pic.draw(&textures[(i % 8) as usize], i as u64 + 1));
                    while !enc.encode_gpu(&pic, i as u64, i as i64) {
                        waited += 1;
                        std::thread::sleep(Duration::from_micros(100));
                    }
                }
                enc.finish();
                let (secs, cpu) = (t0.elapsed().as_secs_f64(), cpu_time() - c0);
                let out = rx.try_iter().filter(|c| matches!(c, WriterCommand::Media(_))).count();
                assert_eq!(out, frames as usize);
                eprintln!(
                    "{w}x{h} P{preset}: {:.0} fps, {:.2} ms CPU a frame ({:.0}% of a core at 540 fps), waited for a slot {waited} times",
                    frames as f64 / secs,
                    cpu * 1000.0 / frames as f64,
                    cpu * 1000.0 / frames as f64 * 54.0
                );
            }
        }
    }
}
