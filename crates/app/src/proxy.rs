//! Scrub proxy: every frame of the clip as a small JPEG, in a file.
//!
//! Only where there's no hardware decoder to scrub with (`hw_decode`: Linux,
//! or a file it can't read): with one, scrubbing decodes live (`video.rs`),
//! keyframes while the playhead moves fast and every frame when it's slow,
//! with nothing built beforehand.
//!
//! Seeking the real file through ffmpeg costs 50-200 ms a frame, far too
//! slow to follow a drag. One pass decodes every frame at a small size; after that any frame
//! is a JPEG decode away, so scrubbing updates the picture on every mouse
//! move. Frames arrive in order while it builds, so the start of the clip is
//! scrubbable almost immediately.
//!
//! On Windows and macOS the pass uses the GPU's video decoder
//! (`capture::decode`) and the CPU only compresses the small frames, on low-priority threads, at
//! full speed even while the clip plays (playback still gets every frame:
//! tested; slowing down left most of a long clip unscrubbable for a minute). It used
//! to be an ffmpeg process decoding every full-size frame in software on every
//! core, which is still the fallback (and elsewhere).
//!
//! A clip just saved gets its proxy built in the background ([`prebuild`]),
//! gently (two compressing threads, paused while any clip is open), and every
//! finished proxy is kept on disk (`cache`), so a clip opens fully scrubbable.
//! A player opening a clip whose build is under way joins it at full speed:
//! one build per clip, never two decoders on it (they slow each other down).
//!
//! The frames go straight into their file as they're made ([`Data`]); only
//! where each one is stays in memory. Held in memory they took 0.2-1 GB per
//! open clip (a 5-minute clip at 640 px: 914 MB). Reads come from Windows'
//! file cache, which gives the memory back when something else needs it, and
//! a kept proxy opens without being read in at all.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

/// JPEG quality of proxy frames. From 90 up the encoder keeps colour at full
/// resolution (below, at half: greys shifted, a 2E3238 came back 2E313A),
/// and blocking is gone: mean error a third of a level at 1280 px.
const QUALITY: u8 = 90;

/// Proxy frame width: as sharp as fits in roughly 500 MB for the clip (a
/// 1280 px frame is ~70 KB at quality 90): up to a minute of 60 fps at 1280,
/// up to 2.5 minutes at 960, longer at 640.
fn width_for(total: usize) -> u32 {
    match total {
        0..=4_000 => 1280,
        4_001..=9_000 => 960,
        _ => 640,
    }
}

/// Where one JPEG is in the proxy's file; every slot it stands for points at
/// it (a dropped frame's slot shows the one before it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Spot {
    at: u64,
    len: u32,
}

/// The proxy frames by frame number, filled in any order (see `hw`).
#[derive(Default)]
struct Store {
    frames: Vec<Option<Spot>>,
    /// How many slots are filled.
    ready: usize,
}

impl Store {
    fn set(&mut self, i: usize, spot: Spot) {
        if i >= self.frames.len() {
            self.frames.resize(i + 1, None);
        }
        if self.frames[i].is_none() {
            self.ready += 1;
        }
        self.frames[i] = Some(spot);
    }

    /// The next slot in order (the ffmpeg pass fills them front to back).
    fn push(&mut self, spot: Spot) {
        let i = self.frames.len();
        self.set(i, spot);
    }
}

/// A proxy's file: JPEGs one after another, appended as they're made and
/// readable at once; once complete, the slot index and a footer follow, and
/// it's renamed to the clip's cache file (see `cache`). One being built is
/// deleted when it's dropped unfinished.
struct Data {
    /// Writing: the file and its length. `None` once finished, or for a
    /// proxy opened from the cache.
    write: Mutex<Option<(std::fs::File, u64)>>,
    read: std::fs::File,
    path: Mutex<PathBuf>,
    /// Delete the file when dropped (not finished and kept).
    discard: AtomicBool,
}

impl Data {
    /// A new, empty file at `path`.
    fn create(path: PathBuf) -> std::io::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let write = std::fs::File::create(&path)?;
        let read = std::fs::File::open(&path)?;
        Ok(Self { write: Mutex::new(Some((write, 0))), read, path: Mutex::new(path), discard: AtomicBool::new(true) })
    }

    /// Add a JPEG; where it went.
    fn append(&self, jpeg: &[u8]) -> std::io::Result<Spot> {
        let mut w = self.write.lock().unwrap();
        let (file, len) = w.as_mut().ok_or_else(|| std::io::Error::other("the proxy is finished"))?;
        // Unbuffered: a slot is filled right after this, and readers read it.
        file.write_all(jpeg)?;
        let spot = Spot { at: *len, len: jpeg.len() as u32 };
        *len += jpeg.len() as u64;
        Ok(spot)
    }

    /// Bytes of JPEGs written so far.
    fn bytes(&self) -> u64 {
        self.write.lock().unwrap().as_ref().map_or(0, |w| w.1)
    }

    fn read(&self, spot: Spot) -> Option<Vec<u8>> {
        let mut buf = vec![0u8; spot.len as usize];
        read_at(&self.read, &mut buf, spot.at).ok()?;
        Some(buf)
    }
}

