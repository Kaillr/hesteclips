//! Scrub proxy: every frame of the clip as a small JPEG, kept in memory.
//!
//! Seeking the real file costs 50-200 ms a frame, far too slow to follow a
//! drag. One pass decodes every frame at a small size; after that any frame
//! is a JPEG decode away, so scrubbing updates the picture on every mouse
//! move. Frames arrive in order while it builds, so the start of the clip is
//! scrubbable almost immediately.
//!
//! On Windows the pass uses the GPU's video decoder (`capture::win::decode`)
//! and the CPU only compresses the small frames, on low-priority threads, at
//! full speed even while the clip plays (playback still gets every frame:
//! tested; slowing down left most of a long clip unscrubbable for a minute). It used
//! to be an ffmpeg process decoding every full-size frame in software on every
//! core, which is still the fallback (and elsewhere).

use std::io::Read;
use std::path::Path;
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Proxy frame width; enough for a crisp preview while dragging.
const WIDTH: u32 = 640;

/// JPEG quality of proxy frames: soft but clean at preview size.
const QUALITY: u8 = 75;

/// One frame, shared by every index it stands for (a dropped frame's slot
/// shows the one before it).
type Jpeg = Arc<[u8]>;

pub struct Proxy {
    frames: Arc<Mutex<Vec<Jpeg>>>,
    child: Option<Child>,
    stop: Arc<AtomicBool>,
}

