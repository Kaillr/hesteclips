//! Live scrubbing: the picture under the playhead on every redraw, out of
//! a cache of decoded frames that a pool of decoders fills ahead of it.
//!
//! The UI looks the playhead's frame up here on each redraw and shows it
//! at once: nothing is asked of a thread and shown a redraw later. A frame
//! not decoded yet shows as the nearest one that is, so the picture moves
//! with the playhead instead of freezing on an old one.
//!
//! Frames are decoded by several decoders at once ([`decoders`]), one
//! thread each, each on a stretch of the video from a keyframe (the only
//! place H.264 decoding can start): together they decode three to four
//! times as many frames a second as one decoder on every core does, and
//! a jump costs one of them its start, not all of them. Which stretch
//! each takes follows the playhead ([`Inner::next_job`]): the ones it's
//! heading into nearest first, as far ahead as the cache has room for,
//! counting only the frames a decoder would have out before the playhead
//! gets to them ([`Inner::reachable`]: at speed, that's the first part of
//! each stretch ahead, and none of the stretch the playhead is in); both
//! ways when it stands still, so a drag that starts from a pause has its
//! frames in hand; and every keyframe near it first of all, so there's a
//! picture to show at any speed. A decoder drops its stretch once the
//! playhead has passed what it's decoding, or one nearer needs doing.
//!
//! The cache keeps the frames nearest the playhead ([`budget`]), those it
//! has passed going first (three times as readily as those ahead, once
//! it's moving at speed), keyframes last ([`KEY`] times as far off): a
//! frame passed is only wanted again on a turn, and keyframes are what a
//! fast scrub shows. Frames decoded for playback are kept too
//! ([`Engine::offer`]), for a scrub back over what just played.
//!
//! Where the frames are, and who decodes them, depends on the platform:
//!
//! - **Linux**: libavcodec on the CPU, a thread per decoder, frames in
//!   memory. The frames the next few redraws will show are uploaded to the
//!   GPU ahead of them by a thread of their own ([`Uploader`]): uploading
//!   one on the UI thread cost it more than drawing everything else did.
//!   On the card (NVIDIA's NVDEC, AMD's and Intel's through VA-API),
//!   libavcodec decodes there into buffers of the renderer's, a pool for
//!   each decoder as on Windows, and the card copies the frames the next
//!   redraws show into their textures.
//! - **macOS**: VideoToolbox (the media engine), frames in IOSurfaces the
//!   renderer draws from where they are.
//! - **Windows**: Media Foundation on the graphics card, frames in
//!   textures shared with the renderer, each decoder's in a pool of its
//!   own made when it opens ([`Inner::limits`]). The card's decoder takes
//!   turns between everything decoding on it, so while another app is
//!   decoding video (a browser playing one) only one decoder works
//!   ([`watch_others`]): with three, that video froze.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[cfg(not(windows))]
use capture::decode::Output;
use capture::decode::{Decoder, Picture};

/// Memory for decoded frames while scrubbing ([`platform_budget`]).
/// Not scrubbing, it keeps less ([`Inner::idle_cap`]).
/// `HASTECLIP_SCRUB_MEMORY_MB` overrides it.
fn budget(gpu: &Gpu) -> u64 {
    if let Some(mb) = std::env::var("HASTECLIP_SCRUB_MEMORY_MB").ok().and_then(|s| s.parse::<u64>().ok()) {
        return mb << 20;
    }
    platform_budget(gpu)
}

/// Linux: frames in memory. An eighth of the machine's, or a quarter of
/// what's free when the clip opens if that's less (a game may hold the
/// rest), from 128 MB to 3 GB (a 1440p frame is 5.5 MB: up to 550 of
/// them, 9 s of 60 fps). Decoded on the card, as on Windows: a sixth of the
/// card's own memory, from 512 MB to 1.5 GB (games want the rest).
#[cfg(target_os = "linux")]
fn platform_budget(gpu: &Gpu) -> u64 {
    if let Some(card) = &gpu.card {
        return card.memory().map_or(512 << 20, |m| m / 6).clamp(512 << 20, 3 << 29);
    }
    let share = capture::total_memory().map_or(1 << 30, |m| m / 8);
    let free = meminfo("MemAvailable").map_or(u64::MAX, |m| m / 4);
    share.min(free).clamp(128 << 20, 3 << 30)
}

/// macOS: frames in memory too (IOSurfaces, which the graphics card shares):
/// an eighth of the machine's, from 128 MB to 3 GB.
#[cfg(target_os = "macos")]
fn platform_budget(_: &Gpu) -> u64 {
    capture::total_memory().map_or(1 << 30, |m| m / 8).clamp(128 << 20, 3 << 30)
}

/// Windows: frames in textures on the graphics card, made when the
/// decoders open (each one's pool: [`Source::open`]). A sixth of the
/// card's own memory, from 512 MB to 1.5 GB (games want the rest); on one
/// that shares the system's memory, a sixteenth of that, 256 MB to 1 GB.
#[cfg(windows)]
fn platform_budget(gpu: &Gpu) -> u64 {
    match Decoder::video_memory(gpu.luid).filter(|&m| m >= 1 << 30) {
        Some(own) => (own / 6).clamp(512 << 20, 3 << 29),
        None => capture::total_memory().map_or(512 << 20, |m| m / 16).clamp(256 << 20, 1 << 30),
    }
}

/// A figure from /proc/meminfo (`key`, like "MemAvailable"), in bytes.
#[cfg(target_os = "linux")]
fn meminfo(key: &str) -> Option<u64> {
    let info = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kb: u64 = info.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix(':'))?.trim().trim_end_matches("kB").trim().parse().ok()?;
    Some(kb * 1024)
}

/// How many decoders ([`platform_decoders`]). `HASTECLIP_SCRUB_DECODERS`
/// overrides it.
fn decoders(gpu: &Gpu) -> usize {
    if let Some(n) = std::env::var("HASTECLIP_SCRUB_DECODERS").ok().and_then(|s| s.parse::<usize>().ok()) {
        return n.clamp(1, 32);
    }
    platform_decoders(gpu)
}

/// Linux: one per physical core, 2 to 12. More (one per hardware thread)
/// decoded no more frames in all, and each one so much slower (25 ms a
/// frame instead of 6, on 6 cores) that none could keep up with the
/// playhead at 1×, while the UI, sharing a core with one, drew half as
/// often. Decoding on the card, 3, as elsewhere.
#[cfg(target_os = "linux")]
fn platform_decoders(gpu: &Gpu) -> usize {
    if gpu.card.is_some() {
        return 3;
    }
    let logical = std::thread::available_parallelism().map_or(4, |n| n.get());
    physical_cores().unwrap_or(logical / 2).clamp(2, 12)
}

/// macOS and Windows: the graphics card's decoder (the Mac's media engine)
/// decodes for all of them. A few at once keep it busy and work in as many
/// places ahead of the playhead; more would only hold more of its memory.
#[cfg(not(target_os = "linux"))]
fn platform_decoders(_: &Gpu) -> usize {
    3
}

/// Where decoded frames go: on Windows, the graphics card the renderer
/// draws with (frames are shared with it); on Linux, the renderer's memory
/// on the card where it decodes there; elsewhere, wherever the decoder puts
/// them.
#[derive(Clone)]
struct Gpu {
    #[cfg(windows)]
    luid: u64,
    #[cfg(target_os = "linux")]
    card: Option<Arc<capture::decode::Card>>,
}

impl Gpu {
    /// The renderer's, if it can draw decoded frames where they are.
    #[cfg(windows)]
    fn find() -> Option<Self> {
        crate::gpu_frames::share_luid().map(|luid| Self { luid })
    }

    #[cfg(target_os = "macos")]
    fn find() -> Option<Self> {
        crate::gpu_frames::available().then_some(Self {})
    }

    #[cfg(target_os = "linux")]
    fn find() -> Option<Self> {
        Some(Self { card: crate::gpu_frames::card() })
    }
}

/// One of the engine's decoders, opened the platform's way.
struct Source {
    dec: Decoder,
}

impl Source {
    /// Open `path`, frames numbered at `fps`. `bytes`: the memory for its
    /// frames, where a decoder sets that aside as it opens (Windows; Linux
    /// on the card, its pool of buffers there).
    /// Linux: on one thread (each decoder has a core).
    #[cfg(target_os = "linux")]
    fn open(path: &Path, fps: f64, gpu: &Gpu, bytes: u64) -> anyhow::Result<Self> {
        let on_card = gpu.card.as_ref().and_then(|card| {
            Decoder::open_card_pooled(path, card, bytes).inspect_err(|e| eprintln!("scrubbing: decoding on the CPU ({e:#})")).ok()
        });
        let mut dec = match on_card {
            Some(d) => d,
            None => Decoder::open_threads(path, Output::Screen, 1)?,
        };
        dec.set_fps(fps);
        Ok(Self { dec })
    }

    #[cfg(target_os = "macos")]
    fn open(path: &Path, fps: f64, _: &Gpu, _: u64) -> anyhow::Result<Self> {
        let mut dec = Decoder::open(path, Output::Screen)?;
        dec.set_fps(fps);
        Ok(Self { dec })
    }

    /// Windows: frames in a pool of textures of its own holding `bytes` of
    /// them, shared with the renderer.
    #[cfg(windows)]
    fn open(path: &Path, fps: f64, gpu: &Gpu, bytes: u64) -> anyhow::Result<Self> {
        let mut dec = Decoder::open_direct_pooled(path, gpu.luid, bytes)?;
        dec.set_fps(fps);
        Ok(Self { dec })
    }