impl Drop for Data {
    fn drop(&mut self) {
        if self.discard.load(Ordering::Relaxed) {
            // Closed first: Windows deletes a file only once nothing has it open.
            drop(self.write.lock().unwrap().take());
            let _ = std::fs::remove_file(&*self.path.lock().unwrap());
        }
    }
}

/// Fill `buf` from `file` at `offset`, without moving any shared position.
fn read_at(file: &std::fs::File, mut buf: &mut [u8], mut offset: u64) -> std::io::Result<()> {
    while !buf.is_empty() {
        #[cfg(windows)]
        let n = std::os::windows::fs::FileExt::seek_read(file, buf, offset)?;
        #[cfg(unix)]
        let n = std::os::unix::fs::FileExt::read_at(file, buf, offset)?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        buf = &mut buf[n..];
        offset += n as u64;
    }
    Ok(())
}

/// One clip's proxy, being built or built: shared by every player showing
/// the clip and by the background builder.
struct Build {
    source: PathBuf,
    fps: f64,
    total: usize,
    frames: Mutex<Store>,
    /// The frame the player is at: the build works outward from here.
    focus: AtomicU64,
    stop: AtomicBool,
    /// Players showing it. With none it's a background build: gentle.
    watchers: AtomicUsize,
    /// Keep building with nobody watching (a clip just saved).
    background: AtomicBool,
    /// Every frame built, so it can be kept.
    complete: AtomicBool,
    /// The pass ended (complete, stopped or failed).
    done: AtomicBool,
    /// The ffmpeg pass, when that's the way it's built.
    child: Mutex<Option<Child>>,
    /// Where it's kept once complete; `None` if the file can't be keyed.
    cache: Option<PathBuf>,
    /// The frames' file, once the build has started.
    data: std::sync::OnceLock<Data>,
}

impl Build {
    fn watched(&self) -> bool {
        self.watchers.load(Ordering::Relaxed) > 0
    }

