//! Clip preview images.
//!
//! A frame is grabbed with ffmpeg on a few background workers, cached as a JPEG
//! (and the clip's duration beside it) under the OS cache dir, keyed by path +
//! mtime + size so a replaced file gets a fresh thumbnail. A cached clip starts
//! no process at all. Each picture is uploaded to the GPU once and reused.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::clips::Clip;

/// Thumbnail width in pixels; cards are displayed at roughly half this, so they
/// stay crisp on Retina displays.
const WIDTH: u32 = 480;

pub struct Thumb {
    pub texture: Option<egui::TextureHandle>,
    pub duration: Option<Duration>,
}

struct Done {
    key: u64,
    image: Option<egui::ColorImage>,
    duration: Option<Duration>,
}

pub struct Thumbs {
    ctx: egui::Context,
    ready: HashMap<u64, Thumb>,
    pending: HashSet<u64>,
    job_tx: Sender<(u64, PathBuf)>,
    done_rx: Receiver<Done>,
}

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
        Self { ctx: app_ctx, ready: HashMap::new(), pending: HashSet::new(), job_tx, done_rx }
    }

    pub fn ctx(&self) -> egui::Context {
        self.ctx.clone()
    }

    /// The clip's preview, queuing generation on first request. `None` while loading.
    pub fn get(&mut self, ctx: &egui::Context, clip: &Clip) -> Option<&Thumb> {
        while let Ok(done) = self.done_rx.try_recv() {
            self.pending.remove(&done.key);
            let texture = done.image.map(|img| {
                ctx.load_texture(format!("thumb-{}", done.key), img, egui::TextureOptions::LINEAR)
            });
            self.ready.insert(done.key, Thumb { texture, duration: done.duration });
        }
        let key = key(clip);
        if !self.ready.contains_key(&key) && self.pending.insert(key) {
            let _ = self.job_tx.send((key, clip.path.clone()));
        }
        self.ready.get(&key)
    }
}

/// Duration in seconds of a clip whose thumbnail has been made (probed then).
impl Thumbs {
}

/// The cached thumbnail JPEG for `clip`, if it's been generated.
pub fn cached_jpeg(clip: &Clip) -> Option<PathBuf> {
    let p = cache_dir().join(format!("{:016x}.jpg", key(clip)));
    p.exists().then_some(p)
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
    dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("hesteclips").join("thumbs")
}

fn generate(key: u64, video: &Path) -> (Option<egui::ColorImage>, Option<Duration>) {
    let jpg = cache_dir().join(format!("{key:016x}.jpg"));
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
        // Skip a little way in: the very first frame of a capture is often black or a
        // half-drawn window. Short clips grab from the middle instead.
        let at = duration.map_or(0.0, |d| (d.as_secs_f64() * 0.5).min(1.0));
        let _ = media::ffmpeg_background()
            .args(["-hide_banner", "-loglevel", "error", "-y", "-ss", &format!("{at:.2}"), "-i"])
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

fn load(jpg: &Path) -> Option<egui::ColorImage> {
    let img = image::open(jpg).ok()?.to_rgba8();
    let size = [img.width() as usize, img.height() as usize];
    Some(egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw()))
}

fn probe_duration(video: &Path) -> Option<Duration> {
    let out = media::ffprobe()
        .args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"])
        .arg(video)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    let secs: f64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    Some(Duration::from_secs_f64(secs))
}

/// "0:42", "12:05", "1:02:33".
pub fn format_duration(d: Duration) -> String {
    let s = d.as_secs_f64().round() as u64;
    let (h, m, s) = (s / 3600, (s / 60) % 60, s % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}