    /// Before the next frame (Linux): a buffer let go of, for it to go into
    /// (no fresh memory per frame).
    #[cfg(target_os = "linux")]
    fn reuse(&mut self, shared: &Shared) {
        if self.dec.spare() == 0 {
            if let Some(buf) = shared.spare.lock().unwrap().pop() {
                self.dec.recycle(buf);
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn reuse(&mut self, _: &Shared) {}
}

/// The memory picture `p` takes: NV12 (RGBA on Windows where the graphics
/// card can't share NV12).
fn frame_bytes(p: &Picture) -> u64 {
    let pixels = p.width as u64 * p.height as u64;
    #[cfg(windows)]
    if p.gpu.as_ref().is_some_and(|s| s.format() == capture::decode::SurfaceFormat::Rgba) {
        return pixels * 4;
    }
    pixels * 3 / 2
}

/// Frames decoded for playback are kept too ([`Engine::offer`]), except on
/// Windows: they're textures of playback's own few dozen, which it would
/// run short of.
const KEEPS_PLAYBACK: bool = !cfg!(windows);
/// A scrub that ended lets go of what it kept beyond the idle room
/// ([`Inner::idle_cap`]), except on Windows: the textures were made when the
/// decoders opened, and stay; letting their frames go frees nothing.
const IDLE_FREES: bool = !cfg!(windows);
/// Windows: textures a decoder's pool keeps free besides its frames in the
/// cache: the UI holds a few (on screen, and for a few passes after while
/// the GPU may still draw them), and one is being decoded into. With none
/// free, a decoder waits for one.
const POOL_FREE: usize = 10;
/// Frames held besides the cache's, counted out of the budget: per decoder
/// its own (Linux: its reference frames and a few spares, about a dozen;
/// macOS: its queue and reference frames), and in all (Linux: the spare
/// buffers; macOS: the IOSurfaces the renderer keeps open, at most 64).
#[cfg(target_os = "linux")]
const OWN_FRAMES: u64 = 12;
#[cfg(target_os = "linux")]
const EXTRA_FRAMES: u64 = SPARE as u64;
#[cfg(not(target_os = "linux"))]
const OWN_FRAMES: u64 = 8;
#[cfg(not(target_os = "linux"))]
const EXTRA_FRAMES: u64 = 64;

/// How many frames the cache holds while scrubbing: `bytes` each, out of
/// `budget`, with `workers` decoders; or (Windows) what their pools of
/// `pool` textures hold, less what each keeps free.
fn cap(budget: u64, bytes: u64, workers: usize, pool: Option<usize>) -> usize {
    if let Some(n) = pool {
        return (workers * n.saturating_sub(POOL_FREE)).max(8);
    }
    let others = OWN_FRAMES * workers as u64 + EXTRA_FRAMES;
    (budget.saturating_sub(others * bytes) / bytes.max(1)).max(8) as usize
}

/// The physical cores, from the kernel's view of which hardware threads
/// share one.
#[cfg(target_os = "linux")]
fn physical_cores() -> Option<usize> {
    let mut cores = std::collections::HashSet::new();
    for entry in std::fs::read_dir("/sys/devices/system/cpu").ok()?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("cpu") || !name[3..].chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        if let Ok(siblings) = std::fs::read_to_string(entry.path().join("topology/thread_siblings_list")) {
            cores.insert(siblings.trim().to_string());
        }
    }
    (!cores.is_empty()).then_some(cores.len())
}

/// Slower than this (times real time, either way) the playhead counts as
/// standing still: frames both ways are decoded, nearest first.
const STILL: f64 = 0.3;
/// Faster than this (times real time), frames passed leave a full cache
/// before frames ahead, [`BEHIND`] times as readily: at speed a turn is
/// rare. Slower, a drag goes back and forth over them.
const AT_SPEED: f64 = 2.5;
const BEHIND: u64 = 3;
/// Keyframes stay this many times farther from the playhead than other
/// frames do (with 4 s of frames kept ahead at 20×, keyframes from the
/// next minute and a half; a sixtieth of the frames, so a third of a
/// long clip's cache at most).
const KEY: u64 = 24;
/// A keyframe by itself (one frame, quick) is decoded this much sooner
/// than a whole stretch at the same distance: far ones are what a fast
/// scrub shows.
const KEY_FIRST: f64 = 0.5;
/// A decoder on a stretch looks for one more pressing every this many frames.
const LOOK: u64 = 2;
/// Behind the playhead in its stretch, a decoder catches it up only if it
/// moves at most this fraction of the decoder's pace (nearer the pace,
/// catching up takes forever); once on it, it stays while under the pace.
const CATCH_UP: f64 = 0.85;
/// At speed, a stretch with fewer frames than this to be had in time
/// isn't worth the start on it (a keyframe costs a couple of frames).
const WORTH: u64 = 6;
/// A frame farther than this (seconds of video) from the playhead isn't
/// shown in place of its frame: keep what's on screen instead.
const NEAR: f64 = 10.0;

/// A cached frame and the last slot it's known to show at (up to the slot
/// before the next frame decoded after it: a dropped frame's slot shows
/// the frame before the gap).
struct Entry {
    pic: Picture,
    to: u64,
    /// The decoder that made it (none: playback's), for [`Inner::limits`].
    from: Option<usize>,
}

/// A cached frame that would make way for another, and its weight
/// ([`Inner::heaviest`]): none when there's room.
type Weighed = Option<(u64, u64)>;

/// What a decoder does next: a stretch's keyframe alone, or the stretch.
#[derive(Clone, Copy)]
enum Job {
    Key(usize),
    Stretch(usize),
}

#[derive(Default)]
struct Stats {
    decoded: u64,
    evicted: u64,
    since: Option<Instant>,
}

struct Inner {
    /// Keyframe numbers, ascending (from the first decoder opened).
    keys: Vec<u64>,
    /// How many frames fit the budget while scrubbing, and how many are
    /// kept otherwise (playing, paused: about 4 s around the playhead, at
    /// most half the budget), so just watching doesn't hold it all.
    cap: usize,
    idle_cap: usize,
    /// A scrub ended with more kept than that: the next idle decoder lets
    /// the rest go and gives the memory back to the system.
    shrink: bool,
    /// Where the playhead is, how fast it moves (frames a second, below
    /// zero going back), and whether it's being scrubbed.
    at: u64,
    speed: f64,
    scrubbing: bool,
    /// Playing: nothing is decoded (the playback decoder has the frames).
    resting: bool,
    /// The playhead is held still: the UI is woken only for its own frame
    /// (it doesn't step through the frames on the way to it).
    quiet: bool,
    /// How long a decoder takes per frame (s), smoothed.
    frame_secs: f64,
    frames: BTreeMap<u64, Entry>,
    /// Per decoder, the stretch (an index into `keys`) it's on.
    jobs: Vec<Option<usize>>,
    /// How many decoders work (the first so many): all of them, or
    /// (Windows) one while another app is decoding video ([`watch_others`]).
    working: usize,
    /// Per decoder, how many of its frames the cache may hold, if that's
    /// limited (Windows: its pool's textures, less [`POOL_FREE`]), and how
    /// many it holds.
    limits: Vec<Option<usize>>,
    held: Vec<usize>,
    /// Stretches whose keyframe can't be decoded: left alone.
    dead: Vec<bool>,
    /// The number of the picture the UI took last, for the repaint.
    shown: Option<u64>,
    stats: Stats,
}

struct Shared {
    inner: Mutex<Inner>,
    /// Decoders wait on it when there's nothing to do.
    wake: Condvar,
    /// Linux: frame buffers let go of (frames dropped from the cache, from
    /// wherever), for the decoders' next frames: no fresh memory per frame.
    #[cfg(target_os = "linux")]
    spare: Mutex<Vec<Vec<u8>>>,
    /// Where the decoders put their frames.
    gpu: Gpu,
    stop: AtomicBool,
    /// Decoders that couldn't open or broke.
    failed: AtomicUsize,
    workers: usize,
    ctx: egui::Context,
    fps: f64,
    /// Frames in the clip, as the app counts them.
    total: u64,
    budget: u64,
}

/// Linux: frame buffers kept for reuse, at most (6 MB each at 1440p).
#[cfg(target_os = "linux")]
const SPARE: usize = 16;

impl Shared {
    /// Frames let go of, with the cache's lock let go (a scrub that ends
    /// lets hundreds go). Linux: their buffers are kept for the decoders'
    /// next frames (those still shown somewhere are left to go when
    /// they're done).
    #[cfg(target_os = "linux")]
    fn let_go(&self, pics: Vec<Picture>) {
        let mut spare = self.spare.lock().unwrap();
        let mut extra = Vec::new();
        for p in pics {
            if spare.len() >= SPARE {
                extra.push(p);
            } else if let Some(capture::decode::Nv12::Memory(nv12)) = p.gpu.map(|s| s.nv12)
                && let Ok(buf) = Arc::try_unwrap(nv12)
            {
                spare.push(buf);
            }
        }
        drop(spare);
        drop(extra);
    }

    /// Elsewhere a frame's memory goes back to its decoder (Windows: its
    /// texture to the pool) once nothing shows it.
    #[cfg(not(target_os = "linux"))]
    fn let_go(&self, pics: Vec<Picture>) {
        drop(pics);
    }
}

/// The last of the engine (it, its decoders' threads and its watcher) gone:
/// its cache goes now, and with it the decoders' pools of textures, unless
/// a frame is still held somewhere. Logged (`HASTECLIP_DEBUG_VIDEO`), to see
/// that a clip let go of actually closed.
impl Drop for Shared {
    fn drop(&mut self) {
        if crate::player::debug() {
            let frames = self.inner.get_mut().map_or(0, |s| s.frames.len());
            eprintln!("{:>8.3} scrub: engine closed, letting go of the {frames} frames it kept", crate::player::uptime());
        }
    }
}

impl Inner {
    /// Which way the playhead moves: 1 forward, -1 back, 0 standing still.
    fn dir(&self, fps: f64) -> i8 {
        if self.speed > STILL * fps {
            1
        } else if self.speed < -STILL * fps {
            -1
        } else {
            0
        }
    }

    /// The stretch frame `i` is in.
    fn stretch_of(&self, i: u64) -> usize {
        self.keys.partition_point(|&k| k <= i).saturating_sub(1)
    }

    /// The last frame of stretch `g`.
    fn end_of(&self, g: usize, total: u64) -> u64 {
        match self.keys.get(g + 1) {
            Some(next) => next - 1,
            None => total.saturating_sub(1).max(self.keys[g]),
        }
    }

    fn is_key(&self, i: u64) -> bool {
        self.keys.binary_search(&i).is_ok()
    }

    /// How readily frame `i` leaves a full cache: its distance from the
    /// playhead, counted [`BEHIND`] times for a frame passed at speed, an
    /// eighth for a keyframe. The heaviest goes first.
    fn weight(&self, i: u64, fps: f64) -> u64 {
        let d = i.abs_diff(self.at);
        let passed = (i < self.at && self.speed > AT_SPEED * fps) || (i > self.at && self.speed < -AT_SPEED * fps);
        let w = if passed { d.saturating_mul(BEHIND) } else { d };
        if self.is_key(i) { w / KEY } else { w }
    }

    /// The cached frame that would leave first, and its weight. Weight
    /// grows with distance either side of the playhead (by a factor of
    /// its own per side, and for keyframes), so the heaviest is at an end
    /// of the cache: the first or last keyframe, or the first or last of
    /// the others.
    fn heaviest(&self, fps: f64) -> Option<(u64, u64)> {
        let ends = |keys: bool| {
            let first = self.frames.keys().copied().find(|&i| self.is_key(i) == keys);
            let last = self.frames.keys().rev().copied().find(|&i| self.is_key(i) == keys);
            [first, last]
        };
        ends(false).into_iter().chain(ends(true)).flatten().map(|i| (i, self.weight(i, fps))).max_by_key(|&(_, w)| w)
    }

    /// How many frames are kept now: all the budget holds while scrubbing,
    /// less otherwise.
    fn room(&self) -> usize {
        if self.scrubbing { self.cap } else { self.idle_cap }
    }

    /// Whether frame `i` would be kept: room for it, or it's nearer than
    /// something kept.
    fn fits(&self, i: u64, fps: f64) -> bool {
        self.fits_with(i, self.heaviest(fps), fps)
    }

    /// Whether frame `i`, decoded by decoder `w`, would be kept: the cache
    /// would keep it, and so would the decoder's own share of it (Windows:
    /// its pool's textures, [`Self::store`]). Asking only the first, a
    /// decoder whose pool was full was given frames it then let go, the same
    /// ones again and again, for as long as the clip was open.
    fn fits_for(&self, w: usize, i: u64, fps: f64) -> bool {
        self.fits(i, fps) && self.own_room(w, i, self.own_heaviest(w, fps), fps)
    }

    /// Whether decoder `w` has a texture free for another frame (its share
    /// isn't limited, or isn't full).
    fn own_free(&self, w: usize) -> bool {
        self.limits[w].is_none_or(|limit| self.held[w] < limit)
    }

    /// Decoder `w`'s cached frame that would make way for another of its
    /// own, when its share is full (none while it has room).
    fn own_heaviest(&self, w: usize, fps: f64) -> Option<(u64, u64)> {
        if self.own_free(w) { None } else { self.heaviest_of(w, fps) }
    }

    /// Whether decoder `w` would keep frame `i`: it has room, or `i` is
    /// nearer than the frame of its own that makes way (`own`, from
    /// [`Self::own_heaviest`]) and the cache is full. While it isn't, the
    /// decoders with room do the work: a full one keeping a frame (a far
    /// keyframe, weighing little) let a frame near the playhead go, which
    /// one with room then decoded again, its whole stretch for that one.
    /// Standing still, it's the decoder holding the cache's heaviest frame
    /// that makes way (as one cache would: what goes is what nothing wants
    /// back; a far keyframe in another's place let a frame near the
    /// playhead go, decoded again with its whole stretch), and only for a
    /// frame less than half as far (one barely nearer, 40 frames off for
    /// 41, cost its stretch from the keyframe, two decoders taking turns at
    /// it for seconds after every pause). After a jump the frames kept are
    /// far, and those around the playhead still take their places. Moving,
    /// any full decoder makes way for a nearer frame, so all keep up.
    fn own_room(&self, w: usize, i: u64, own: Option<(u64, u64)>, fps: f64) -> bool {
        if self.own_free(w) {
            return true;
        }
        let Some((_, hw)) = own.filter(|_| self.frames.len() >= self.room()) else { return false };
        let wi = self.weight(i, fps);
        if self.dir(fps) != 0 {
            return wi < hw;
        }
        wi.saturating_mul(2) < hw && self.heaviest(fps).is_none_or(|(_, top)| hw >= top)
    }

    /// [`Self::fits`] with the heaviest frame found already (it's the same
    /// for every frame asked about while nothing changes).
    fn fits_with(&self, i: u64, heavy: Option<(u64, u64)>, fps: f64) -> bool {
        self.frames.len() < self.room() || heavy.is_none_or(|(_, w)| self.weight(i, fps) < w)
    }

    /// Down to [`Self::room`]: the frames let go of, farthest first.
    fn shrink_to_room(&mut self, fps: f64) -> Vec<Picture> {
        let mut out = Vec::new();
        while self.frames.len() > self.room() {
            let Some((far, _)) = self.heaviest(fps) else { break };
            if let Some(e) = self.remove(far) {
                out.push(e.pic);
            }
        }
        out
    }

    /// Frame `i` out of the cache, counted off its decoder's.
    fn remove(&mut self, i: u64) -> Option<Entry> {
        let e = self.frames.remove(&i)?;
        if let Some(d) = e.from {
            self.held[d] = self.held[d].saturating_sub(1);
        }
        Some(e)
    }

    /// Decoder `d`'s cached frame that would leave first, and its weight.
    fn heaviest_of(&self, d: usize, fps: f64) -> Option<(u64, u64)> {
        self.frames.iter().filter(|(_, e)| e.from == Some(d)).map(|(&i, _)| (i, self.weight(i, fps))).max_by_key(|&(_, w)| w)
    }

    /// Frame `prev` was decoded right before frame `i`: it shows up to the
    /// slot before (`u64::MAX`: to the end, `prev` being the last frame).
    fn cover(&mut self, prev: Option<u64>, i: u64) {
        if let Some(e) = prev.and_then(|p| self.frames.get_mut(&p)) {
            e.to = e.to.max(i.saturating_sub(1));
        }
    }

    /// Keep frame `pic` (numbered `i`, made by decoder `from`; none:
    /// playback's), making room for it. The frames let go of (those that
    /// made room; `pic` itself if it's heavier than all of them, or the one
    /// it replaces), for their memory to be used again, outside the lock.
    fn store(&mut self, i: u64, pic: Picture, from: Option<usize>, fps: f64) -> Vec<Picture> {
        let mut out = Vec::new();
        if self.frames.contains_key(&i) {
            // Decoded again (a decoder starts a stretch at its keyframe, so
            // it goes through what another kept of it): the same picture, and
            // the one there stays. Replaced, it moved to this decoder's pool:
            // two decoders on a stretch took its frames from each other and
            // both stopped where their pools were full, never getting past
            // it, decoding the same frames for as long as the clip was open.
            out.push(pic);
            return out;
        }
        let w = self.weight(i, fps);
        // Its decoder's share full (Windows: the textures it has, but for
        // those it keeps free): room among its own frames, else it isn't kept.
        if let Some(d) = from {
            while self.limits[d].is_some_and(|limit| self.held[d] >= limit) {
                let Some((far, fw)) = self.heaviest_of(d, fps) else { break };
                if fw <= w {
                    out.push(pic);
                    return out;
                }
                if let Some(e) = self.remove(far) {
                    out.push(e.pic);
                    self.stats.evicted += 1;
                }
            }
        }
        while self.frames.len() >= self.room() {
            let Some((far, fw)) = self.heaviest(fps) else { break };
            if fw <= w {
                out.push(pic);
                return out;
            }
            if let Some(e) = self.remove(far) {
                out.push(e.pic);
                self.stats.evicted += 1;
            }
        }
        self.frames.insert(i, Entry { pic, to: i, from });
        if let Some(d) = from {
            self.held[d] += 1;
        }
        self.stats.decoded += 1;
        out
    }

    /// The picture showing at frame `i` if it's cached (exact), else the
    /// nearest cached frame within [`NEAR`], if any.
    fn nearest(&self, i: u64, fps: f64) -> Option<(Picture, bool)> {
        let before = self.frames.range(..=i).next_back();
        if let Some((_, e)) = before {
            if e.to >= i {
                return Some((e.pic.clone(), true));
            }
        }
        let after = self.frames.range(i + 1..).next();
        let near = (NEAR * fps) as u64;
        let b = before.map(|(_, e)| (i - e.to, e));
        let a = after.map(|(&k, e)| (k - i, e));
        let pick = match (b, a) {
            (Some(b), Some(a)) => Some(if b.0 <= a.0 { b } else { a }),
            (b, a) => b.or(a),
        };
        pick.filter(|(d, _)| *d <= near).map(|(_, e)| (e.pic.clone(), false))
    }

    /// The first frame from `from` on, up to `to`, that isn't cached.
    fn missing_forward(&self, from: u64, to: u64) -> Option<u64> {
        let mut i = from;
        while i <= to {
            match self.frames.range(..=i).next_back() {
                Some((_, e)) if e.to >= i => i = e.to.saturating_add(1),
                _ => return Some(i),
            }
        }
        None
    }

    /// The first frame from `from` back, down to `to`, that isn't cached.
    fn missing_backward(&self, from: u64, to: u64) -> Option<u64> {
        let mut i = from;
        loop {
            match self.frames.range(..=i).next_back() {
                Some((&k, e)) if e.to >= i => {
                    if k <= to {
                        return None;
                    }
                    i = k - 1;
                }
                _ => return Some(i),
            }
        }
    }

    /// How many frames the playhead moves per frame a decoder decodes:
    /// above 1, a decoder on the stretch the playhead is in can't catch
    /// it up, and only the first frames of a stretch ahead are out in time.
    fn pace(&self) -> f64 {
        self.speed.abs() * self.frame_secs
    }

    /// The frames of stretch `g` (from its keyframe `k` to `e`) a decoder
    /// starting on it now would have out before the playhead, going `dir`,
    /// gets to them: a range, or none. All of it standing still, going the
    /// other way (for a turn, when there's nothing else to do), or with
    /// the decoder the faster.
    fn reachable(&self, g: usize, dir: i8, total: u64) -> Option<(u64, u64)> {
        let (k, e, at) = (self.keys[g], self.end_of(g, total), self.at);
        let behind = (dir > 0 && e < at) || (dir < 0 && k > at);
        if dir == 0 || behind {
            return Some((k, e));
        }
        let r = self.pace();
        let (kf, ef, atf) = (k as f64, e as f64, at as f64);
        let (lo, hi) = if dir > 0 {
            if k <= at {
                // The playhead's own stretch: from behind it, only if it can
                // be caught up with.
                if r > CATCH_UP {
                    return None;
                }
                (atf, ef)
            } else {
                // Frame i is out (i - k) frame-times after the start and wanted
                // (i - at) / speed after: i < (r k - at) / (r - 1).
                (kf, if r > 1.0 { ((r * kf - atf) / (r - 1.0)).floor().min(ef) } else { ef })
            }
        } else {
            // Wanted (at - i) / speed after the start: i < (at + r k) / (r + 1);
            // frames past the playhead are behind it.
            (kf, ((atf + r * kf) / (r + 1.0)).floor().min(ef).min(atf))
        };
        if lo > hi || (r > 1.0 && hi - lo + 1.0 < WORTH as f64) {
            return None;
        }
        Some((lo as u64, hi as u64))
    }

    /// The first frame of stretch `g` not cached that decoder `w` starting
    /// on it now would have out in time ([`Self::reachable`]), counting
    /// from where the playhead gets into it going `dir` (both ways
    /// standing still), if it would be kept (`heavy`: see
    /// [`Self::fits_with`]; `own`: [`Self::own_heaviest`]).
    #[allow(clippy::too_many_arguments)]
    fn missing_of(&self, g: usize, dir: i8, heavy: Option<(u64, u64)>, w: usize, own: Option<(u64, u64)>, fps: f64, total: u64) -> Option<u64> {
        let (lo, hi) = self.reachable(g, dir, total)?;
        let at = self.at;
        let m = if hi < at {
            self.missing_backward(hi, lo)
        } else if lo > at {
            self.missing_forward(lo, hi)
        } else {
            match dir {
                1 => self.missing_forward(at, hi),
                -1 => self.missing_backward(at, lo),
                _ => match (self.missing_forward(at, hi), self.missing_backward(at, lo)) {
                    (Some(f), Some(b)) => Some(if f - at <= at - b { f } else { b }),
                    (f, b) => f.or(b),
                },
            }
        };
        m.filter(|&m| self.fits_with(m, heavy, fps) && self.own_room(w, m, own, fps))
    }

    /// What decoder `w` does next, its decoder about to give frame
    /// `position` reading on: the most pressing stretch (or keyframe) no
    /// other decoder is on. Pressing is near the playhead going its way;
    /// stretches it's left behind come last.
    fn next_job(&self, w: usize, position: Option<u64>, fps: f64, total: u64) -> Option<Job> {
        if self.resting || self.keys.is_empty() || w >= self.working {
            return None;
        }
        let (at, dir) = (self.at, self.dir(fps));
        let heavy = self.heaviest(fps);
        let own = self.own_heaviest(w, fps);
        // Full, nothing farther than this from the playhead would be kept (a
        // keyframe weighs a [`KEY`]th of its distance); nor, its own share
        // full, by this decoder.
        let reach = heavy.filter(|_| self.frames.len() >= self.room()).map(|(_, hw)| hw.saturating_mul(KEY));
        let reach = match own {
            Some((_, ow)) => Some(reach.map_or(ow.saturating_mul(KEY), |r| r.min(ow.saturating_mul(KEY)))),
            None if !self.own_free(w) => Some(0),
            None => reach,
        };
        let mut best: Option<(f64, Job)> = None;
        // Outward from the playhead's stretch, a side at a time, each side
        // ending where nothing could beat what's found or be kept: every
        // offer is worth at least half its distance (more for stretches
        // left behind). A clip with thousands of stretches isn't gone
        // through for each job, holding the lock the UI waits on.
        let h = self.stretch_of(at);
        self.offer_stretch(h, w, position, (heavy, own), fps, total, &mut best);
        let (mut lo, mut hi) = (h, h);
        let (mut left, mut right) = (h > 0, h + 1 < self.keys.len());
        let out_of_reach = |d: u64, behind: bool, best: &Option<(f64, Job)>| {
            let least = 0.5 * (d as f64 + if behind { 1e9 } else { 0.0 });
            best.as_ref().is_some_and(|(b, _)| least >= *b) || reach.is_some_and(|r| d >= r)
        };
        while left || right {
            if right {
                let g = hi + 1;
                if out_of_reach(self.keys[g].saturating_sub(at), dir < 0, &best) {
                    right = false;
                } else {
                    self.offer_stretch(g, w, position, (heavy, own), fps, total, &mut best);
                    hi = g;
                    right = hi + 1 < self.keys.len();
                }
            }
            if left {
                let g = lo - 1;
                if out_of_reach(at.saturating_sub(self.end_of(g, total)), dir > 0, &best) {
                    left = false;
                } else {
                    self.offer_stretch(g, w, position, (heavy, own), fps, total, &mut best);
                    lo = g;
                    left = lo > 0;
                }
            }
        }
        best.map(|(_, j)| j)
    }

    /// For [`Self::next_job`]: what stretch `g` has for decoder `w` (its
    /// keyframe alone, or the stretch), if it's more pressing than `best`
    /// and it would be kept (`heavy`, `own`: see [`Self::missing_of`]).
    #[allow(clippy::too_many_arguments)]
    fn offer_stretch(&self, g: usize, w: usize, position: Option<u64>, (heavy, own): (Weighed, Weighed), fps: f64, total: u64, best: &mut Option<(f64, Job)>) {
        if self.dead[g] || self.jobs.iter().enumerate().any(|(k, j)| k != w && *j == Some(g)) {
            return;
        }
        let mut offer = |u: f64, j: Job| {
            if best.as_ref().is_none_or(|(b, _)| u < *b) {
                *best = Some((u, j));
            }
        };
        let (at, dir) = (self.at, self.dir(fps));
        let (key, end) = (self.keys[g], self.end_of(g, total));
        // Where the playhead gets into it, and whether it's left it behind.
        let (entry, behind) = if (key..=end).contains(&at) {
            (at, false)
        } else if end < at {
            (end, dir > 0)
        } else {
            (key, dir < 0)
        };
        let dist = at.abs_diff(entry) as f64 + if behind { 1e9 } else { 0.0 };
        if !self.frames.contains_key(&key) && self.fits_with(key, heavy, fps) && self.own_room(w, key, own, fps) {
            offer(dist * KEY_FIRST, Job::Key(g));
        }
        if let Some(m) = self.missing_of(g, dir, heavy, w, own, fps, total) {
            let mut u = at.abs_diff(m) as f64 + if behind { 1e9 } else { 0.0 };
            // Its decoder is in this stretch already: nothing to decode twice.
            if position.is_some_and(|p| p > key && p <= end) {
                u *= 0.5;
            }
            offer(u, Job::Stretch(g));
        }
    }

    /// Whether decoder `w` on stretch `g`, about to decode frame `next`,
    /// should leave it: the playhead has left it behind at speed, or
    /// passed `next` with no catching it up (going back, a frame past the
    /// playhead is behind it), or nothing more of it would be kept; or
    /// it's stopped working (another app decodes video).
    fn should_stop(&self, w: usize, g: usize, next: u64, fps: f64, total: u64) -> bool {
        let (key, end, at) = (self.keys[g], self.end_of(g, total), self.at);
        // Playing: the playback decoder has the frames.
        if next > end || self.resting || w >= self.working {
            return true;
        }
        let dir = self.dir(fps);
        if dir > 0 && (end < at || (next < at && self.pace() > 1.0)) {
            return true;
        }
        if dir < 0 && (key > at || next > at) {
            return true;
        }
        !self.fits_for(w, at.clamp(next, end), fps)
    }

    /// Whether a stretch nearer the playhead than `g` (the one it's in or
    /// the next two its way; either way, standing still) has frames that
    /// decoder `w` would decode in time and keep, and no decoder on it.
    fn more_pressing(&self, w: usize, g: usize, fps: f64, total: u64) -> bool {
        let dir = self.dir(fps);
        let h = self.stretch_of(self.at) as i64;
        let near: [i64; 3] = match dir {
            1 => [h, h + 1, h + 2],
            -1 => [h, h - 1, h - 2],
            _ => [h, h - 1, h + 1],
        };
        let heavy = self.heaviest(fps);
        let own = self.own_heaviest(w, fps);
        for n in near {
            let Some(n) = usize::try_from(n).ok().filter(|&n| n < self.keys.len()) else { continue };
            if n == g {
                return false;
            }
            if !self.dead[n] && !self.jobs.contains(&Some(n)) && self.missing_of(n, dir, heavy, w, own, fps, total).is_some() {
                return true;
            }
        }
        false
    }

    /// Frame `i` just came in: whether the UI should redraw (while
    /// scrubbing): it's nearer the playhead than what's on screen (held
    /// still: it's the playhead's own). Asked under the lock, the redraw
    /// requested after it's let go.
    fn worth_a_redraw(&self, i: u64) -> bool {
        self.scrubbing && if self.quiet { i == self.at } else { self.shown.is_none_or(|s| i.abs_diff(self.at) < s.abs_diff(self.at)) }
    }
}

/// The scrub engine for one clip: see the module's doc.
pub struct Engine {
    shared: Arc<Shared>,
}

impl Engine {
    /// Start decoding `path` (frames numbered at `fps`, `total` of them);
    /// none if the renderer can't draw decoded frames where they are.
    pub fn open(ctx: &egui::Context, path: &Path, fps: f64, total: u64) -> Option<Self> {
        let gpu = Gpu::find()?;
        let n = decoders(&gpu);
        let budget = budget(&gpu);
        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner {
                keys: Vec::new(),
                cap: 0,
                idle_cap: 0,
                shrink: false,
                at: 0,
                speed: 0.0,
                scrubbing: false,
                resting: false,
                quiet: false,
                // A 1440p P-frame on one thread takes ~6 ms; measured as it goes.
                frame_secs: 1.0 / 150.0,
                frames: BTreeMap::new(),
                jobs: vec![None; n],
                // Windows: one, till it's known no other app is decoding video.
                working: if cfg!(windows) { 1 } else { n },
                limits: vec![None; n],
                held: vec![0; n],
                dead: Vec::new(),
                shown: None,
                stats: Stats::default(),
            }),
            wake: Condvar::new(),
            #[cfg(target_os = "linux")]
            spare: Mutex::new(Vec::new()),
            gpu,
            stop: AtomicBool::new(false),
            failed: AtomicUsize::new(0),
            workers: n,
            ctx: ctx.clone(),
            fps,
            total,
            budget,
        });
        for w in 0..n {
            let (shared, path) = (shared.clone(), path.to_path_buf());
            std::thread::Builder::new().name(format!("scrub decoder {w}")).spawn(move || work(shared, path, w)).expect("spawn a scrub decoder");
        }
        #[cfg(windows)]
        {
            let shared = shared.clone();
            std::thread::Builder::new().name("scrub watcher".into()).spawn(move || watch_others(shared)).expect("spawn the scrub watcher");
        }
        Some(Self { shared })
    }

    /// No decoder could open the clip (or they all broke).
    pub fn failed(&self) -> bool {
        self.shared.failed.load(Ordering::Relaxed) >= self.shared.workers
    }

    /// The playhead is being scrubbed: it's at frame `at`, moving `speed`
    /// frames a second (below zero going back).
    /// `held`: it's held still (or was just put there): the UI is woken
    /// only for its own frame, not each one nearer.
    pub fn scrub_to(&self, at: u64, speed: f64, held: bool) {
        let fps = self.shared.fps;
        let mut s = self.shared.inner.lock().unwrap();
        let before = (s.at, s.dir(fps), s.scrubbing, s.resting);
        s.at = at;
        s.speed = speed;
        s.scrubbing = true;
        s.resting = false;
        s.quiet = held;
        let changed = before != (s.at, s.dir(fps), s.scrubbing, s.resting);
        if crate::player::debug() {
            self.log(&mut s);
        }
        drop(s);
        // Idle decoders woken on every redraw found the same nothing again,
        // each holding the lock while it looked.
        if changed {
            self.shared.wake.notify_all();
        }
    }

    /// Not scrubbing, at frame `at`: frames around it are decoded while
    /// idle, so a drag from here has them in hand.
    pub fn idle_at(&self, at: u64) {
        let mut s = self.shared.inner.lock().unwrap();
        if s.at == at && !s.scrubbing && !s.resting && s.speed == 0.0 {
            return;
        }
        s.shrink |= s.scrubbing;
        s.at = at;
        s.speed = 0.0;
        s.scrubbing = false;
        s.resting = false;
        drop(s);
        self.shared.wake.notify_all();
    }

    /// Playing, at frame `at`: decode nothing (the playback decoder has
    /// the frames) until the playhead stops somewhere.
    pub fn rest(&self, at: u64) {
        let mut s = self.shared.inner.lock().unwrap();
        let shrink = s.scrubbing;
        s.shrink |= shrink;
        s.at = at;
        s.speed = 0.0;
        s.scrubbing = false;
        s.resting = true;
        drop(s);
        // An idle decoder lets go of what a scrub kept.
        if shrink {
            self.shared.wake.notify_one();
        }
    }

    /// A frame decoded for playback: kept, if it's near enough, for a
    /// scrub back over what just played (not on Windows: [`KEEPS_PLAYBACK`]).
    /// (What it shows up to isn't known here: frames between those offered
    /// may have been skipped.)
    pub fn offer(&self, pic: Picture) {
        if !KEEPS_PLAYBACK {
            return;
        }
        // Linux, decoded on the card: in a buffer of playback's own few
        // dozen, which it would run short of (as on Windows).
        #[cfg(target_os = "linux")]
        if pic.gpu.as_ref().is_some_and(|g| matches!(g.nv12, capture::decode::Nv12::Card(_))) {
            return;
        }
        let mut s = self.shared.inner.lock().unwrap();
        if s.cap == 0 || !s.fits(pic.index, self.shared.fps) {
            return;
        }
        let out = s.store(pic.index, pic, None, self.shared.fps);
        drop(s);
        self.shared.let_go(out);
    }

    /// The picture to show at frame `at`: its own (exact), else the
    /// nearest decoded within reach, else nothing (keep what's shown).
    pub fn frame_for(&self, at: u64) -> Option<(Picture, bool)> {
        let mut s = self.shared.inner.lock().unwrap();
        let found = s.nearest(at, self.shared.fps);
        s.shown = found.as_ref().map(|(p, _)| p.index);
        found
    }

    /// Debug log, once a second while scrubbing: what the cache holds and
    /// how the decoders are doing.
    fn log(&self, s: &mut Inner) {
        let since = *s.stats.since.get_or_insert_with(Instant::now);
        if since.elapsed() < Duration::from_secs(1) {
            return;
        }
        let secs = since.elapsed().as_secs_f64();
        let keys = s.keys.iter().filter(|k| s.frames.contains_key(k)).count();
        let busy = s.jobs.iter().filter(|j| j.is_some()).count();
        let bytes = s.frames.values().next().map_or(0, |e| frame_bytes(&e.pic)) * s.frames.len() as u64;
        let (lo, hi) = (s.frames.keys().next().copied(), s.frames.keys().next_back().copied());
        eprintln!(
            "{:>8.3} scrub: {} frames cached ({} MB, cap {}) {:?}..{:?}, {:.0} decoded/s, {:.0} dropped/s, {busy} of {} decoders busy at {:.1} ms a frame, {keys} of {} keyframes, playhead {} at {:.0} frames/s",
            crate::player::uptime(),
            s.frames.len(),
            bytes >> 20,
            s.cap,
            lo,
            hi,
            s.stats.decoded as f64 / secs,
            s.stats.evicted as f64 / secs,
            self.shared.workers - self.shared.failed.load(Ordering::Relaxed),
            s.frame_secs * 1000.0,
            s.keys.len(),
            s.at,
            s.speed,
        );
        s.stats = Stats { since: Some(Instant::now()), ..Stats::default() };
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared.wake.notify_all();
    }
}