impl Proxy {
    /// Every one of the `total` frames of `source`, numbered at `fps`.
    pub fn build(ctx: &egui::Context, source: &Path, fps: f64, total: usize) -> Self {
        let frames = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        #[cfg(windows)]
        if std::env::var_os("HESTECLIPS_NO_HW_DECODE").is_none() {
            let started = hw::build(ctx, source, fps, total, &frames, &stop);
            if started {
                return Self { frames, child: None, stop };
            }
        }
        let _ = total;
        let child = media::ffmpeg_background()
            .args(["-hide_banner", "-loglevel", "error", "-i"])
            .arg(source)
            // fps= pins one output frame per source frame index, even for VFR input.
            .args(["-an", "-vf", &format!("fps={fps},scale={WIDTH}:-2:flags=bilinear")])
            .args(["-c:v", "mjpeg", "-q:v", "6", "-f", "image2pipe", "-"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok();
        let mut child = child;
        if let Some(stdout) = child.as_mut().and_then(|c| c.stdout.take()) {
            let (frames, ctx) = (frames.clone(), ctx.clone());
            std::thread::spawn(move || split_jpegs(stdout, &frames, &ctx));
        }
        Self { frames, child, stop }
    }

    /// How many frames are ready (they arrive in order).
    pub fn ready(&self) -> usize {
        self.frames.lock().map_or(0, |f| f.len())
    }

    /// Decode proxy frame `idx`, if it's been built yet.
    pub fn frame(&self, idx: u64) -> Option<egui::ColorImage> {
        let jpeg = self.frames.lock().ok()?.get(idx as usize)?.clone();
        let img = image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg).ok()?.to_rgba8();
        Some(egui::ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw()))
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// Split an MJPEG byte stream into individual JPEGs (each SOI 0xFFD8 … EOI 0xFFD9).
///
/// ffmpeg's mjpeg encoder byte-stuffs 0xFF inside entropy-coded data, so the first
/// EOI after a frame's start ends that frame.
fn split_jpegs(mut r: impl Read, frames: &Mutex<Vec<Jpeg>>, ctx: &egui::Context) {
    let mut buf: Vec<u8> = Vec::with_capacity(1 << 20);
    let mut chunk = vec![0u8; 1 << 16];
    // Bytes before `scanned` have been searched for an EOI already.
    let mut scanned = 0usize;
    let mut last_repaint = std::time::Instant::now();
    loop {
        let n = match r.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        buf.extend_from_slice(&chunk[..n]);
        // Search from one byte back so an EOI split across reads is still found;
        // never inside the 2-byte SOI at the start of the frame.
        let mut from = scanned.saturating_sub(1).max(2);
        while from + 1 < buf.len() {
            match buf[from..].windows(2).position(|w| w == [0xFF, 0xD9]) {
                Some(off) => {
                    let end = from + off + 2;
                    if let Ok(mut f) = frames.lock() {
                        f.push(Arc::from(&buf[..end]));
                    }
                    buf.drain(..end);
                    from = 2;
                }
                None => break,
            }
        }
        scanned = buf.len();
        if last_repaint.elapsed().as_millis() > 100 {
            ctx.request_repaint();
            last_repaint = std::time::Instant::now();
        }
    }
    ctx.request_repaint();
}

/// The proxy from the GPU's video decoder.
#[cfg(windows)]
mod hw {
    use super::*;
    use std::sync::mpsc;

    use capture::win::decode::{Decoder, Picture};


    /// Threads compressing frames (the slow part, ~2-5 ms a frame): enough to
    /// get near the decoder's ~900 fps.
    const ENCODERS: usize = 4;

    /// Start building; false if the decoder can't open the clip.
    pub fn build(ctx: &egui::Context, source: &Path, fps: f64, total: usize, frames: &Arc<Mutex<Vec<Jpeg>>>, stop: &Arc<AtomicBool>) -> bool {
        // Open here, so a clip the decoder can't read falls back to ffmpeg.
        let mut dec = match Decoder::open(source, WIDTH, None) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("scrub proxy: hardware decoder unavailable, using ffmpeg: {e:#}");
                return false;
            }
        };
        dec.set_fps(fps);
        // Decoding on one thread, compressing on a few (it's the slower part),
        // then put back in order on another.
        let (tx, jobs) = mpsc::sync_channel::<(u64, Picture)>(ENCODERS * 2);
        let stop_d = stop.clone();
        std::thread::spawn(move || {
            lower_priority();
            let mut seq = 0u64;
            while !stop_d.load(Ordering::Relaxed) {
                match dec.next() {
                    Ok(Some(p)) => {
                        if tx.send((seq, p)).is_err() {
                            break;
                        }
                        seq += 1;
                    }
                    Ok(None) => break,
                    Err(e) => {
                        eprintln!("scrub proxy: {e:#}");
                        break;
                    }
                }
            }
        });
        let jobs = Arc::new(Mutex::new(jobs));
        let (done_tx, done) = mpsc::channel::<(u64, u64, Option<Jpeg>)>();
        for _ in 0..ENCODERS {
            let (jobs, done_tx) = (jobs.clone(), done_tx.clone());
            std::thread::spawn(move || {
                lower_priority();
                loop {
                    let job = jobs.lock().unwrap().recv();
                    let Ok((seq, p)) = job else { return };
                    if done_tx.send((seq, p.index, encode(&p))).is_err() {
                        return;
                    }
                }
            });
        }
        drop(done_tx);
        let (frames, ctx, stop) = (frames.clone(), ctx.clone(), stop.clone());
        let started = std::time::Instant::now();
        std::thread::spawn(move || {
            lower_priority();
            let mut last_repaint = std::time::Instant::now();
            let mut last: Option<Jpeg> = None;
            let mut waiting = std::collections::BTreeMap::new();
            let mut next_seq = 0u64;
            for (seq, index, jpeg) in done {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                waiting.insert(seq, (index, jpeg));
                while let Some((index, jpeg)) = waiting.remove(&next_seq) {
                    next_seq += 1;
                    let Some(jpeg) = jpeg else { continue };
                    place(&mut frames.lock().unwrap(), last.as_ref(), index as usize, &jpeg);
                    last = Some(jpeg);
                }
                if last_repaint.elapsed().as_millis() > 100 {
                    ctx.request_repaint();
                    last_repaint = std::time::Instant::now();
                }
            }
            // To the very end, even if the last frames were dropped.
            let mut f = frames.lock().unwrap();
            if let Some(prev) = last {
                while f.len() < total {
                    f.push(prev.clone());
                }
            }
            if std::env::var_os("HESTECLIPS_DEBUG_VIDEO").is_some() {
                let mut seen = std::collections::HashSet::new();
                let bytes: usize = f.iter().filter(|j| seen.insert(j.as_ptr())).map(|j| j.len()).sum();
                eprintln!("scrub proxy: {} frames in {:.1} s, {} MB", f.len(), started.elapsed().as_secs_f64(), bytes / 1_000_000);
            }
            ctx.request_repaint();
        });
        true
    }

    /// Put decoded frame `i` in its slot. A dropped frame's slots show the
    /// frame before it (`last`), like ffmpeg's `fps` filter; two frames on one
    /// slot keep the later. Frames come in order.
    pub(super) fn place(f: &mut Vec<Jpeg>, last: Option<&Jpeg>, i: usize, jpeg: &Jpeg) {
        if let Some(prev) = last {
            while f.len() < i {
                f.push(prev.clone());
            }
        }
        if f.len() == i + 1 {
            f[i] = jpeg.clone();
        } else if f.len() == i {
            f.push(jpeg.clone());
        }
    }

    fn encode(p: &Picture) -> Option<Jpeg> {
        media::encode_jpeg(&p.rgba, p.width as u16, p.height as u16, QUALITY).map(Arc::from)
    }

