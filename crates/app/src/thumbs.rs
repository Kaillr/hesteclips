//! Clip preview images.
//!
//! A clip's first frame is decoded on a few background workers, by the
//! decoder playback uses (ffmpeg for what that can't open), and cached as a
//! JPEG (and the clip's duration beside it) under the OS cache dir, keyed by
//! path + mtime + size so a replaced file gets a fresh thumbnail. A
//! screenshot is simply made smaller. Each picture is uploaded to the GPU
//! once and reused.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::clips::Clip;

/// Thumbnail width in pixels; cards are displayed at roughly half this, so they
/// stay crisp on Retina displays.
const WIDTH: u32 = 480;

pub struct Thumb {
    pub texture: Option<egui::TextureHandle>,
    pub duration: Option<Duration>,
}

/// Width of [`Thumb::small`].
pub const SMALL_W: usize = 40;

struct Done {
    key: u64,
    image: Option<egui::ColorImage>,
    duration: Option<Duration>,
}

pub struct Thumbs {
    ctx: egui::Context,
    /// With when each was last asked for.
    ready: HashMap<u64, (Thumb, Instant)>,
    pending: HashSet<u64>,
    job_tx: Sender<(u64, PathBuf)>,
    done_rx: Receiver<Done>,
    /// When unused pictures were last let go.
    swept: Instant,
    /// Clip lengths read from the disk cache, so sorting by length doesn't
    /// read a file per clip every frame.
    durations: HashMap<u64, Option<Duration>>,
    /// Goes up when a clip's length becomes known: sorting by length sorts again.
    lengths_version: u64,
    /// Every tiny picture made so far (a few KB each), kept when the big one
    /// is let go: the library panel's glass blurs them, also for clips
    /// scrolled away.
    smalls: HashMap<u64, std::sync::Arc<(usize, usize, Vec<u8>)>>,
}

/// Pictures kept however long ago they were shown: a screenful or two and
/// some scrolling around (each is ~0.5 MB on the graphics card). Past this,
/// ones not shown for [`UNUSED`] are let go; they come back from the disk
/// cache in a moment. Both guesses.
const KEEP: usize = 150;
const UNUSED: Duration = Duration::from_secs(60);

impl Thumbs {
    pub fn new(ctx: egui::Context) -> Self {
        let (job_tx, job_rx) = mpsc::channel::<(u64, PathBuf)>();
        let (done_tx, done_rx) = mpsc::channel::<Done>();
        let app_ctx = ctx.clone();
        // A few workers sharing one queue: thumbnails still arrive roughly
        // newest-first (the order they're requested). Each ffmpeg runs at low
        // priority and is mostly process start-up, so a capture isn't starved.
        let workers = std::thread::available_parallelism().map_or(2, |n| n.get() / 3).clamp(2, 4);
        let job_rx = Arc::new(Mutex::new(job_rx));
        for _ in 0..workers {
            let (job_rx, done_tx, ctx) = (job_rx.clone(), done_tx.clone(), ctx.clone());
            std::thread::spawn(move || {
                loop {
                    let Ok((key, path)) = job_rx.lock().unwrap().recv() else { break };
                    let (image, duration) = generate(key, &path);
                    if done_tx.send(Done { key, image, duration }).is_err() {
                        break;
                    }
                    ctx.request_repaint();
                }
            });
        }
        Self { ctx: app_ctx, ready: HashMap::new(), pending: HashSet::new(), job_tx, done_rx, swept: Instant::now(), durations: HashMap::new(), lengths_version: 0, smalls: HashMap::new() }
    }

    pub fn ctx(&self) -> egui::Context {
        self.ctx.clone()
    }

    /// Take in the pictures made since last time.
    fn receive(&mut self, ctx: &egui::Context) {
        while let Ok(done) = self.done_rx.try_recv() {
            self.pending.remove(&done.key);
            // The picture at a tiny size (RGBA, [`SMALL_W`] wide), for the
            // library panel's glass, which blurs it on the CPU.
            if let Some(img) = &done.image {
                self.smalls.insert(done.key, std::sync::Arc::new(shrink(img, SMALL_W)));
            }
            // A length newly known (looked up before or not): sorting by length sorts again.
            if done.duration.is_some() && self.durations.insert(done.key, done.duration) != Some(done.duration) {
                self.lengths_version += 1;
            }
            let texture = done.image.map(|img| {
                ctx.load_texture(format!("thumb-{}", done.key), img, egui::TextureOptions::LINEAR)
            });
            self.ready.insert(done.key, (Thumb { texture, duration: done.duration }, Instant::now()));
        }
    }

    /// The clip's preview, queuing generation on first request. `None` while loading.
    pub fn get(&mut self, ctx: &egui::Context, clip: &Clip) -> Option<&Thumb> {
        self.receive(ctx);
        let now = Instant::now();
        if self.ready.len() > KEEP && now.duration_since(self.swept) > Duration::from_secs(5) {
            self.swept = now;
            self.ready.retain(|_, (_, used)| now.duration_since(*used) < UNUSED);
        }
        let key = key(clip);
        if !self.ready.contains_key(&key) && self.pending.insert(key) {
            let _ = self.job_tx.send((key, clip.path.clone()));
        }
        let (thumb, used) = self.ready.get_mut(&key)?;
        *used = now;
        Some(thumb)
    }
}