/// Frames uploaded ahead of the playhead on a thread of their own, into a
/// ring of textures ([`crate::gpu_frames::Ring`]), so the UI finds the
/// one it wants uploaded already (uploading a 1440p frame costs the
/// thread doing it a millisecond or two, which on the UI thread halved
/// how often it drew). Which ones: what the engine would show at the
/// playhead now and where it will be at the next few redraws, going at
/// its speed ([`AHEAD`] of them). The slot of the frame farthest from the
/// playhead is written over, never the one the UI draws from or did last.
#[cfg(target_os = "linux")]
pub struct Uploader {
    shared: Arc<UpShared>,
    engine: Arc<Shared>,
}

/// Redraws ahead to upload for.
#[cfg(target_os = "linux")]
const AHEAD: u64 = 5;
/// Texture slots: two for what's drawn and was, the rest ahead.
#[cfg(target_os = "linux")]
const SLOTS: usize = 8;

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, PartialEq)]
enum Slot {
    Free,
    /// Written with this picture (its number, BT.601 or not), for the
    /// GPU to copy in.
    Writing(u64, bool),
    /// Holds this picture, BT.601 or not.
    Ready(u64, bool),
}

#[cfg(target_os = "linux")]
struct UpInner {
    slots: Vec<Slot>,
    /// The playhead: where, how fast (frames a second, below zero going
    /// back), and how long a redraw takes (s); whether it's scrubbed.
    at: u64,
    speed: f64,
    redraw: f64,
    active: bool,
    /// The slot the UI draws from, and the one before.
    shown: Option<usize>,
    before: Option<usize>,
    /// Debug: redraws that found their frame uploaded, that didn't, and
    /// uploads made, since `since`.
    hits: u64,
    misses: u64,
    uploads: u64,
    since: Option<Instant>,
}