    /// This thread yields to the game, the app and playback.
    fn lower_priority() {
        use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL};
        unsafe {
            let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Gaps repeat the frame before; a slot two frames land on keeps the later.
    #[cfg(windows)]
    #[test]
    fn frames_fill_their_slots() {
        let j = |n: u8| -> Jpeg { Arc::from(vec![n]) };
        let mut f = Vec::new();
        let mut last: Option<Jpeg> = None;
        for (i, n) in [(0, 0), (1, 1), (3, 3), (3, 4), (6, 6)] {
            hw::place(&mut f, last.as_ref(), i, &j(n));
            last = Some(j(n));
        }
        let got: Vec<u8> = f.iter().map(|x| x[0]).collect();
        assert_eq!(got, [0, 1, 1, 4, 4, 4, 6]);
    }

    /// Frames split correctly no matter how reads chop the stream.
    #[test]
    fn splits_across_read_boundaries() {
        let frame = |b: u8| vec![0xFF, 0xD8, b, b, 0xFF, 0x00, b, 0xFF, 0xD9];
        let stream: Vec<u8> = (1..=5).flat_map(frame).collect();
        for chunk in 1..stream.len() {
            struct Chunked(Vec<u8>, usize);
            impl Read for Chunked {
                fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
                    let n = self.1.min(self.0.len()).min(out.len());
                    out[..n].copy_from_slice(&self.0[..n]);
                    self.0.drain(..n);
                    Ok(n)
                }
            }
            let frames = Mutex::new(Vec::new());
            split_jpegs(Chunked(stream.clone(), chunk), &frames, &egui::Context::default());
            let got: Vec<Vec<u8>> = frames.into_inner().unwrap().iter().map(|f| f.to_vec()).collect();
            assert_eq!(got, (1..=5).map(frame).collect::<Vec<_>>(), "chunk size {chunk}");
        }
    }
}

#[cfg(test)]
mod speed {
    /// `cargo test --release -p hesteclips proxy_jpeg_speed -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn proxy_jpeg_speed() {
        let (w, h) = (640u16, 360u16);
        let rgba: Vec<u8> = (0..w as usize * h as usize * 4).map(|i| ((i * 31) ^ (i / 2560 * 7)) as u8).collect();
        for q in [60u8, 75] {
            let t = std::time::Instant::now();
            let mut size = 0;
            for _ in 0..100 {
                size = media::encode_jpeg(&rgba, w, h, q).unwrap().len();
            }
            println!("q{q}: {:.2} ms/frame, {} KB", t.elapsed().as_secs_f64() * 10.0, size / 1024);
        }
        // Colours survive the round trip (encoder in, `image` out, as scrubbing does).
        let flat: Vec<u8> = (0..w as usize * h as usize).flat_map(|_| [200u8, 60, 30, 255]).collect();
        let out = media::encode_jpeg(&flat, w, h, 75).unwrap();
        let back = image::load_from_memory_with_format(&out, image::ImageFormat::Jpeg).unwrap().to_rgba8();
        let px = back.get_pixel(320, 180).0;
        println!("flat (200,60,30) comes back as {px:?}");
        assert!(px[0].abs_diff(200) <= 3 && px[1].abs_diff(60) <= 3 && px[2].abs_diff(30) <= 3);
    }
}

#[cfg(test)]
mod formats {
    /// JPEG vs PNG vs raw for a proxy frame: `PROXY_FRAME=<640x360 rgba file>
    /// cargo test --release -p hesteclips proxy_formats -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn proxy_formats() {
        let Some(path) = std::env::var_os("PROXY_FRAME") else { return };
        let rgba = std::fs::read(path).unwrap();
        let (w, h) = (640u32, 360u32);
        let time = |f: &mut dyn FnMut()| {
            let t = std::time::Instant::now();
            for _ in 0..50 {
                f();
            }
            t.elapsed().as_secs_f64() * 1000.0 / 50.0
        };
        let jpeg = media::encode_jpeg(&rgba, w as u16, h as u16, 75).unwrap();
        let enc_j = time(&mut || drop(media::encode_jpeg(&rgba, w as u16, h as u16, 75)));
        let dec_j = time(&mut || drop(image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg).unwrap().to_rgba8()));
        let img = image::RgbaImage::from_raw(w, h, rgba.clone()).unwrap();
        let mut png = Vec::new();
        let enc_p = time(&mut || {
            png.clear();
            img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
        });
        let dec_p = time(&mut || drop(image::load_from_memory_with_format(&png, image::ImageFormat::Png).unwrap().to_rgba8()));
        let mb = |b: usize| b as f64 * 7013.0 / 1e6;
        println!("raw : {:>4} KB/frame -> {:>5.0} MB for a 2-min clip", rgba.len() / 1024, mb(rgba.len()));
        println!("jpeg: {:>4} KB/frame -> {:>5.0} MB, encode {enc_j:.1} ms, decode {dec_j:.1} ms", jpeg.len() / 1024, mb(jpeg.len()));
        println!("png : {:>4} KB/frame -> {:>5.0} MB, encode {enc_p:.1} ms, decode {dec_p:.1} ms", png.len() / 1024, mb(png.len()));
    }
}