impl Thumbs {
    /// The tiny picture of a clip, made once and kept; asked for if it
    /// hasn't been made (it turns up in a moment, without loading the big one
    /// onto the graphics card).
    pub fn small(&mut self, ctx: &egui::Context, clip: &Clip) -> Option<std::sync::Arc<(usize, usize, Vec<u8>)>> {
        self.receive(ctx);
        let key = key(clip);
        if let Some(s) = self.smalls.get(&key) {
            return Some(s.clone());
        }
        if !self.ready.contains_key(&key) && self.pending.insert(key) {
            let _ = self.job_tx.send((key, clip.path.clone()));
        }
        None
    }

    /// The clip's length, if its picture has been made (it's found then),
    /// without asking for one.
    pub fn duration(&mut self, clip: &Clip) -> Option<Duration> {
        let k = key(clip);
        if let Some((t, _)) = self.ready.get(&k) {
            return t.duration;
        }
        if let Some(d) = self.durations.get(&k) {
            return *d;
        }
        // Read once, found or not ([`Thumbs::lengths_version`] says when one
        // turns up later).
        let d = std::fs::read_to_string(cache_dir().join(format!("{k:016x}.dur"))).ok().and_then(|s| s.trim().parse::<f64>().ok()).map(Duration::from_secs_f64);
        self.durations.insert(k, d);
        d
    }

    /// Goes up whenever a clip's length becomes known (its picture was made).
    pub fn lengths_version(&self) -> u64 {
        self.lengths_version
    }
}

/// The cached thumbnail JPEG for `clip`, if it's been generated.
pub fn cached_jpeg(clip: &Clip) -> Option<PathBuf> {
    let p = jpeg_path(key(clip));
    p.exists().then_some(p)
}

/// Where a clip's thumbnail is kept. `-first`: taken from the first frame
/// (older ones, from a second in, are made again).
fn jpeg_path(key: u64) -> PathBuf {
    cache_dir().join(format!("{key:016x}-first.jpg"))
}

fn key(clip: &Clip) -> u64 {
    let mut h = DefaultHasher::new();
    clip.path.hash(&mut h);
    clip.modified.hash(&mut h);
    clip.created.hash(&mut h);
    clip.size_bytes.hash(&mut h);
    h.finish()
}

fn cache_dir() -> PathBuf {
    dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("hasteclip").join("thumbs")
}

fn generate(key: u64, video: &Path) -> (Option<egui::ColorImage>, Option<Duration>) {
    let jpg = jpeg_path(key);
    if crate::clips::is_image(video) {
        if !jpg.exists() {
            let _ = std::fs::create_dir_all(cache_dir());
            if let Err(e) = shrink_picture(video, &jpg) {
                eprintln!("couldn't make a thumbnail of {}: {e}", video.display());
            }
        }
        return (load(&jpg), None);
    }
    let dur_file = cache_dir().join(format!("{key:016x}.dur"));
    let cached = std::fs::read_to_string(&dur_file).ok().and_then(|s| s.trim().parse::<f64>().ok());
    let duration = match cached {
        Some(secs) => Some(Duration::from_secs_f64(secs)),
        None => {
            let d = probe_duration(video);
            if let Some(d) = d {
                let _ = std::fs::create_dir_all(cache_dir());
                let _ = std::fs::write(&dur_file, d.as_secs_f64().to_string());
            }
            d
        }
    };
    if !jpg.exists() {
        let _ = std::fs::create_dir_all(cache_dir());
        // The first frame: what you see when the clip starts playing. Kept
        // as it's decoded (no reading the JPEG back).
        if let Some((w, h, rgba)) = first_frame(video) {
            if let Some(bytes) = media::encode_jpeg(&rgba, w as u16, h as u16, 85) {
                let _ = std::fs::write(&jpg, bytes);
            }
            return (Some(egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba)), duration);
        }
        let _ = media::ffmpeg_background()
            .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
            .arg(video)
            .args(["-frames:v", "1", "-vf", &format!("scale={WIDTH}:-2"), "-q:v", "4"])
            .arg(&jpg)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    (load(&jpg), duration)
}

/// A clip's first frame, [`WIDTH`] wide, as (width, height, RGBA), by the
/// decoder playback uses: ~30 ms where ffmpeg took ~150 ms (and twice the
/// CPU time) to start and write it. `None` where that can't open the clip.
fn first_frame(video: &Path) -> Option<(usize, usize, Vec<u8>)> {
    let mut dec = capture::decode::Decoder::open_rgba(video, WIDTH).ok()?;
    let p = dec.frame(0).ok()??;
    (p.rgba.len() == p.width as usize * p.height as usize * 4 && p.width > 0).then(|| (p.width as usize, p.height as usize, p.rgba))
}