#[cfg(target_os = "linux")]
struct UpShared {
    inner: Mutex<UpInner>,
    wake: Condvar,
    stop: AtomicBool,
    ring: crate::gpu_frames::Ring,
}

#[cfg(target_os = "linux")]
impl Uploader {
    /// For `engine`'s frames, `width`×`height`; none without the renderer.
    pub fn new(engine: &Engine, width: u32, height: u32) -> Option<Self> {
        let ring = crate::gpu_frames::Ring::new(width, height, SLOTS)?;
        let shared = Arc::new(UpShared {
            inner: Mutex::new(UpInner { slots: vec![Slot::Free; SLOTS], at: 0, speed: 0.0, redraw: 1.0 / 240.0, active: false, shown: None, before: None, hits: 0, misses: 0, uploads: 0, since: None }),
            wake: Condvar::new(),
            stop: AtomicBool::new(false),
            ring,
        });
        let (s, e) = (shared.clone(), engine.shared.clone());
        std::thread::Builder::new().name("scrub uploader".into()).spawn(move || upload_work(s, e)).expect("spawn the scrub uploader");
        Some(Self { shared, engine: engine.shared.clone() })
    }

    /// The frames are this size.
    pub fn size(&self) -> (u32, u32) {
        self.shared.ring.size()
    }