    /// Nobody wants it any more: stop building.
    fn stop_if_unwanted(&self) {
        if self.watched() || self.background.load(Ordering::Relaxed) {
            return;
        }
        self.stop.store(true, Ordering::Relaxed);
        if let Some(mut c) = self.child.lock().unwrap().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    /// Keep a JPEG made for slot(s) to come; where it went (`None`: it
    /// couldn't be written, so it's left out).
    fn write(&self, jpeg: &[u8]) -> Option<Spot> {
        self.data.get()?.append(jpeg).inspect_err(|e| eprintln!("scrub proxy: couldn't write a frame: {e}")).ok()
    }

    /// The pass ended: keep the proxy if every frame is in.
    fn finish(&self) {
        let complete = self.complete.load(Ordering::Relaxed) && !self.stop.load(Ordering::Relaxed);
        if let (true, Some(path), Some(data)) = (complete, &self.cache, self.data.get()) {
            let t = std::time::Instant::now();
            // A copy of the list, so the lock isn't held while it's written:
            // the player reads it every frame.
            let frames = self.frames.lock().unwrap().frames.clone();
            match cache::finish(data, &frames, path) {
                Err(e) => eprintln!("scrub proxy: couldn't keep it: {e}"),
                Ok(()) if std::env::var_os("HASTECLIP_DEBUG_VIDEO").is_some() => {
                    eprintln!("{:>8.3} scrub proxy: kept on disk in {:.0} ms", crate::player::uptime(), t.elapsed().as_secs_f64() * 1000.0);
                }
                Ok(()) => {}
            }
        }
        self.done.store(true, Ordering::Relaxed);
    }
}

/// Builds under way or in use, by cache key (one per clip).
static BUILDS: Mutex<Option<HashMap<u64, Weak<Build>>>> = Mutex::new(None);

/// Stop building `source`'s preview (it's about to be deleted: its decoder
/// has the file open).
pub fn release(source: &Path) {
    let builds = BUILDS.lock().unwrap();
    for b in builds.iter().flat_map(|m| m.values()).filter_map(Weak::upgrade).filter(|b| b.source == source) {
        b.stop.store(true, Ordering::Relaxed);
        if let Some(mut c) = b.child.lock().unwrap().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// While anyone watches a build, background builds wait (the decoder is
/// theirs, and the CPU mostly is).
#[cfg(hw_decode)]
fn anyone_watching() -> bool {
    let builds = BUILDS.lock().unwrap();
    builds.as_ref().is_some_and(|m| m.values().filter_map(Weak::upgrade).any(|b| b.watched() && !b.done.load(Ordering::Relaxed)))
}

/// The clip's build: the one under way, else a new one (from the cache if
/// it's kept there).
fn shared_build(ctx: &egui::Context, source: &Path, fps: f64, total: usize, background: bool) -> Arc<Build> {
    let key = cache::key(source, fps, total);
    let mut builds = BUILDS.lock().unwrap();
    let map = builds.get_or_insert_with(HashMap::new);
    map.retain(|_, b| b.strong_count() > 0);
    if let Some(b) = key.and_then(|k| map.get(&k)).and_then(Weak::upgrade).filter(|b| !b.stop.load(Ordering::Relaxed)) {
        if background {
            b.background.store(true, Ordering::Relaxed);
        }
        return b;
    }
    let b = Arc::new(Build {
        source: source.to_path_buf(),
        fps,
        total,
        frames: Mutex::new(Store::default()),
        focus: AtomicU64::new(0),
        stop: AtomicBool::new(false),
        watchers: AtomicUsize::new(0),
        background: AtomicBool::new(background),
        complete: AtomicBool::new(false),
        done: AtomicBool::new(false),
        child: Mutex::new(None),
        cache: key.map(cache::path),
        data: Default::default(),
    });
    if let Some(k) = key {
        map.insert(k, Arc::downgrade(&b));
    }
    drop(builds);
    let (ctx, b2) = (ctx.clone(), b.clone());
    // Opening a decoder takes a few hundred ms: not on the UI thread.
    std::thread::spawn(move || start(&ctx, &b2));
    b
}

fn start(ctx: &egui::Context, b: &Arc<Build>) {
    if let Some(path) = &b.cache {
        let t = std::time::Instant::now();
        if let Some((data, store)) = cache::load(path, b.total) {
            if std::env::var_os("HASTECLIP_DEBUG_VIDEO").is_some() {
                eprintln!("{:>8.3} scrub proxy: {} frames from the cache in {:.0} ms", crate::player::uptime(), store.ready, t.elapsed().as_secs_f64() * 1000.0);
            }
            let _ = b.data.set(data);
            *b.frames.lock().unwrap() = store;
            b.done.store(true, Ordering::Relaxed);
            ctx.request_repaint();
            return;
        }
    }
    // Where the frames go while it's built: beside the kept ones (it becomes
    // one when complete), else in the temporary folder.
    let made = Data::create(cache::partial_path(b.cache.as_deref())).or_else(|e| {
        eprintln!("scrub proxy: can't write beside the kept ones ({e}); using the temporary folder");
        Data::create(cache::partial_path(None))
    });
    match made {
        Ok(d) => {
            let _ = b.data.set(d);
        }
        Err(e) => {
            eprintln!("scrub proxy: nowhere to keep frames: {e}");
            b.done.store(true, Ordering::Relaxed);
            return;
        }
    }
    // The sound first: building reads the whole clip too, and on a hard drive
    // the two made each other several times slower. After it, the clip is in
    // memory and building is quick.
    while media::pcm::busy() && !b.stop.load(Ordering::Relaxed) {
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    #[cfg(hw_decode)]
    if std::env::var_os("HASTECLIP_NO_HW_DECODE").is_none() && hw::build(ctx, b) {
        return;
    }
    let mut slot = b.child.lock().unwrap();
    *slot = start_ffmpeg(ctx, b);
    if slot.is_none() {
        b.done.store(true, Ordering::Relaxed);
    }
    // Closed while it started: the drop already ran, so stop it here.
    if b.stop.load(Ordering::Relaxed) {
        if let Some(mut c) = slot.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// Build `source`'s proxy in the background and keep it, unless it's kept
/// already: one clip at a time, in the order asked. Nothing where the
/// hardware decoder scrubs.
pub fn prebuild(ctx: &egui::Context, source: &Path) {
    if cfg!(hw_decode) {
        return;
    }
    static QUEUE: std::sync::OnceLock<Mutex<std::sync::mpsc::Sender<PathBuf>>> = std::sync::OnceLock::new();
    let queue = QUEUE.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<PathBuf>();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            for source in rx {
                // The same numbers a player opening it uses.
                let Ok(info) = media::probe(&source) else { continue };
                let total = (info.duration * info.fps).floor() as usize;
                if cache::key(&source, info.fps, total).is_some_and(|k| cache::path(k).exists()) {
                    continue;
                }
                let b = shared_build(&ctx, &source, info.fps, total, true);
                while !b.done.load(Ordering::Relaxed) && !b.stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(250));
                }
                b.background.store(false, Ordering::Relaxed);
                b.stop_if_unwanted();
            }
        });
        Mutex::new(tx)
    });
    let _ = queue.lock().unwrap().send(source.to_path_buf());
}

/// Where the hardware decoder scrubs, proxies kept from before it did (up to
/// 5 GB) are of no more use: delete them, in the background.
pub fn forget_kept() {
    if cfg!(hw_decode) {
        std::thread::spawn(|| {
            let _ = std::fs::remove_dir_all(cache::dir());
        });
    }
}

pub struct Proxy {
    build: Arc<Build>,
}

impl Proxy {
    /// Every one of the `total` frames of `source`, numbered at `fps`.
    pub fn build(ctx: &egui::Context, source: &Path, fps: f64, total: usize) -> Self {
        let build = shared_build(ctx, source, fps, total, false);
        build.watchers.fetch_add(1, Ordering::Relaxed);
        Self { build }
    }

    /// How many frames are ready.
    pub fn ready(&self) -> usize {
        self.build.frames.lock().map_or(0, |f| f.ready)
    }