/// A picture `w` wide (box-averaged), as (width, height, RGBA).
fn shrink(img: &egui::ColorImage, w: usize) -> (usize, usize, Vec<u8>) {
    let [sw, sh] = img.size;
    let h = ((sh * w) / sw.max(1)).max(1);
    let mut out = vec![0u8; w * h * 4];
    for y in 0..h {
        let (y0, y1) = (y * sh / h, ((y + 1) * sh / h).max(y * sh / h + 1));
        for x in 0..w {
            let (x0, x1) = (x * sw / w, ((x + 1) * sw / w).max(x * sw / w + 1));
            let mut acc = [0u32; 4];
            for yy in y0..y1.min(sh) {
                for xx in x0..x1.min(sw) {
                    let c = img.pixels[yy * sw + xx];
                    acc[0] += c.r() as u32;
                    acc[1] += c.g() as u32;
                    acc[2] += c.b() as u32;
                    acc[3] += c.a() as u32;
                }
            }
            let n = ((y1.min(sh) - y0) * (x1.min(sw) - x0)).max(1) as u32;
            let o = (y * w + x) * 4;
            for k in 0..4 {
                out[o + k] = (acc[k] / n) as u8;
            }
        }
    }
    (w, h, out)
}

/// A screenshot's thumbnail: [`WIDTH`] wide, as a JPEG at `to`.
fn shrink_picture(from: &Path, to: &Path) -> Result<(), String> {
    let img = image::open(from).map_err(|e| e.to_string())?.to_rgb8();
    let h = ((img.height() as u64 * WIDTH as u64 / img.width().max(1) as u64) as u32).max(1);
    let small = image::imageops::resize(&img, WIDTH, h, image::imageops::FilterType::Triangle);
    let file = std::fs::File::create(to).map_err(|e| e.to_string())?;
    image::codecs::jpeg::JpegEncoder::new_with_quality(std::io::BufWriter::new(file), 85).encode_image(&small).map_err(|e| e.to_string())
}

fn load(jpg: &Path) -> Option<egui::ColorImage> {
    let img = image::open(jpg).ok()?.to_rgba8();
    let size = [img.width() as usize, img.height() as usize];
    Some(egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw()))
}

fn probe_duration(video: &Path) -> Option<Duration> {
    let secs = media::probe(video).ok()?.duration;
    (secs.is_finite() && secs >= 0.0).then(|| Duration::from_secs_f64(secs))
}

/// "0:42", "12:05", "1:02:33".
pub fn format_duration(d: Duration) -> String {
    let s = d.as_secs_f64().round() as u64;
    let (h, m, s) = (s / 3600, (s / 60) % 60, s % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

#[cfg(test)]
mod first_frame_tests {
    use super::*;

    /// The first frame decoded here is ffmpeg's, scaled down the same way
    /// (each pixel the average of the area it covers), in a fraction of the
    /// time ffmpeg takes to make a thumbnail (its own, bicubic).
    #[test]
    #[ignore = "needs a clip: HASTECLIP_CLIP=<mp4> cargo test --release -p hasteclip first_frame -- --ignored --nocapture"]
    fn first_frame_is_ffmpegs() {
        let clip = PathBuf::from(std::env::var("HASTECLIP_CLIP").unwrap());
        let t = Instant::now();
        let (w, h, ours) = first_frame(&clip).expect("decoded");
        let ours_ms = t.elapsed().as_secs_f64() * 1000.0;
        let t = Instant::now();
        let thumbnail = std::env::temp_dir().join(format!("hc-first-frame-{}.jpg", std::process::id()));
        media::ffmpeg()
            .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
            .arg(&clip)
            .args(["-frames:v", "1", "-vf", &format!("scale={WIDTH}:-2"), "-q:v", "4"])
            .arg(&thumbnail)
            .output()
            .unwrap();
        let theirs_ms = t.elapsed().as_secs_f64() * 1000.0;
        let _ = std::fs::remove_file(&thumbnail);
        // With the colour matrix playback uses (an untagged HD clip is
        // BT.709, as players take it; ffmpeg would take BT.601).
        let matrix = if media::probe(&clip).unwrap().bt601 { "bt601" } else { "bt709" };
        let out = media::ffmpeg()
            .args(["-hide_banner", "-loglevel", "error", "-i"])
            .arg(&clip)
            .args(["-frames:v", "1", "-vf", &format!("scale={WIDTH}:-2:flags=area:in_color_matrix={matrix}"), "-f", "rawvideo", "-pix_fmt", "rgba", "-"])
            .output()
            .unwrap();
        assert_eq!(out.stdout.len(), w * h * 4, "the same size ({w}x{h})");
        let mean = ours.iter().zip(&out.stdout).map(|(a, b)| a.abs_diff(*b) as u64).sum::<u64>() as f64 / ours.len() as f64;
        eprintln!("{w}x{h}: {ours_ms:.1} ms here, {theirs_ms:.1} ms by ffmpeg; off by {mean:.2} on average");
        assert!(mean < 2.0, "off by {mean:.2}");
    }
}