    /// The playhead is being scrubbed: at `at`, moving `speed` frames a
    /// second, a redraw every `redraw` seconds.
    pub fn steer(&self, at: u64, speed: f64, redraw: f64) {
        let mut s = self.shared.inner.lock().unwrap();
        self.flush(&mut s);
        s.at = at;
        s.speed = speed;
        s.redraw = redraw;
        s.active = true;
        if crate::player::debug() {
            let since = *s.since.get_or_insert_with(Instant::now);
            if since.elapsed() >= Duration::from_secs(1) {
                eprintln!("{:>8.3} uploader: {} redraws found their frame uploaded, {} didn't, {} uploads", crate::player::uptime(), s.hits, s.misses, s.uploads);
                (s.hits, s.misses, s.uploads, s.since) = (0, 0, 0, Some(Instant::now()));
            }
        }
        drop(s);
        self.shared.wake.notify_all();
    }

    /// Not scrubbing: nothing to upload ahead.
    pub fn rest(&self) {
        self.shared.inner.lock().unwrap().active = false;
    }

    /// The UI thread: the frames the uploader has written go to their
    /// slots now (the GPU copies them in before the UI's next frame).
    fn flush(&self, s: &mut UpInner) {
        for slot in self.shared.ring.flush() {
            if let Slot::Writing(i, bt601) = s.slots[slot] {
                s.slots[slot] = Slot::Ready(i, bt601);
            }
            self.shared.wake.notify_all();
        }
    }