    /// Which of `n` evenly spaced frames from `first` to `last` are built.
    pub fn built(&self, first: u64, last: u64, n: usize) -> Vec<bool> {
        let Ok(f) = self.build.frames.lock() else { return vec![false; n] };
        (0..n)
            .map(|k| {
                let i = first + (last.saturating_sub(first) as f64 * (k as f64 + 0.5) / n.max(1) as f64) as u64;
                f.frames.get(i as usize).is_some_and(Option::is_some)
            })
            .collect()
    }

    /// The player is at frame `idx`: build around here next.
    pub fn set_focus(&self, idx: u64) {
        self.build.focus.store(idx, Ordering::Relaxed);
    }

    /// The built frame nearest `idx`, at most `within` frames away (ties go
    /// to the earlier one): a stand-in while `idx` itself isn't built yet.
    pub fn nearest(&self, idx: u64, within: u64) -> Option<u64> {
        let f = self.build.frames.lock().ok()?;
        let built = |i: u64| f.frames.get(i as usize).is_some_and(Option::is_some);
        (0..=within).find_map(|d| {
            if idx >= d && built(idx - d) {
                Some(idx - d)
            } else if built(idx + d) {
                Some(idx + d)
            } else {
                None
            }
        })
    }

    /// Decode proxy frame `idx`, if it's been built yet.
    pub fn frame(&self, idx: u64) -> Option<egui::ColorImage> {
        let spot = (*self.build.frames.lock().ok()?.frames.get(idx as usize)?)?;
        let jpeg = self.build.data.get()?.read(spot)?;
        // JPEGs decode to RGB: taken as it is (no alpha to add, nothing to premultiply).
        let img = image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg).ok()?.into_rgb8();
        Some(egui::ColorImage::from_rgb([img.width() as usize, img.height() as usize], img.as_raw()))
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.build.watchers.fetch_sub(1, Ordering::Relaxed);
        self.build.stop_if_unwanted();
    }
}