    /// The texture holding picture `index`, ready to draw, if it's
    /// uploaded, else the nearest uploaded within `near` frames of it
    /// (half a redraw's travel: at speed, as good) that's nearer to it
    /// than `on_screen` is: the UI draws from it now. Its picture's number.
    pub fn find(&self, index: u64, near: u64, on_screen: Option<u64>) -> Option<(crate::gpu_frames::Shown, u64)> {
        let mut s = self.shared.inner.lock().unwrap();
        self.flush(&mut s);
        // (Leaving out only the picture on screen, the one beside the frame
        // asked could stand in for it even when the frame itself was there.)
        let current = on_screen.map(|i| i.abs_diff(index));
        let found = s
            .slots
            .iter()
            .enumerate()
            .filter_map(|(k, sl)| match sl {
                Slot::Ready(i, bt601) => {
                    let d = i.abs_diff(index);
                    (d == 0 || (d <= near && current.is_none_or(|c| d < c))).then_some((d, k, *i, *bt601))
                }
                _ => None,
            })
            .min_by_key(|&(d, ..)| d);
        let Some((_, k, i, bt601)) = found else {
            s.misses += 1;
            return None;
        };
        if s.shown != Some(k) {
            s.before = s.shown;
            s.shown = Some(k);
        }
        s.hits += 1;
        Some((self.shared.ring.shown(k, bt601), i))
    }
}

#[cfg(target_os = "linux")]
impl Drop for Uploader {
    fn drop(&mut self) {
        let _ = &self.engine;
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared.wake.notify_all();
    }
}

/// The uploader's thread.
#[cfg(target_os = "linux")]
fn upload_work(shared: Arc<UpShared>, engine: Arc<Shared>) {
    let fps = engine.fps;
    loop {
        if shared.stop.load(Ordering::Relaxed) {
            return;
        }
        let (at, speed, redraw, active) = {
            let s = shared.inner.lock().unwrap();
            (s.at, s.speed, s.redraw, s.active)
        };
        if !active {
            let s = shared.inner.lock().unwrap();
            let _ = shared.wake.wait_timeout(s, Duration::from_millis(100));
            continue;
        }
        // A buffer to write into: one the GPU has copied from already. The
        // UI's frames see to that; left waiting (nothing drawn), ask the GPU.
        if !shared.ring.can_fill() {
            let s = shared.inner.lock().unwrap();
            let timed_out = shared.wake.wait_timeout(s, Duration::from_millis(5)).unwrap().1.timed_out();
            if timed_out {
                shared.ring.poll();
            }
            continue;
        }
        // Where the playhead will be at the next redraws, and the pictures
        // the engine would show there (one each; standing still, the frames
        // either side).
        let step = (speed.abs() * redraw).round().max(1.0) as u64;
        let targets: Vec<u64> = (0..=AHEAD)
            .map(|k| {
                if speed > 0.0 {
                    at + k * step
                } else if speed < 0.0 {
                    at.saturating_sub(k * step)
                } else if k % 2 == 1 {
                    at + k.div_ceil(2)
                } else {
                    at.saturating_sub(k / 2)
                }
            })
            .collect();
        let mut wanted: Vec<Picture> = Vec::new();
        for t in targets {
            if let Some((p, _)) = engine.inner.lock().unwrap().nearest(t, fps) {
                if !wanted.iter().any(|q| q.index == p.index) {
                    wanted.push(p);
                }
            }
        }
        // The first of them not uploaded: into a free slot, else the slot
        // of the frame farthest from the playhead if that's farther than
        // this one (never the UI's).
        let job = {
            let mut s = shared.inner.lock().unwrap();
            let mut job = None;
            for p in &wanted {
                let have = s.slots.iter().any(|sl| matches!(sl, Slot::Ready(i, _) | Slot::Writing(i, _) if *i == p.index));
                if have {
                    continue;
                }
                let victim = s
                    .slots
                    .iter()
                    .enumerate()
                    .filter(|(k, _)| Some(*k) != s.shown && Some(*k) != s.before)
                    .filter_map(|(k, sl)| match sl {
                        Slot::Free => Some((k, u64::MAX)),
                        Slot::Ready(i, _) => Some((k, i.abs_diff(at))),
                        Slot::Writing(..) => None,
                    })
                    .max_by_key(|&(_, d)| d);
                let Some((k, d)) = victim else { break };
                if d <= p.index.abs_diff(at) {
                    break;
                }
                let bt601 = p.gpu.as_ref().is_some_and(|g| g.bt601);
                s.slots[k] = Slot::Writing(p.index, bt601);
                job = Some((k, p.clone()));
                break;
            }
            job
        };
        match job {
            Some((k, p)) => {
                let written = match p.gpu.as_ref().map(|g| &g.nv12) {
                    Some(capture::decode::Nv12::Memory(nv12)) => shared.ring.fill(k, nv12),
                    Some(capture::decode::Nv12::Card(f)) => shared.ring.fill_card(k, f.clone()),
                    None => false,
                };
                let mut s = shared.inner.lock().unwrap();
                if written {
                    // Ready once the UI has had it copied in (`Uploader::flush`).
                    s.uploads += 1;
                } else {
                    s.slots[k] = Slot::Free;
                }
            }
            None => {
                // Nothing to do until the playhead moves (each redraw that
                // moves it wakes this), or now and then for frames decoded
                // since. (Every 2 ms, it kept a core busy with nothing to do
                // while the playhead was held still.)
                let s = shared.inner.lock().unwrap();
                let _ = shared.wake.wait_timeout(s, Duration::from_millis(20));
            }
        }
    }
}

/// This thread's niceness (0 normal, 10 behind it): whether it took.
#[cfg(target_os = "linux")]
fn set_nice(n: i32) -> bool {
    // SAFETY: a plain scheduling call on this thread.
    unsafe { libc::setpriority(libc::PRIO_PROCESS, libc::gettid() as libc::id_t, n) == 0 }
}

/// Elsewhere the graphics card decodes: the thread itself does little.
#[cfg(not(target_os = "linux"))]
fn set_nice(_: i32) -> bool {
    false
}