/// The ffmpeg pass: every frame decoded in software, as an MJPEG stream.
fn start_ffmpeg(ctx: &egui::Context, b: &Arc<Build>) -> Option<Child> {
    let mut child = media::ffmpeg_background()
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(&b.source)
        // fps= pins one output frame per source frame index, even for VFR input.
        .args(["-an", "-vf", &format!("fps={},scale={}:-2:flags=lanczos,format=yuvj444p", b.fps, width_for(b.total))])
        .args(["-c:v", "mjpeg", "-q:v", "2", "-f", "image2pipe", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    let (b, ctx) = (b.clone(), ctx.clone());
    std::thread::spawn(move || {
        split_jpegs(stdout, &ctx, |jpeg| match b.write(jpeg) {
            Some(spot) => {
                b.frames.lock().unwrap().push(spot);
                true
            }
            // Slots are filled in order: one missing would shift every later one.
            None => false,
        });
        let ready = b.frames.lock().map_or(0, |f| f.ready);
        b.complete.store(ready >= b.total, Ordering::Relaxed);
        b.finish();
    });
    Some(child)
}

/// Split an MJPEG byte stream into individual JPEGs (each SOI 0xFFD8 … EOI 0xFFD9).
///
/// ffmpeg's mjpeg encoder byte-stuffs 0xFF inside entropy-coded data, so the first
/// EOI after a frame's start ends that frame. Each goes to `put`, which says
/// whether to go on.
fn split_jpegs(mut r: impl Read, ctx: &egui::Context, mut put: impl FnMut(&[u8]) -> bool) {
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
                    if !put(&buf[..end]) {
                        ctx.request_repaint();
                        return;
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
#[cfg(hw_decode)]
mod hw {
    use super::*;
    use std::sync::mpsc;

    use capture::decode::{Decoder, Picture};


    /// Threads compressing frames (the slow part, 6-10 ms a frame at quality
    /// 90): every core but two, at below-normal priority, so a game or the
    /// app still comes first.
    fn encoders() -> usize {
        std::thread::available_parallelism().map_or(4, |n| n.get().saturating_sub(2)).clamp(2, 16)
    }

    /// Start building; false if the decoder can't open the clip.
    pub fn build(ctx: &egui::Context, b: &Arc<Build>) -> bool {
        let (source, fps, total) = (&b.source, b.fps, b.total);
        let debug = std::env::var_os("HASTECLIP_DEBUG_VIDEO").is_some();
        if debug {
            eprintln!("{:>8.3} scrub proxy: opening the decoder", crate::player::uptime());
        }
        // Open here, so a clip the decoder can't read falls back to ffmpeg.
        let mut dec = match Decoder::open_rgba(source, width_for(total)) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("scrub proxy: hardware decoder unavailable, using ffmpeg: {e:#}");
                return false;
            }
        };
        dec.set_fps(fps);
        if debug {
            eprintln!("{:>8.3} scrub proxy: decoder open", crate::player::uptime());
        }
        // Decoding on one thread, compressing on a few (it's the slower part),
        // then put back in order on another.
        let encoders = encoders();
        let (tx, jobs) = mpsc::sync_channel::<(u64, Job)>(encoders * 2);
        let bd = b.clone();
        std::thread::spawn(move || {
            let (stop_d, focus) = (&bd.stop, &bd.focus);
            crate::video::lower_priority();
            let chunks = total.div_ceil(CHUNK);
            let mut built = vec![false; chunks];
            let mut seq = 0u64;
            let mut send = |job: Job| {
                let ok = tx.send((seq, job)).is_ok();
                seq += 1;
                ok
            };
            // A frame read past the end of a chunk: the next chunk's first,
            // if that's the one done next (then no seek is needed).
            let mut carry: Option<Picture> = None;
            'chunks: while !stop_d.load(Ordering::Relaxed) {
                let at = (focus.load(Ordering::Relaxed) as usize).min(total.saturating_sub(1)) / CHUNK;
                let Some(c) = next_chunk(&built, at) else {
                    bd.complete.store(true, Ordering::Relaxed);
                    break;
                };
                let (start, end) = (c * CHUNK, ((c + 1) * CHUNK).min(total));
                if debug {
                    eprintln!("{:>8.3} scrub proxy: building frames {start}..{end}", crate::player::uptime());
                }
                let asked = std::time::Instant::now();
                let first = match carry.take() {
                    Some(p) if (start..end).contains(&(p.index as usize)) => Ok(Some(p)),
                    _ => dec.frame(start as u64),
                };
                if debug {
                    eprintln!("{:>8.3} scrub proxy: first frame of {start} decoded in {:.0} ms", crate::player::uptime(), asked.elapsed().as_secs_f64() * 1000.0);
                }
                let mut got = first;
                let mut n = 0u32;
                loop {
                    crate::video::yield_to_urgent();
                    // In the background: wait while any clip is open.
                    if n % 16 == 0 && !bd.watched() {
                        while anyone_watching() && !stop_d.load(Ordering::Relaxed) {
                            std::thread::sleep(Duration::from_millis(100));
                        }
                    }
                    n += 1;
                    match got {
                        Ok(Some(p)) if (p.index as usize) >= end => {
                            carry = Some(p);
                            break;
                        }
                        Ok(Some(p)) => {
                            if !send(Job::Frame { start, end, pic: p }) {
                                break 'chunks;
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            eprintln!("scrub proxy: {e:#}");
                            break 'chunks;
                        }
                    }
                    if stop_d.load(Ordering::Relaxed) {
                        break 'chunks;
                    }
                    // Scrubbed off into a part not built yet: go there now,
                    // leaving this chunk for later. The frame just sent stays:
                    // a chunk's first is a keyframe (quick to decode), so a
                    // fast scrub still leaves one every chunk or so to show.
                    let now_at = (focus.load(Ordering::Relaxed) as usize).min(total.saturating_sub(1)) / CHUNK;
                    if now_at != c && !built[now_at] {
                        if debug {
                            eprintln!("{:>8.3} scrub proxy: left frames {start}..{end} for {}", crate::player::uptime(), now_at * CHUNK);
                        }
                        carry = None;
                        continue 'chunks;
                    }
                    got = dec.next();
                }
                built[c] = true;
                if !send(Job::End { start, end }) {
                    break;
                }
            }
        });
        let jobs = Arc::new(Mutex::new(jobs));
        let (done_tx, done) = mpsc::channel::<(u64, Done)>();
        for k in 0..encoders {
            let (jobs, done_tx, b) = (jobs.clone(), done_tx.clone(), b.clone());
            std::thread::spawn(move || {
                crate::video::lower_priority();
                loop {
                    // In the background only two compress.
                    while k >= BACKGROUND_ENCODERS && !b.watched() && !b.stop.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    let job = jobs.lock().unwrap().recv();
                    let Ok((seq, job)) = job else { return };
                    let out = match job {
                        Job::Frame { start, end, pic } => Done::Frame { start, end, index: pic.index as usize, jpeg: encode(&pic) },
                        Job::End { start, end } => Done::End { start, end },
                    };
                    if done_tx.send((seq, out)).is_err() {
                        return;
                    }
                }
            });
        }
        drop(done_tx);
        let (b, ctx) = (b.clone(), ctx.clone());
        let started = std::time::Instant::now();
        std::thread::spawn(move || {
            let (frames, stop) = (&b.frames, &b.stop);
            crate::video::lower_priority();
            let mut last_repaint = std::time::Instant::now();
            let mut waiting = std::collections::BTreeMap::new();
            let mut next_seq = 0u64;
            let mut chunk: Option<Chunk> = None;
            let mut first = true;
            for (seq, out) in done {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                let mut urgent_repaint = false;
                waiting.insert(seq, out);
                while let Some(out) = waiting.remove(&next_seq) {
                    next_seq += 1;
                    let (start, end) = match &out {
                        Done::Frame { start, end, .. } | Done::End { start, end } => (*start, *end),
                    };
                    let c = chunk.get_or_insert_with(|| Chunk::new(start, end));
                    if c.start != start {
                        *c = Chunk::new(start, end);
                    }
                    match out {
                        Done::Frame { index, jpeg: Some(jpeg), .. } => {
                            // Into the file first (not holding the slots: the
                            // player reads them every frame).
                            let Some(spot) = b.write(&jpeg) else { continue };
                            // A chunk's first frame: often all a scrub gets
                            // from it, so show it now.
                            if index == start {
                                urgent_repaint = true;
                            }
                            if debug && first {
                                eprintln!("{:>8.3} scrub proxy: first frame in", crate::player::uptime());
                                first = false;
                            }
                            c.frame(&mut frames.lock().unwrap(), index, spot);
                        }
                        Done::Frame { jpeg: None, .. } => {}
                        Done::End { .. } => c.end(&mut frames.lock().unwrap()),
                    }
                }
                // Otherwise at most every 100 ms, but always within it (a
                // frame arriving while the mouse rests still gets shown).
                if !b.watched() {
                } else if urgent_repaint || last_repaint.elapsed().as_millis() > 100 {
                    ctx.request_repaint();
                    last_repaint = std::time::Instant::now();
                } else {
                    ctx.request_repaint_after(std::time::Duration::from_millis(100));
                }
            }
            if debug {
                let bytes = b.data.get().map_or(0, Data::bytes);
                eprintln!("scrub proxy: {} frames in {:.1} s, {} MB", frames.lock().unwrap().ready, started.elapsed().as_secs_f64(), bytes / 1_000_000);
            }
            b.finish();
            ctx.request_repaint();
        });
        true
    }

    /// Compressing threads of a build nobody's watching (a clip just saved,
    /// while you may be playing): a game keeps the rest of the CPU.
    const BACKGROUND_ENCODERS: usize = 2;

    /// Frames per chunk: built one at a time, nearest the player first; one
    /// left half-built when the player moves to another not built yet.
    /// 2 s at 60 fps (starting on a keyframe, as clips are recorded: one a
    /// second, at most 120 frames apart).
    const CHUNK: usize = 120;

    enum Job {
        Frame { start: usize, end: usize, pic: Picture },
        /// The chunk's last frame was sent: fill its remaining slots.
        End { start: usize, end: usize },
    }

    enum Done {
        Frame { start: usize, end: usize, index: usize, jpeg: Option<Vec<u8>> },
        End { start: usize, end: usize },
    }

    /// The chunk to build next: the one the player is in, then the ones
    /// ahead of it, then behind (a chunk behind counts as three ahead).
    pub(super) fn next_chunk(built: &[bool], at: usize) -> Option<usize> {
        (0..built.len()).filter(|&c| !built[c]).min_by_key(|&c| if c >= at { (c - at, 0) } else { ((at - c) * 3, 1) })
    }

    /// Fills a chunk's slots as its frames come in, in order: slot `i` shows
    /// the last frame at or before `i` (a dropped frame's slots show the frame
    /// before; two frames on one slot keep the later).
    pub(super) struct Chunk {
        start: usize,
        end: usize,
        /// The first slot not filled yet.
        next: usize,
        last: Option<Spot>,
    }

    impl Chunk {
        pub(super) fn new(start: usize, end: usize) -> Self {
            Self { start, end, next: start, last: None }
        }

        pub(super) fn frame(&mut self, store: &mut Store, index: usize, spot: Spot) {
            while self.next < index.min(self.end) {
                store.set(self.next, self.last.unwrap_or(spot));
                self.next += 1;
            }
            if (self.start..self.end).contains(&index) {
                store.set(index, spot);
                self.next = self.next.max(index + 1);
            }
            self.last = Some(spot);
        }

        pub(super) fn end(&mut self, store: &mut Store) {
            if let Some(last) = self.last {
                while self.next < self.end {
                    store.set(self.next, last);
                    self.next += 1;
                }
            }
        }
    }

    fn encode(p: &Picture) -> Option<Vec<u8>> {
        media::encode_jpeg(&p.rgba, p.width as u16, p.height as u16, QUALITY)
    }
}

/// Finished proxies on disk, so a clip opens fully scrubbable: one file per
/// clip (keyed by path, size, modification time and frame count, so an edited
/// or replaced file gets a fresh one), the least recently opened dropped once
/// they pass [`CAP`].
///
/// A file is the frames' JPEGs one after another (as [`Data`] wrote them
/// while building), then each slot's place (offset `u64`, length `u32`;
/// offset `u64::MAX`: none), then the slot count, where those start (both
/// `u64`) and [`MAGIC`]. Opening one reads only that index.
mod cache {
    use super::*;
    use std::hash::{Hash, Hasher};

    /// Disk the kept proxies may take, in all.
    const CAP: u64 = 5_000_000_000;
    const MAGIC: &[u8; 8] = b"HCPROXY2";
    /// The format before: the whole file was read into memory to open it.
    const OLD_MAGIC: &[u8; 8] = b"HCPROXY1";
    /// Bytes per slot in the index, and in the footer.
    const SLOT: usize = 12;
    const FOOTER: u64 = 24;

    pub fn dir() -> PathBuf {
        dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("hasteclip").join("proxy")
    }

    pub fn key(source: &Path, fps: f64, total: usize) -> Option<u64> {
        let meta = std::fs::metadata(source).ok()?;
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (MAGIC, source, meta.len(), meta.modified().ok()?, meta.created().ok(), fps.to_bits(), total, super::QUALITY).hash(&mut h);
        Some(h.finish())
    }

    pub fn path(key: u64) -> PathBuf {
        dir().join(format!("{key:016x}.proxy"))
    }

    /// A new file for a build: beside `kept` (the file it becomes), else in
    /// the temporary folder. Never one another build may be using.
    pub fn partial_path(kept: Option<&Path>) -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let name = format!("{}-{}", std::process::id(), n);
        match kept {
            Some(p) => p.with_extension(format!("{name}.partial")),
            None => std::env::temp_dir().join(format!("hasteclip-proxy-{name}.partial")),
        }
    }

    /// The build in `data` is complete: write its index after the frames and
    /// keep it at `path`.
    pub fn finish(data: &Data, frames: &[Option<Spot>], path: &Path) -> std::io::Result<()> {
        let mut w = data.write.lock().unwrap();
        let (file, len) = w.as_mut().ok_or_else(|| std::io::Error::other("already finished"))?;
        let mut tail = Vec::with_capacity(frames.len() * SLOT + FOOTER as usize);
        for f in frames {
            let s = f.unwrap_or(Spot { at: u64::MAX, len: 0 });
            tail.extend_from_slice(&s.at.to_le_bytes());
            tail.extend_from_slice(&s.len.to_le_bytes());
        }
        tail.extend_from_slice(&(frames.len() as u64).to_le_bytes());
        tail.extend_from_slice(&len.to_le_bytes());
        tail.extend_from_slice(MAGIC);
        file.write_all(&tail)?;
        file.flush()?;
        *w = None;
        drop(w);
        // Readers keep their handle across the rename.
        let from = data.path.lock().unwrap().clone();
        std::fs::rename(&from, path)?;
        *data.path.lock().unwrap() = path.to_path_buf();
        data.discard.store(false, Ordering::Relaxed);
        evict();
        Ok(())
    }

    /// The kept proxy at `path`, if there is a whole one for `total` frames:
    /// its file, open, and the slots. Anything else there is deleted.
    pub fn load(path: &Path, total: usize) -> Option<(Data, Store)> {
        let file = std::fs::File::open(path).ok()?;
        let Some(store) = parse(&file, total) else {
            drop(file);
            let _ = std::fs::remove_file(path);
            return None;
        };
        // Just opened: the last to be dropped.
        let _ = std::fs::File::options().write(true).open(path).and_then(|f| f.set_modified(std::time::SystemTime::now()));
        let data = Data { write: Mutex::new(None), read: file, path: Mutex::new(path.to_path_buf()), discard: AtomicBool::new(false) };
        Some((data, store))
    }

    fn parse(file: &std::fs::File, total: usize) -> Option<Store> {
        let len = file.metadata().ok()?.len();
        let mut footer = [0u8; FOOTER as usize];
        read_at(file, &mut footer, len.checked_sub(FOOTER)?).ok()?;
        let u64_at = |b: &[u8]| u64::from_le_bytes(b.try_into().unwrap());
        let (slots, index_at) = (u64_at(&footer[..8]) as usize, u64_at(&footer[8..16]));
        if &footer[16..] != MAGIC || slots != total || index_at.checked_add(slots.checked_mul(SLOT)? as u64)? != len - FOOTER {
            return None;
        }
        let mut index = vec![0u8; slots * SLOT];
        read_at(file, &mut index, index_at).ok()?;
        let mut store = Store::default();
        for (i, s) in index.chunks_exact(SLOT).enumerate() {
            let spot = Spot { at: u64_at(&s[..8]), len: u32::from_le_bytes(s[8..].try_into().unwrap()) };
            if spot.at != u64::MAX {
                if spot.at.checked_add(spot.len as u64)? > index_at {
                    return None;
                }
                store.set(i, spot);
            }
        }
        Some(store)
    }

    /// Drop the least recently opened until they fit in [`CAP`]; also any in
    /// the old format, and builds a crash left behind.
    fn evict() {
        let Ok(entries) = std::fs::read_dir(dir()) else { return };
        let old = |p: &Path| {
            let mut head = [0u8; 8];
            std::fs::File::open(p).and_then(|mut f| f.read_exact(&mut head)).is_ok() && &head == OLD_MAGIC
        };
        let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = Vec::new();
        for e in entries.flatten() {
            let (Ok(m), path) = (e.metadata(), e.path()) else { continue };
            let Ok(modified) = m.modified() else { continue };
            let stale = path.extension().is_some_and(|x| x == "partial") && modified.elapsed().is_ok_and(|a| a > Duration::from_secs(24 * 3600));
            if stale || old(&path) {
                let _ = std::fs::remove_file(&path);
                continue;
            }
            files.push((modified, m.len(), path));
        }
        files.sort();
        let mut total: u64 = files.iter().map(|f| f.1).sum();
        for (_, len, path) in files {
            if total <= CAP {
                break;
            }
            if std::fs::remove_file(&path).is_ok() {
                total -= len;
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn kept_proxies_read_back() {
            let kept = std::env::temp_dir().join(format!("hasteclip-proxy-test-{}.proxy", std::process::id()));
            let data = Data::create(partial_path(Some(&kept))).unwrap();
            let a = data.append(&[1, 1, 1]).unwrap();
            let b = data.append(&[2, 2]).unwrap();
            // Readable while it's still being written.
            assert_eq!(data.read(b).unwrap(), [2, 2]);
            let mut store = Store::default();
            store.set(0, a);
            store.set(1, a);
            store.set(3, b);
            finish(&data, &store.frames, &kept).unwrap();
            assert_eq!(data.read(a).unwrap(), [1, 1, 1], "still readable after it's kept");
            drop(data);
            assert!(kept.exists(), "kept, not deleted");
            let (back, slots) = load(&kept, 4).expect("reads back");
            assert_eq!(slots.ready, 3);
            assert_eq!(back.read(slots.frames[1].unwrap()).unwrap(), [1, 1, 1]);
            assert_eq!(back.read(slots.frames[3].unwrap()).unwrap(), [2, 2]);
            assert!(slots.frames[2].is_none());
            assert_eq!(slots.frames[0], slots.frames[1], "one copy per frame");
            drop(back);
            assert!(load(&kept, 5).is_none(), "another frame count isn't this clip's");
            assert!(!kept.exists(), "a file that isn't this clip's is dropped");
        }

        #[test]
        fn unfinished_builds_are_deleted() {
            let path = partial_path(None);
            let data = Data::create(path.clone()).unwrap();
            data.append(&[7; 100]).unwrap();
            assert!(path.exists());
            drop(data);
            assert!(!path.exists());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Gaps repeat the frame before; a slot two frames land on keeps the
    /// later; a chunk starting in a gap shows the frame before it.
    #[cfg(hw_decode)]
    #[test]
    fn chunks_fill_their_slots() {
        let j = |n: u8| Spot { at: n as u64, len: 1 };
        let mut store = Store::default();
        let mut c = hw::Chunk::new(0, 7);
        for (i, n) in [(0, 0), (1, 1), (3, 3), (3, 4), (6, 6)] {
            c.frame(&mut store, i, j(n));
        }
        c.end(&mut store);
        let got: Vec<u64> = store.frames.iter().map(|x| x.unwrap().at).collect();
        assert_eq!(got, [0, 1, 1, 4, 4, 4, 6]);
        // A chunk 10..14 whose first frame (the one showing at 10) is 8.
        let mut c = hw::Chunk::new(10, 14);
        for (i, n) in [(8, 8), (12, 12), (15, 15)] {
            c.frame(&mut store, i, j(n));
        }
        c.end(&mut store);
        let got: Vec<u64> = (10..14).map(|i| store.frames[i].unwrap().at).collect();
        assert_eq!(got, [8, 8, 12, 12]);
        assert_eq!(store.ready, 11);
    }

    /// Chunks are built from where the player is: there, ahead, then behind.
    #[cfg(hw_decode)]
    #[test]
    fn chunks_start_at_the_player() {
        let mut built = vec![false; 10];
        let mut order = Vec::new();
        while let Some(c) = hw::next_chunk(&built, 6) {
            built[c] = true;
            order.push(c);
        }
        assert_eq!(order, [6, 7, 8, 9, 5, 4, 3, 2, 1, 0]);
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
            let mut got: Vec<Vec<u8>> = Vec::new();
            split_jpegs(Chunked(stream.clone(), chunk), &egui::Context::default(), |j| {
                got.push(j.to_vec());
                true
            });
            assert_eq!(got, (1..=5).map(frame).collect::<Vec<_>>(), "chunk size {chunk}");
        }
    }
}

#[cfg(test)]
mod speed {
    /// `cargo test --release -p hasteclip proxy_jpeg_speed -- --ignored --nocapture`
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
    /// cargo test --release -p hasteclip proxy_formats -- --ignored --nocapture`
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

#[cfg(test)]
mod quality {
    /// Size and colour error of proxy JPEGs: `PROXY_DIR=<dir with f640/f960/f1280.raw>
    /// cargo test --release -p hasteclip proxy_quality -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn proxy_quality() {
        let Some(dir) = std::env::var_os("PROXY_DIR") else { return };
        for (w, h) in [(640u32, 360u32), (960, 540), (1280, 720)] {
            let rgba = std::fs::read(std::path::Path::new(&dir).join(format!("f{w}.raw"))).unwrap();
            for q in [75u8, 85, 90, 95] {
                let t = std::time::Instant::now();
                let jpeg = media::encode_jpeg(&rgba, w as u16, h as u16, q).unwrap();
                let enc = t.elapsed().as_secs_f64() * 1000.0;
                let back = image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg).unwrap().to_rgba8();
                let (mut sum, mut max) = (0u64, 0u8);
                for (a, b) in rgba.chunks_exact(4).zip(back.as_raw().chunks_exact(4)) {
                    for c in 0..3 {
                        let d = a[c].abs_diff(b[c]);
                        sum += d as u64;
                        max = max.max(d);
                    }
                }
                let mean = sum as f64 / (w * h * 3) as f64;
                println!("{w}x{h} q{q}: {:>4} KB ({:>5.0} MB per 2-min clip), encode {enc:.1} ms, colour error mean {mean:.2} max {max}", jpeg.len() / 1024, jpeg.len() as f64 * 7200.0 / 1e6);
            }
        }
    }
}