/// Give the memory of frames let go of back to the system. The allocator
/// keeps freed blocks for reuse, and a scrub that ends lets hundreds of
/// megabytes of frames go at once.
fn trim_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: only hands free memory back; any thread may call it.
    unsafe {
        libc::malloc_trim(0);
    }
}

/// `HASTECLIP_DEBUG_SCRUB=1`: log what each decoder takes on and leaves.
fn debug_jobs() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("HASTECLIP_DEBUG_SCRUB").is_some())
}

/// Windows: another app keeping the card's video decoder at least this busy
/// (percent, all of them together) is decoding video.
#[cfg(windows)]
const OTHERS_DECODING: f64 = 0.5;

/// Windows: while the clip is open, whether another app is decoding video (a
/// browser playing one), read about once a second: while one is, only the
/// first decoder works. The card's video decoder takes turns between
/// everything decoding on it, whatever their priority: beside the three
/// decoders, busy all through a scrub, another app got a fifth of it, too
/// little for a big video, which froze and stuttered; beside one, about
/// half (see the `decode_share` example), and the one still decodes far
/// more frames than a scrub shows. It starts with one, till the first
/// reading (a fraction of a second) says no other app is decoding.
#[cfg(windows)]
fn watch_others(shared: Arc<Shared>) {
    let Some(mut others) = capture::win::gpu::GpuUse::video_decode() else {
        set_working(&shared, shared.workers);
        return;
    };
    let me = std::process::id();
    // Readings in a row with no other app decoding (none yet, so the
    // first quiet one is enough).
    let mut quiet = 1;
    let mut wait = Duration::from_millis(300);
    loop {
        // In short steps, to be gone soon after the clip closes.
        let until = Instant::now() + wait;
        while Instant::now() < until {
            if shared.stop.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        wait = Duration::from_secs(1);
        let busy = others.others_now(me);
        quiet = if busy > OTHERS_DECODING { 0 } else { quiet + 1 };
        // All of them again once the other app's been quiet for two readings:
        // a video that buffers or loops is no reason to switch back and forth.
        let n = if quiet >= 2 { shared.workers } else { 1 };
        if set_working(&shared, n) && crate::player::debug() {
            eprintln!("{:>8.3} scrub: other apps keep the video decoder {busy:.0}% busy: {n} of {} decoders work", crate::player::uptime(), shared.workers);
        }
    }
}

/// Let the first `n` decoders work (the others finish the frame they're on
/// and wait): whether that's a change.
#[cfg(windows)]
fn set_working(shared: &Shared, n: usize) -> bool {
    let mut s = shared.inner.lock().unwrap();
    if s.working == n {
        return false;
    }
    s.working = n;
    drop(s);
    // Those that may work now find their jobs.
    shared.wake.notify_all();
    true
}

/// A decoder's thread: its stretches, one after another.
fn work(shared: Arc<Shared>, path: PathBuf, w: usize) {
    let (fps, total) = (shared.fps, shared.total);
    // Decoding around a paused playhead, behind a game; scrubbing, as soon
    // as the UI (a game running, decoders behind it couldn't keep up at
    // 1×). Back up only where the system lets a thread raise its priority
    // again (`nice`: where it doesn't, they stay behind).
    let mut nice = 10;
    set_nice(nice);
    // Its share of the budget, where a decoder sets that aside as it opens
    // (Windows: its pool of textures).
    let share = shared.budget / shared.workers.max(1) as u64;
    let open = || Source::open(&path, fps, &shared.gpu, share);
    let mut src = match open() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("scrubbing: decoder {w} can't open the clip: {e:#}");
            shared.failed.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };
    {
        let mut s = shared.inner.lock().unwrap();
        let pool = src.dec.pool_size();
        s.limits[w] = pool.map(|n| n.saturating_sub(POOL_FREE).max(4));
        if s.keys.is_empty() {
            s.keys = src.dec.keyframes();
            s.dead = vec![false; s.keys.len()];
            let (sw, sh) = src.dec.source_size();
            let bytes = (sw as u64 * sh as u64 * 3 / 2).max(1);
            s.cap = cap(shared.budget, bytes, shared.workers, pool);
            s.idle_cap = if IDLE_FREES { (s.cap / 2).min((4.0 * fps) as usize).max(s.cap.min(8)) } else { s.cap };
        }
    }
    // The frame the decoder would give next, reading on, when that's known.
    let mut position: Option<u64> = None;
    // Stretches in a row it couldn't decode: after a few, it's opened afresh.
    let mut errors = 0;
    loop {
        let (job, key, end, read_on) = {
            let mut s = shared.inner.lock().unwrap();
            if shared.stop.load(Ordering::Relaxed) {
                return;
            }
            // A scrub ended: what it kept beyond the idle room goes, and its
            // memory back to the system.
            if s.shrink {
                s.shrink = false;
                let out = s.shrink_to_room(fps);
                drop(s);
                let let_go = !out.is_empty();
                shared.let_go(out);
                if let_go {
                    trim_memory();
                }
                continue;
            }
            match s.next_job(w, position, fps, total) {
                Some(job) => {
                    let g = match job {
                        Job::Key(g) | Job::Stretch(g) => g,
                    };
                    let (key, end) = (s.keys[g], s.end_of(g, total));
                    // On from where the decoder is when that's in the stretch
                    // and no frame before it that would be kept is missing.
                    // Weight grows away from the playhead, so the likeliest
                    // kept of the missing ones are those nearest it either
                    // way, and the first after the keyframe (looking at only
                    // that one, it read on away from the frames wanted).
                    let read_on = position.is_some_and(|p| {
                        p > key && p <= end && {
                            let (heavy, own) = (s.heaviest(fps), s.own_heaviest(w, fps));
                            let c = s.at.clamp(key, p - 1);
                            [s.missing_forward(key, p - 1), s.missing_backward(c, key), s.missing_forward(c, p - 1)]
                                .into_iter()
                                .flatten()
                                .all(|m| !(s.fits_with(m, heavy, fps) && s.own_room(w, m, own, fps)))
                        }
                    });
                    s.jobs[w] = Some(g);
                    let wanted = if s.scrubbing { 0 } else { 10 };
                    if wanted != nice && set_nice(wanted) {
                        nice = wanted;
                    }
                    if debug_jobs() {
                        let what = if matches!(job, Job::Key(_)) { "keyframe" } else { "stretch" };
                        let from = if read_on { format!("on from {}", position.unwrap_or(0)) } else { "from its keyframe".into() };
                        eprintln!("{:>8.3} scrub: decoder {w} takes {what} {key}..={end} {from} (playhead {} at {:.0}, {} cached)", crate::player::uptime(), s.at, s.speed, s.frames.len());
                    }
                    (job, key, end, read_on)
                }
                None => {
                    let _ = shared.wake.wait_timeout(s, Duration::from_millis(100));
                    continue;
                }
            }
        };
        let g = match job {
            Job::Key(g) | Job::Stretch(g) => g,
        };
        // The frame decoded before the next one: reading on, the one the
        // decoder gave last.
        let mut prev: Option<u64> = if read_on { position.map(|p| p - 1) } else { None };
        let mut started = Instant::now();
        let mut got = if read_on { src.dec.next() } else { src.dec.frame(key) };
        let mut n = 0u64;
        loop {
            let took = started.elapsed().as_secs_f64();
            let mut evicted = Vec::new();
            let (mut redraw, mut leave, mut broken) = (false, false, None);
            {
                let mut s = shared.inner.lock().unwrap();
                match got {
                    Err(e) => {
                        // A stretch that can't be decoded (a damaged frame):
                        // left alone, the decoder going on with the others.
                        // (Quitting instead, every decoder met the same
                        // stretch in turn, and scrubbing stopped working.)
                        s.dead[g] = true;
                        broken = Some(e);
                    }
                    Ok(None) => {
                        // The end of the clip; or nothing at all from a
                        // keyframe: a stretch to leave alone.
                        match prev {
                            Some(p) => s.cover(Some(p), u64::MAX),
                            None => s.dead[g] = true,
                        }
                        position = None;
                        leave = true;
                    }
                    // Its keyframe came out numbered as another: asked for again
                    // and again, it would never be had. Left alone.
                    Ok(Some(pic)) if n == 0 && !read_on && pic.index != key => {
                        eprintln!("scrubbing: frame {key} came out as frame {}: frames {key}..={end} left alone", pic.index);
                        s.dead[g] = true;
                        evicted.push(pic);
                        position = None;
                        leave = true;
                    }
                    Ok(Some(pic)) => {
                        errors = 0;
                        let i = pic.index;
                        s.frame_secs += (took - s.frame_secs) * 0.05;
                        s.cover(prev, i);
                        if s.fits_for(w, i, fps) {
                            evicted = s.store(i, pic, Some(w), fps);
                            redraw = s.worth_a_redraw(i);
                        } else {
                            evicted.push(pic);
                        }
                        prev = Some(i);
                        position = Some(i + 1);
                        n += 1;
                        let why = if matches!(job, Job::Key(_)) {
                            Some("its keyframe")
                        } else if i >= end {
                            Some("its end")
                        } else if shared.stop.load(Ordering::Relaxed) {
                            Some("closing")
                        } else if s.should_stop(w, g, i + 1, fps, total) {
                            Some("passed, or no more of it kept")
                        } else if n.is_multiple_of(LOOK) && s.more_pressing(w, g, fps, total) {
                            Some("a nearer stretch waits")
                        } else {
                            None
                        };
                        if let Some(why) = why {
                            if debug_jobs() {
                                eprintln!("{:>8.3} scrub: decoder {w} leaves {key}..={end} at {i} ({n} frames): {why} (playhead {} at {:.0})", crate::player::uptime(), s.at, s.speed);
                            }
                            leave = true;
                        }
                    }
                }
            }
            // Asked for with the lock let go: egui's own lock is taken.
            if redraw {
                shared.ctx.request_repaint();
            }
            // The frames that made room: their memory goes to the next ones,
            // this decoder's or any's.
            shared.let_go(evicted);
            if let Some(e) = broken {
                eprintln!("scrubbing: decoder {w}: frames {key}..={end} can't be decoded, left alone: {e:#}");
                position = None;
                errors += 1;
                if errors >= 3 {
                    errors = 0;
                    match open() {
                        Ok(d) => {
                            shared.inner.lock().unwrap().limits[w] = d.dec.pool_size().map(|n| n.saturating_sub(POOL_FREE).max(4));
                            src = d;
                        }
                        Err(e) => {
                            eprintln!("scrubbing: decoder {w} can't open the clip again: {e:#}");
                            shared.inner.lock().unwrap().jobs[w] = None;
                            shared.failed.fetch_add(1, Ordering::Relaxed);
                            return;
                        }
                    }
                }
                break;
            }
            if leave {
                break;
            }
            src.reuse(&shared);
            started = Instant::now();
            got = src.dec.next();
        }
        shared.inner.lock().unwrap().jobs[w] = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cache for a 6000-frame clip with a keyframe every 60, `workers`
    /// decoders and room for `cap` frames, scrubbed at frame `at`.
    fn cache(workers: usize, cap: usize, at: u64) -> Inner {
        Inner {
            keys: (0..100).map(|k| k * 60).collect(),
            cap,
            idle_cap: cap,
            shrink: false,
            at,
            speed: 0.0,
            scrubbing: true,
            resting: false,
            quiet: false,
            frame_secs: 1.0 / 150.0,
            frames: BTreeMap::new(),
            jobs: vec![None; workers],
            working: workers,
            limits: vec![None; workers],
            held: vec![0; workers],
            dead: vec![false; 100],
            shown: None,
            stats: Stats::default(),
        }
    }

    fn picture(index: u64) -> Picture {
        Picture { index, width: 64, height: 36, rgba: Vec::new(), gpu: None }
    }

    /// What each decoder's count says agrees with the frames kept.
    fn counts_agree(s: &Inner) {
        for d in 0..s.held.len() {
            let kept = s.frames.values().filter(|e| e.from == Some(d)).count();
            assert_eq!(s.held[d], kept, "decoder {d}'s count");
        }
    }

    /// A decoder whose pool is limited (Windows) never has more frames in
    /// the cache than its share, however much room the cache has: the
    /// farthest of its own go first, and a frame farther than all of them
    /// isn't kept. Another decoder's frames aren't touched.
    #[test]
    fn a_decoder_keeps_within_its_pool() {
        let fps = 60.0;
        let mut s = cache(2, 100, 1000);
        s.limits[0] = Some(5);
        for i in 1000..1010 {
            s.store(i, picture(i), Some(0), fps);
        }
        for i in 1100..1110 {
            s.store(i, picture(i), Some(1), fps);
        }
        assert_eq!(s.held[0], 5);
        let mine: Vec<u64> = s.frames.iter().filter(|(_, e)| e.from == Some(0)).map(|(&i, _)| i).collect();
        assert_eq!(mine, vec![1000, 1001, 1002, 1003, 1004], "the nearest five");
        assert_eq!(s.held[1], 10, "the other decoder's frames stay");
        // The playhead moves back: a nearer frame of its own takes the
        // farthest one's place.
        s.at = 990;
        let out = s.store(995, picture(995), Some(0), fps);
        assert_eq!(out.iter().map(|p| p.index).collect::<Vec<_>>(), vec![1004]);
        assert!(s.frames.contains_key(&995) && !s.frames.contains_key(&1004));
        assert_eq!(s.held[0], 5);
        counts_agree(&s);
    }

    /// Decoders working as [`work`] does, one job at a time each, for up to
    /// `rounds` rounds: how many frames they decoded, and whether they ran
    /// out of things to do (the playhead standing still, they should).
    fn run_decoders(s: &mut Inner, fps: f64, total: u64, rounds: usize) -> (u64, bool) {
        let mut decoded = 0;
        for _ in 0..rounds {
            let mut busy = false;
            for w in 0..s.jobs.len() {
                let Some(job) = s.next_job(w, None, fps, total) else { continue };
                busy = true;
                let g = match job {
                    Job::Key(g) | Job::Stretch(g) => g,
                };
                let (key, end) = (s.keys[g], s.end_of(g, total));
                s.jobs[w] = Some(g);
                let mut prev = None;
                for i in key..=end {
                    decoded += 1;
                    s.cover(prev, i);
                    if s.fits_for(w, i, fps) {
                        s.store(i, picture(i), Some(w), fps);
                    }
                    prev = Some(i);
                    if matches!(job, Job::Key(_)) || i >= end || s.should_stop(w, g, i + 1, fps, total) {
                        break;
                    }
                }
                s.jobs[w] = None;
            }
            if !busy {
                return (decoded, true);
            }
        }
        (decoded, false)
    }

    /// Paused (Windows: each decoder's pool limits what it keeps), the
    /// decoders fill the cache around the playhead and then stop: a decoder
    /// whose pool is full isn't given frames it couldn't keep. (It was: it
    /// decoded a stretch, kept none of it, and was given the same stretch
    /// again, the hardware decoder busy for as long as the player was open.)
    #[test]
    fn paused_decoders_settle() {
        let fps = 60.0;
        for at in [3030, 0, 3000, 5999] {
            let mut s = cache(3, 120, at);
            s.scrubbing = false;
            s.limits = vec![Some(40); 3];
            let (decoded, settled) = run_decoders(&mut s, fps, 6000, 200);
            assert!(settled, "at {at}: still decoding after {decoded} frames, {} kept", s.frames.len());
            assert!(decoded < 600, "at {at}: {decoded} frames decoded to keep {}", s.frames.len());
            assert!(s.frames.len() >= 100, "at {at}: the room's used ({} kept)", s.frames.len());
            assert!(s.frames.contains_key(&at), "at {at}: the playhead's own frame");
            counts_agree(&s);
        }
    }

    /// While another app decodes video (Windows), only the first decoder
    /// works: the others get nothing to do and leave the stretch they're
    /// on, and the one alone fills the cache around the playhead.
    #[test]
    fn only_working_decoders_decode() {
        let fps = 60.0;
        let mut s = cache(3, 120, 3030);
        s.scrubbing = false;
        s.working = 1;
        assert!(s.next_job(0, None, fps, 6000).is_some());
        assert!(s.next_job(1, None, fps, 6000).is_none() && s.next_job(2, None, fps, 6000).is_none());
        let g = s.stretch_of(3030);
        assert!(s.should_stop(1, g, 3031, fps, 6000) && !s.should_stop(0, g, 3031, fps, 6000));
        let (decoded, settled) = run_decoders(&mut s, fps, 6000, 400);
        assert!(settled, "still decoding after {decoded} frames");
        assert!(s.frames.contains_key(&3030), "the playhead's own frame");
        assert!(s.frames.values().all(|e| e.from == Some(0)), "only the first decoder's frames");
        counts_agree(&s);
    }

    /// Paused, the playhead put somewhere else (a click on the timeline):
    /// the frames around it take the places of those around where it was,
    /// and then the decoders stop again.
    #[test]
    fn paused_decoders_follow_a_jump() {
        let fps = 60.0;
        let mut s = cache(3, 120, 1000);
        s.scrubbing = false;
        s.limits = vec![Some(40); 3];
        assert!(run_decoders(&mut s, fps, 6000, 200).1);
        s.at = 4030;
        let (decoded, settled) = run_decoders(&mut s, fps, 6000, 200);
        assert!(settled, "still decoding after {decoded} frames");
        assert!(decoded < 600, "{decoded} frames decoded");
        assert!(s.frames.contains_key(&4030), "the playhead's own frame");
        let near = s.frames.keys().filter(|&&i| i.abs_diff(4030) <= 120).count();
        assert!(near >= 90, "{near} of {} kept near the playhead", s.frames.len());
        counts_agree(&s);
    }

    /// The counts follow every way a frame leaves or another comes: a frame
    /// decoded again (the one there stays, with its decoder), a full cache,
    /// shrinking when a scrub ends. Playback's frames count against no
    /// decoder.
    #[test]
    fn counts_follow_the_cache() {
        let fps = 60.0;
        let mut s = cache(3, 12, 500);
        for (k, i) in (490..510).enumerate() {
            s.store(i, picture(i), Some(k % 3), fps);
            counts_agree(&s);
        }
        assert_eq!(s.frames.len(), 12);
        // Another decoder's copy of a frame kept: let go, the first stays.
        let d = s.frames[&500].from.unwrap();
        let out = s.store(500, picture(500), Some((d + 1) % 3), fps);
        assert_eq!((out.len(), s.frames[&500].from), (1, Some(d)));
        counts_agree(&s);
        // Playback's, on a slot of its own.
        s.frames.remove(&501).and_then(|e| e.from).map(|d| s.held[d] -= 1);
        s.store(501, picture(501), None, fps);
        counts_agree(&s);
        assert!(s.frames[&501].from.is_none());
        // A scrub ends: down to the idle room.
        s.scrubbing = false;
        s.idle_cap = 4;
        let out = s.shrink_to_room(fps);
        assert_eq!(s.frames.len(), 4);
        assert_eq!(out.len(), 8);
        counts_agree(&s);
    }
}
