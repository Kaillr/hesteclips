//! Video decoding for playback, in-process: libavcodec's H.264 decoder
//! (`avdec.c`, opened at run time) fed straight from the file by our own
//! index (`mp4read`). One decoder stays open per clip, so playing, seeking
//! and stepping never start anything new: an ffmpeg process per play took
//! ~200 ms to show its first frame, and scrubbing had to make do with small
//! pictures built beforehand.
//!
//! Frames come out one of two ways ([`Output`]):
//! - **For the screen**: NV12 at the video's own size (`Picture::gpu`), which
//!   the app uploads and draws with its own filter: nothing scaled or
//!   compressed on the way. On NVIDIA's card ([`Decoder::open_card`]) they're
//!   decoded there (NVDEC), into buffers of the app's renderer ([`Card`]),
//!   and never come to the CPU: a tenth of the CPU time (a 1440p frame took
//!   11.5 ms of it, 6 cores at 540 fps).
//! - **As pixels** (thumbnails): RGBA at the size asked for, shrunk here.
//!
//! Decoding runs on a few threads at once (frame threading): a frame comes
//! out a few frames after it goes in, so the ones after it go in meanwhile.
//! Only the frames wanted are copied out; the ones decoded only as
//! references on the way are let go as they come.
//!
//! Frames are numbered by time from the start of the clip (as the editor
//! counts them), on the grid of the app's frame rate.

use std::collections::{BTreeMap, HashSet};
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result, bail, ensure};

use crate::mp4read::Index;

unsafe extern "C" {
    fn hcav_load(dir: *const c_char, err: *mut c_char, errlen: c_int) -> c_int;
    fn hcav_open(avcc: *const u8, avcc_len: c_int, threads: c_int, cuda: *mut c_void, err: *mut c_char, errlen: c_int) -> *mut c_void;
    fn hcav_send(d: *mut c_void, data: *const u8, size: c_int, pts: i64, key: c_int, code: *mut c_int) -> c_int;
    fn hcav_receive(d: *mut c_void, pts: *mut i64, code: *mut c_int) -> c_int;
    fn hcav_take_nv12(d: *mut c_void, out: *mut u8, width: c_int, height: c_int) -> c_int;
    fn hcav_frame_cuda(d: *mut c_void, y: *mut u64, y_pitch: *mut c_int, uv: *mut u64, uv_pitch: *mut c_int) -> c_int;
    fn hcav_download(d: *mut c_void) -> c_int;
    fn hcav_flush(d: *mut c_void);
    fn hcav_close(d: *mut c_void);
    fn hcav_error_text(code: c_int, out: *mut c_char, len: c_int);

    fn hccu_open(uuid: *const u8, err: *mut c_char, errlen: c_int) -> *mut c_void;
    fn hccu_context(c: *mut c_void) -> *mut c_void;
    fn hccu_import(c: *mut c_void, fd: c_int, alloc: u64, size: u64, ptr: *mut u64, mem: *mut *mut c_void, err: *mut c_char, errlen: c_int) -> c_int;
    fn hccu_release(c: *mut c_void, ptr: u64, mem: *mut c_void);
    fn hccu_copy_nv12(c: *mut c_void, y: u64, y_pitch: c_int, uv: u64, uv_pitch: c_int, dst: u64, dst_pitch: c_int, width: c_int, height: c_int, err: *mut c_char, errlen: c_int) -> c_int;
    fn hccu_close(c: *mut c_void);
}

const FRAME: c_int = 1;
const AGAIN: c_int = 0;
const END: c_int = -1;
const SENT: c_int = 2;

/// Bytes read from the file at a time (frames are stored one after another):
/// one read per few dozen frames, not per frame.
const BLOCK: usize = 4 << 20;

/// Decoding threads for playback: more decode faster, but each adds a frame
/// between a frame going in and coming out (a jump waits for those).
const THREADS: c_int = 6;

/// How decoded frames come out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    /// NV12 at the video's own size, in `Picture::gpu`.
    Screen,
    /// RGBA at this width (height keeps the aspect, even), in `Picture::rgba`.
    Rgba(u32),
}

/// A decoded picture: NV12 in `gpu` ([`Output::Screen`]), or opaque RGBA in
/// `rgba`, rows top to bottom, no padding ([`Output::Rgba`]).
#[derive(Clone)]
pub struct Picture {
    /// Frame number from the start of the clip.
    pub index: u64,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub gpu: Option<Surface>,
}

/// A decoded frame for the screen: studio-range NV12 (the luma, then Cb, Cr
/// pairs at half size), which the app draws.
#[derive(Clone)]
pub struct Surface {
    pub nv12: Nv12,
    /// BT.601 colours (else BT.709).
    pub bt601: bool,
}

/// Where a frame's NV12 is.
#[derive(Clone)]
pub enum Nv12 {
    /// In our memory, rows as wide as the frame: the app uploads it.
    Memory(Arc<Vec<u8>>),
    /// On the graphics card, in a buffer of the app's renderer.
    Card(CardFrame),
}

impl Nv12 {
    /// The bytes, where they're in our memory.
    pub fn memory(&self) -> Option<&[u8]> {
        match self {
            Self::Memory(b) => Some(b),
            Self::Card(_) => None,
        }
    }
}

/// Decoding on NVIDIA's card into the memory of the app's renderer: a CUDA
/// context on the renderer's card (`cuda.c`), its frame buffers taken in.
/// One for the app; each decoder has its buffers ([`Pool`]).
pub struct Card {
    device: wgpu::Device,
    cuda: *mut c_void,
}

// SAFETY: CUDA's context is made current for each call (`cuda.c`), from
// whichever thread; the device is wgpu's, which is Send and Sync.
unsafe impl Send for Card {}
unsafe impl Sync for Card {}

impl Card {
    /// For frames drawn by `device` (the app's renderer, on Vulkan): where
    /// it's an NVIDIA card whose memory CUDA can share.
    pub fn new(device: &wgpu::Device) -> Result<Arc<Self>> {
        load()?;
        let uuid = super::gpu::uuid_of(device).context("the renderer isn't on Vulkan")?;
        let mut err = [0 as c_char; 512];
        // SAFETY: a 16-byte UUID and an error buffer.
        let cuda = unsafe { hccu_open(uuid.as_ptr(), err.as_mut_ptr(), err.len() as c_int) };
        ensure!(!cuda.is_null(), "{}", text(&err));
        let card = Arc::new(Self { device: device.clone(), cuda });
        // A buffer shared and taken in once, to know it works.
        Slot::new(&card, 1 << 16).context("the renderer's memory can't be shared with CUDA")?;
        Ok(card)
    }
}

impl Card {
    /// The card's own memory, in bytes (its largest heap of it).
    pub fn memory(&self) -> Option<u64> {
        use wgpu::hal::api::Vulkan;
        // SAFETY: only read from.
        let hal = unsafe { self.device.as_hal::<Vulkan>() }?;
        // SAFETY: a valid physical device.
        let props = unsafe { hal.shared_instance().raw_instance().get_physical_device_memory_properties(hal.raw_physical_device()) };
        props.memory_heaps_as_slice().iter().filter(|h| h.flags.contains(ash::vk::MemoryHeapFlags::DEVICE_LOCAL)).map(|h| h.size).max()
    }
}

impl Drop for Card {
    fn drop(&mut self) {
        // SAFETY: ours, opened once; every buffer has been let go of (each
        // holds the card).
        unsafe { hccu_close(self.cuda) };
    }
}

/// A frame buffer: the renderer's memory, which CUDA writes into.
struct Slot {
    card: Arc<Card>,
    buffer: wgpu::Buffer,
    /// Where CUDA has it, and its hold on it.
    at: u64,
    mem: *mut c_void,
}

// SAFETY: as `Card`; the memory handle is only let go of on drop.
unsafe impl Send for Slot {}
unsafe impl Sync for Slot {}

impl Slot {
    fn new(card: &Arc<Card>, bytes: u64) -> Result<Self> {
        let (buffer, fd, allocated) = super::gpu::shared_buffer(&card.device, bytes)?;
        let (mut at, mut mem, mut err) = (0u64, std::ptr::null_mut(), [0 as c_char; 512]);
        // SAFETY: the fd is handed over; valid out-parameters and buffer.
        let r = unsafe { hccu_import(card.cuda, std::os::fd::IntoRawFd::into_raw_fd(fd), allocated, bytes, &mut at, &mut mem, err.as_mut_ptr(), err.len() as c_int) };
        ensure!(r == 0, "{}", text(&err));
        Ok(Self { card: card.clone(), buffer, at, mem })
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        // SAFETY: what `hccu_import` took in, let go of once; the buffer
        // (and its memory) goes after.
        unsafe { hccu_release(self.card.cuda, self.at, self.mem) };
    }
}

/// A decoder's frame buffers on the card, made as they're needed (up to
/// `most`), each back for the next frame once nothing holds the frame in it.
struct Pool {
    card: Arc<Card>,
    /// The frames' size, and bytes from one row to the next (as the
    /// renderer copies rows).
    size: (u32, u32),
    pitch: u32,
    most: usize,
    slots: Mutex<Vec<Arc<Slot>>>,
    free: Mutex<Vec<usize>>,
}

/// Frame buffers a decoder makes at most (a 1440p frame is 5.5 MB): enough
/// for playback's frames ahead and on screen. Past them, frames come to our
/// memory.
const CARD_SLOTS: usize = 48;

/// Frame threads for playback on the card: the next frames are read while
/// the card decodes one (one thread at a time left it waiting). 1440p, an
/// RTX 4060 Ti: 510-520 fps on one, 600 on three, no more on more.
const CARD_THREADS: c_int = 3;

impl Pool {
    fn new(card: &Arc<Card>, (w, h): (u32, u32), most: usize) -> Self {
        Self { card: card.clone(), size: (w, h), pitch: w.next_multiple_of(256), most, slots: Mutex::new(Vec::new()), free: Mutex::new(Vec::new()) }
    }

    /// A frame's bytes: the luma's rows and the pairs' rows, at `pitch`.
    fn bytes(&self) -> u64 {
        let h = self.size.1;
        self.pitch as u64 * (h as u64 + h.div_ceil(2) as u64).max(1)
    }

    /// A free buffer, made if there's room for one.
    fn take(self: &Arc<Self>) -> Option<CardFrame> {
        let free = self.free.lock().unwrap().pop();
        let (index, slot) = match free {
            Some(i) => (i, self.slots.lock().unwrap()[i].clone()),
            None => {
                let mut slots = self.slots.lock().unwrap();
                if slots.len() >= self.most {
                    return None;
                }
                let slot = Arc::new(Slot::new(&self.card, self.bytes()).inspect_err(|e| eprintln!("decoding: a frame buffer on the card: {e:#}")).ok()?);
                slots.push(slot.clone());
                (slots.len() - 1, slot)
            }
        };
        Some(CardFrame(Arc::new(Held { pool: self.clone(), index, slot })))
    }
}

/// A frame in one of a decoder's buffers on the card. Copies share it; the
/// buffer goes back to the decoder when the last one is dropped.
#[derive(Clone)]
pub struct CardFrame(Arc<Held>);

struct Held {
    pool: Arc<Pool>,
    index: usize,
    slot: Arc<Slot>,
}

impl Drop for Held {
    fn drop(&mut self) {
        self.pool.free.lock().unwrap().push(self.index);
    }
}

impl CardFrame {
    /// The buffer, on the renderer's device: the luma's rows, then the U, V
    /// pairs' rows at half the size each way, [`Self::pitch`] bytes apart.
    pub fn buffer(&self) -> &wgpu::Buffer {
        &self.0.slot.buffer
    }

    /// Bytes from one row to the next (a multiple of 256, as wgpu copies).
    pub fn pitch(&self) -> u32 {
        self.0.pool.pitch
    }

    /// Where the U, V pairs' rows start.
    pub fn chroma_at(&self) -> u64 {
        self.0.pool.pitch as u64 * self.0.pool.size.1 as u64
    }

    /// Which buffer it is, unique while it's held (the pool, the buffer).
    pub fn key(&self) -> (usize, usize) {
        (Arc::as_ptr(&self.0.pool) as usize, self.0.index)
    }
}

/// Open libavcodec, once: from `lib/` beside the app (the release ships it
/// there), else the system's.
fn load() -> Result<()> {
    static LOADED: OnceLock<Result<(), String>> = OnceLock::new();
    LOADED
        .get_or_init(|| {
            let dir = std::env::current_exe().ok().and_then(|e| e.parent().map(|p| p.join("lib"))).unwrap_or_default();
            let dir = CString::new(dir.as_os_str().as_bytes()).unwrap_or_default();
            let mut err = [0 as c_char; 512];
            // SAFETY: valid strings and buffer; the library is opened once.
            match unsafe { hcav_load(dir.as_ptr(), err.as_mut_ptr(), err.len() as c_int) } {
                0 => Ok(()),
                _ => Err(text(&err)),
            }
        })
        .clone()
        .map_err(|e| anyhow::anyhow!(e))
}

fn text(buf: &[c_char]) -> String {
    // SAFETY: the C side always writes a terminated string into the buffer.
    unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned()
}

fn error_text(code: c_int) -> String {
    let mut buf = [0 as c_char; 128];
    // SAFETY: a valid buffer of that length.
    unsafe { hcav_error_text(code, buf.as_mut_ptr(), buf.len() as c_int) };
    text(&buf)
}

pub struct Decoder {
    dec: *mut c_void,
    index: Index,
    file: File,
    block: Vec<u8>,
    block_at: u64,
    /// Sample (decode order) of each keyframe, ascending.
    keys: Vec<usize>,
    /// Samples in presentation order (indices into `index.samples`).
    by_pts: Vec<usize>,
    /// Output size.
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    output: Output,
    bt601: bool,
    /// Where frames are decoded into on the card ([`Self::open_card`]).
    pool: Option<Arc<Pool>>,
    /// The video's own size.
    source: (u32, u32),
    /// The next sample (decode order) to hand the decoder.
    next_feed: usize,
    /// The end was sent (the last frames out): it starts over at a keyframe
    /// before decoding anything more.
    draining: bool,
    /// Frames decoded and kept, by presentation time.
    decoded: BTreeMap<i64, Kept>,
    /// Frames before this time aren't kept as they come out (they were
    /// decoded only as references on the way to it).
    keep_from: i64,
    /// Presentation time of the last frame handed out: frames up to it aren't
    /// kept.
    handed: i64,
    /// The next frame to hand out (position in `by_pts`), when reading on.
    next_out: Option<usize>,
    /// Frame buffers given back ([`Self::recycle`]), used again for the
    /// next frames out: no fresh memory to map and fault in per frame.
    spare: Vec<Vec<u8>>,
    /// Times of frames the decoder turned down (damaged) since it last
    /// started over: not waited for.
    rejected: HashSet<i64>,
}

/// A frame decoded and kept: in our memory, or on the card.
enum Kept {
    Memory(Vec<u8>),
    Card(CardFrame),
}

/// Buffers kept for reuse, at most.
const SPARE: usize = 4;

/// A frame not out this many frames after it went in never will be (H.264
/// holds back at most 16 for reordering, and frame threading a few more).
const GIVE_UP: usize = 48;

// The decoder is used from one thread at a time, which owns it.
unsafe impl Send for Decoder {}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: opened by `hcav_open`, closed once.
        unsafe { hcav_close(self.dec) };
    }
}

impl Decoder {
    /// Open `path`, frames coming out as `output` says.
    pub fn open(path: &Path, output: Output) -> Result<Self> {
        // Thumbnails are made in the background: two threads are plenty.
        Self::open_threads(path, output, if output == Output::Screen { THREADS } else { 2 })
    }

    /// [`Self::open`] on `threads` decoding threads. On one, a frame comes
    /// out as soon as it goes in (nothing waits on frames after it): for
    /// several decoders at once, each on its own stretch of the video.
    pub fn open_threads(path: &Path, output: Output, threads: c_int) -> Result<Self> {
        Self::open_with(path, output, threads, None)
    }

    /// Frames for the screen decoded on NVIDIA's card and kept there, in
    /// buffers of `card`'s renderer ([`Nv12::Card`]). (A frame the card's
    /// decoder can't take, or with no buffer free, comes to our memory.)
    pub fn open_card(path: &Path, card: &Arc<Card>) -> Result<Self> {
        Self::open_with(path, Output::Screen, CARD_THREADS, Some((card, CARD_SLOTS)))
    }

    /// [`Self::open_card`] with buffers for up to `bytes` of frames (the
    /// scrub engine's share of the card's memory), at least 8; on one
    /// thread, so each frame comes out as soon as it's decoded (a jump
    /// waits for nothing after it).
    pub fn open_card_pooled(path: &Path, card: &Arc<Card>, bytes: u64) -> Result<Self> {
        let mut dec = Self::open_with(path, Output::Screen, 1, Some((card, 8)))?;
        if let Some(pool) = &mut dec.pool {
            let frames = (bytes / Pool::new(card, pool.size, 0).bytes().max(1)) as usize;
            *pool = Arc::new(Pool::new(card, pool.size, frames.max(8)));
        }
        Ok(dec)
    }

    fn open_with(path: &Path, output: Output, threads: c_int, card: Option<(&Arc<Card>, usize)>) -> Result<Self> {
        load()?;
        let index = Index::read(path)?;
        ensure!(!index.samples.is_empty(), "no video frames");
        let keys = index.keyframes();
        ensure!(keys.first() == Some(&0), "the video doesn't start with a keyframe");
        let mut by_pts: Vec<usize> = (0..index.samples.len()).collect();
        by_pts.sort_by_key(|&i| index.samples[i].pts);
        let source = (index.width, index.height);
        let (width, height) = match output {
            Output::Screen => source,
            Output::Rgba(w) => {
                let w = w.min(source.0).max(2) / 2 * 2;
                (w, even_height(w, source))
            }
        };
        // BT.601 only when the file says so, as other players do.
        let bt601 = matches!(index.matrix, Some(5 | 6));
        let mut err = [0 as c_char; 512];
        // SAFETY: the setup bytes and the error buffer are valid for the call.
        // SAFETY: an open card's context.
        let cuda = card.map_or(std::ptr::null_mut(), |(c, _)| unsafe { hccu_context(c.cuda) });
        let dec = unsafe { hcav_open(index.avcc.as_ptr(), index.avcc.len() as c_int, threads, cuda, err.as_mut_ptr(), err.len() as c_int) };
        ensure!(!dec.is_null(), "{}", text(&err));
        // Frames span about one interval; the caller usually knows the rate better.
        let fps = {
            let s = &index.samples;
            let span = (s[by_pts[by_pts.len() - 1]].pts - s[by_pts[0]].pts) as f64 / index.timescale.max(1) as f64;
            if by_pts.len() > 1 && span > 0.0 { (by_pts.len() - 1) as f64 / span } else { 60.0 }
        };
        Ok(Self {
            dec,
            file: File::open(path)?,
            block: Vec::new(),
            block_at: 0,
            keys,
            by_pts,
            index,
            width,
            height,
            fps,
            output,
            bt601,
            pool: card.map(|(c, most)| Arc::new(Pool::new(c, source, most))),
            source,
            next_feed: 0,
            draining: false,
            decoded: BTreeMap::new(),
            keep_from: i64::MIN,
            handed: i64::MIN,
            next_out: Some(0),
            spare: Vec::new(),
            rejected: HashSet::new(),
        })
    }

    /// A frame's buffer (as `Surface::nv12` holds it) no longer needed:
    /// the next frame decoded goes into it. Keeping frames by the hundred
    /// and dropping them as fast, every frame got fresh memory from the
    /// system and gave it back, which cost more than the decoding.
    pub fn recycle(&mut self, buf: Vec<u8>) {
        if self.spare.len() < SPARE && buf.len() == media_frame_bytes(self.source) as usize {
            self.spare.push(buf);
        }
    }

    /// Buffers it has for the next frames (up to 4 are kept).
    pub fn spare(&self) -> usize {
        self.spare.len()
    }

    /// Frames at `width` wide, as RGBA (thumbnails).
    pub fn open_rgba(path: &Path, width: u32) -> Result<Self> {
        Self::open(path, Output::Rgba(width))
    }

    /// The video's own size.
    pub fn source_size(&self) -> (u32, u32) {
        self.source
    }

    /// Whether its colours are BT.601's (else BT.709's).
    pub fn bt601(&self) -> bool {
        self.bt601
    }

    /// The keyframes' numbers, ascending.
    pub fn keyframes(&self) -> Vec<u64> {
        let mut k: Vec<u64> = self.keys.iter().map(|&i| self.index_of(self.pts_sec(i))).collect();
        k.sort_unstable();
        // Two on one slot (an uneven frame rate) count once.
        k.dedup();
        k
    }

    /// Number frames on this rate's grid instead of the file's own (to match
    /// how the app counts them, from ffprobe's average rate).
    pub fn set_fps(&mut self, fps: f64) {
        if fps > 0.0 {
            self.fps = fps;
        }
    }

    /// How many frames can be out at once, if limited: on the card, its
    /// buffers (in our memory, each is its own).
    pub fn pool_size(&self) -> Option<usize> {
        self.pool.as_ref().map(|p| p.most)
    }

    /// Frames kept for scrubbing back and forth: here full size, the
    /// decoder's own (nothing is scaled down), as many as were asked for (in
    /// frames of `width`, in `bytes`) that fit in a share of the memory.
    pub fn small_frames(&mut self, width: u32, bytes: u64) -> Result<usize> {
        let w = width.min(self.source.0).max(2);
        let asked = (bytes as f64 / (w as f64 * even_height(w, self.source) as f64 * 1.5)).round() as u64;
        let full = media_frame_bytes(self.source);
        // A sixteenth of the memory, from 256 MB to 2 GB: a 1440p frame is 5.5 MB.
        let budget = crate::total_memory().map_or(1 << 30, |m| (m / 16).clamp(256 << 20, 2 << 30));
        Ok(asked.min(budget / full).max(16) as usize)
    }

    /// [`Self::frame`] (frames kept scrubbing are full size here).
    pub fn frame_small(&mut self, index: u64) -> Result<Option<Picture>> {
        self.frame(index)
    }

    /// [`Self::next`] (frames kept scrubbing are full size here).
    pub fn next_small(&mut self) -> Result<Option<Picture>> {
        self.next()
    }

    /// Nothing to send: frames are finished when handed out.
    pub fn flush(&self) {}

    /// How frames are decoded, for the log.
    pub fn way(&self) -> &'static str {
        if self.pool.is_some() { "libavcodec on NVIDIA's card" } else { "libavcodec" }
    }

    /// The frame showing at `index`: the last one at or before it, so a
    /// dropped frame's slot shows the frame before the gap (as playback
    /// does). Decoded on from where the decoder is when that's close, else
    /// from the keyframe before it. `None` past the end.
    pub fn frame(&mut self, index: u64) -> Result<Option<Picture>> {
        // The last frame (presentation order) numbered at or before the slot,
        // numbered as frames are handed out (`index_of`); before the first
        // frame, the first one shows. (Cut at the slot's end in seconds
        // instead, a frame starting a hair before the next slot came out
        // for this one numbered as the next: in a recording with a dropped
        // frame or two, frame 0 came out as frame 1, and the scrub engine
        // asked for frame 0 again and again.)
        let pos = self.by_pts.partition_point(|&i| self.index_of(self.pts_sec(i)) <= index).saturating_sub(1);
        self.at(pos)
    }

    /// The frame after the last one handed out. `None` at the end.
    pub fn next(&mut self) -> Result<Option<Picture>> {
        let Some(pos) = self.next_out else { return Ok(None) };
        if pos >= self.by_pts.len() {
            return Ok(None);
        }
        self.at(pos)
    }

    /// Frame at `pos` in presentation order.
    fn at(&mut self, pos: usize) -> Result<Option<Picture>> {
        let sample = self.by_pts[pos];
        let pts = self.index.samples[sample].pts;
        if let Some(buf) = self.decoded.remove(&pts) {
            return self.finish(pos, buf).map(Some);
        }
        // Sent and still to come out (kept when it does): on its way.
        let on_its_way = sample < self.next_feed && !self.draining && pts > self.handed && pts >= self.keep_from;
        if !on_its_way {
            // Decoding on reaches it unless it's past the next keyframe (or behind).
            let key = self.keys[self.keys.partition_point(|&k| k <= sample) - 1];
            if self.draining || !(key <= self.next_feed && self.next_feed <= sample) {
                self.reset();
                self.next_feed = key;
            }
            self.keep_from = pts;
        }
        loop {
            self.collect()?;
            if let Some(buf) = self.decoded.remove(&pts) {
                return self.finish(pos, buf).map(Some);
            }
            // Never coming (the decoder turned it down, or it's long overdue):
            // its slot shows the next frame that came out, as for a dropped
            // frame. (Waiting for it, every frame up to the end of the file
            // was decoded and kept, gigabytes of them.)
            if self.rejected.contains(&pts) || self.next_feed > sample + GIVE_UP {
                if let Some((&later, _)) = self.decoded.range(pts..).next() {
                    let buf = self.decoded.remove(&later).expect("just found");
                    let at = self.by_pts.partition_point(|&i| self.index.samples[i].pts < later);
                    return self.finish(at, buf).map(Some);
                }
            }
            if self.next_feed < self.index.samples.len() && !self.draining {
                self.feed(self.next_feed)?;
                self.next_feed += 1;
            } else if !self.draining {
                // The end of the file: the last frames come out.
                let mut code = 0;
                // SAFETY: a null packet asks for the end.
                unsafe { hcav_send(self.dec, std::ptr::null(), 0, 0, 0, &mut code) };
                self.draining = true;
            } else {
                bail!("the decoder gave no frame for {pts}");
            }
        }
    }

    /// Hand out frame `pos`.
    fn finish(&mut self, pos: usize, kept: Kept) -> Result<Picture> {
        self.next_out = Some(pos + 1);
        // Forget decoded frames shown before it (a jump back leaves some).
        let pts = self.index.samples[self.by_pts[pos]].pts;
        self.handed = pts;
        self.decoded.retain(|&t, _| t > pts);
        let index = self.index_of(self.pts_sec(self.by_pts[pos]));
        Ok(match (self.output, kept) {
            (Output::Screen, kept) => {
                let nv12 = match kept {
                    Kept::Memory(buf) => Nv12::Memory(Arc::new(buf)),
                    Kept::Card(f) => Nv12::Card(f),
                };
                Picture { index, width: self.width, height: self.height, rgba: Vec::new(), gpu: Some(Surface { nv12, bt601: self.bt601 }) }
            }
            (Output::Rgba(_), Kept::Memory(buf)) => {
                let rgba = nv12_to_rgba(&buf, self.source, (self.width, self.height), self.bt601);
                Picture { index, width: self.width, height: self.height, rgba, gpu: None }
            }
            (Output::Rgba(_), Kept::Card(_)) => bail!("a frame on the card for pixels"),
        })
    }

    /// Take out every frame that's ready: the ones wanted are kept, the rest
    /// (references on the way, frames passed) let go.
    fn collect(&mut self) -> Result<()> {
        loop {
            let (mut pts, mut code) = (0i64, 0);
            // SAFETY: the decoder is open; the out-parameters are valid.
            match unsafe { hcav_receive(self.dec, &mut pts, &mut code) } {
                FRAME if pts > self.handed && pts >= self.keep_from => {
                    let kept = match self.on_card()? {
                        Some(f) => Kept::Card(f),
                        None => Kept::Memory(self.take_frame()?),
                    };
                    self.decoded.insert(pts, kept);
                }
                FRAME => {}
                AGAIN | END => return Ok(()),
                _ => bail!("the decoder failed: {}", error_text(code)),
            }
        }
    }

    /// The frame just out, where it is on the card, copied into a buffer of
    /// the renderer's: `None` if it isn't there as NV12, or no buffer is free.
    fn on_card(&mut self) -> Result<Option<CardFrame>> {
        let Some(pool) = &self.pool else { return Ok(None) };
        let (mut y, mut y_pitch, mut uv, mut uv_pitch) = (0u64, 0, 0u64, 0);
        // SAFETY: the decoder is open, holding the frame; valid out-parameters.
        if unsafe { hcav_frame_cuda(self.dec, &mut y, &mut y_pitch, &mut uv, &mut uv_pitch) } != 0 {
            return Ok(None);
        }
        let Some(frame) = pool.take() else { return Ok(None) };
        let (w, h) = pool.size;
        let mut err = [0 as c_char; 512];
        // SAFETY: the frame stays put while it's held; the buffer is a whole
        // frame at its pitch, and no one reads it until this returns.
        let r = unsafe { hccu_copy_nv12(pool.card.cuda, y, y_pitch, uv, uv_pitch, frame.0.slot.at, pool.pitch as c_int, w as c_int, h as c_int, err.as_mut_ptr(), err.len() as c_int) };
        ensure!(r == 0, "{}", text(&err));
        Ok(Some(frame))
    }

    /// The frame just out, in our memory (copied down from the card if it's
    /// there): tight NV12 at the video's size, in a buffer given back if
    /// there is one.
    fn take_frame(&mut self) -> Result<Vec<u8>> {
        let (w, h) = self.source;
        // SAFETY: the decoder is open, holding the frame.
        ensure!(unsafe { hcav_download(self.dec) } == 0, "a decoded frame couldn't come down from the graphics card");
        // A buffer given back, else a new one (every byte of either is
        // written below).
        let mut buf = self.spare.pop().unwrap_or_else(|| vec![0u8; media_frame_bytes(self.source) as usize]);
        // SAFETY: `buf` holds a whole frame of that size.
        let r = unsafe { hcav_take_nv12(self.dec, buf.as_mut_ptr(), w as c_int, h as c_int) };
        ensure!(r == 0, "a decoded frame isn't {w}×{h}, 8-bit 4:2:0");
        Ok(buf)
    }

    /// Hand the decoder sample `i` (decode order).
    fn feed(&mut self, i: usize) -> Result<()> {
        let s = self.index.samples[i];
        let (start, end) = (s.offset, s.offset + s.size as u64);
        if start < self.block_at || end > self.block_at + self.block.len() as u64 {
            let file_len = self.file.metadata()?.len();
            let len = BLOCK.max(s.size as usize).min(file_len.saturating_sub(start) as usize).max(s.size as usize);
            self.block.resize(len, 0);
            self.file.seek(SeekFrom::Start(start))?;
            self.file.read_exact(&mut self.block).context("reading the video")?;
            self.block_at = start;
        }
        let at = (start - self.block_at) as usize;
        let data = self.block[at..at + s.size as usize].as_ptr();
        loop {
            let mut code = 0;
            // SAFETY: `data` points at the sample's bytes in `block`, which
            // nothing changes until this returns.
            match unsafe { hcav_send(self.dec, data, s.size as c_int, s.pts, s.key as c_int, &mut code) } {
                SENT => return Ok(()),
                // Full: take frames out, then again.
                AGAIN => self.collect()?,
                // A damaged frame: skipped (the ones after it show as best
                // they can, as in any player), and not waited for.
                _ => {
                    self.rejected.insert(s.pts);
                    return Ok(());
                }
            }
        }
    }

    /// Drop everything queued and decoded, to start again from a keyframe.
    fn reset(&mut self) {
        // SAFETY: the decoder is open.
        unsafe { hcav_flush(self.dec) };
        self.decoded.clear();
        self.rejected.clear();
        self.draining = false;
        self.keep_from = i64::MIN;
        self.handed = i64::MIN;
    }

    fn pts_sec(&self, sample: usize) -> f64 {
        self.index.samples[sample].pts as f64 / self.index.timescale.max(1) as f64
    }

    /// The frame whose slot on the grid contains `secs` from the first
    /// frame. Allow a thousandth of a frame: times in the file's units can
    /// read a hair early (else a frame takes the previous one's number).
    fn index_of(&self, secs: f64) -> u64 {
        ((secs - self.start()) * self.fps + 1e-3).floor().max(0.0) as u64
    }

    /// The first frame's time: frame 0.
    fn start(&self) -> f64 {
        self.pts_sec(self.by_pts[0])
    }
}

/// The bytes in an NV12 frame of `(width, height)`.
fn media_frame_bytes((w, h): (u32, u32)) -> u64 {
    w as u64 * h as u64 + 2 * w.div_ceil(2) as u64 * h.div_ceil(2) as u64
}

/// Height for `width` keeping `source`'s aspect, even.
fn even_height(width: u32, source: (u32, u32)) -> u32 {
    (((width as f64) * source.1 as f64 / source.0.max(1) as f64 / 2.0).round() as u32 * 2).max(2)
}

/// A studio-range NV12 frame of `source` size as opaque RGBA at `size`:
/// each pixel the average of the ones it covers.
fn nv12_to_rgba(nv12: &[u8], (sw, sh): (u32, u32), (w, h): (u32, u32), bt601: bool) -> Vec<u8> {
    let (sw, sh, w, h) = (sw as usize, sh as usize, w as usize, h as usize);
    let (cw, ch) = (sw.div_ceil(2), sh.div_ceil(2));
    let (luma, chroma) = nv12.split_at(sw * sh);
    // Each output pixel's span of source pixels, across and down.
    let span = |o: usize, n: usize, of: usize| {
        let a = o * of / n;
        (a, ((o + 1) * of / n).max(a + 1).min(of))
    };
    let (kr, kgb, kgr, kb) = if bt601 { (1.596, 0.392, 0.813, 2.017) } else { (1.793, 0.213, 0.533, 2.112) };
    let mut out = vec![255u8; w * h * 4];
    for oy in 0..h {
        let (y0, y1) = span(oy, h, sh);
        let (cy0, cy1) = span(oy, h, ch);
        for ox in 0..w {
            let (x0, x1) = span(ox, w, sw);
            let (cx0, cx1) = span(ox, w, cw);
            let mut sum = 0u32;
            for y in y0..y1 {
                sum += luma[y * sw + x0..y * sw + x1].iter().map(|&v| v as u32).sum::<u32>();
            }
            let yv = sum as f32 / ((y1 - y0) * (x1 - x0)) as f32;
            let (mut cb, mut cr) = (0u32, 0u32);
            for y in cy0..cy1 {
                for x in cx0..cx1 {
                    cb += chroma[(y * cw + x) * 2] as u32;
                    cr += chroma[(y * cw + x) * 2 + 1] as u32;
                }
            }
            let n = ((cy1 - cy0) * (cx1 - cx0)) as f32;
            let (cb, cr) = (cb as f32 / n - 128.0, cr as f32 / n - 128.0);
            let l = 1.164 * (yv - 16.0);
            let px = &mut out[(oy * w + ox) * 4..][..3];
            px[0] = (l + kr * cr).round().clamp(0.0, 255.0) as u8;
            px[1] = (l - kgb * cb - kgr * cr).round().clamp(0.0, 255.0) as u8;
            px[2] = (l + kb * cb).round().clamp(0.0, 255.0) as u8;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frames come out exactly as ffmpeg decodes them, wherever they're asked
    /// for: reading on, a jump back, a jump ahead, the last. A clip with
    /// B-frames (frames come out in another order than they go in) and a
    /// keyframe a second.
    #[test]
    fn frames_match_ffmpeg() {
        if let Err(e) = load() {
            eprintln!("skipped: {e:#}");
            return;
        }
        let dir = std::env::temp_dir().join(format!("hc-avdec-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("clip.mp4");
        let made = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=s=640x360:r=30:d=3"])
            .args(["-c:v", "libx264", "-g", "30", "-bf", "2", "-pix_fmt", "yuv420p"])
            .arg(&clip)
            .status()
            .unwrap();
        assert!(made.success());
        let ffmpeg_frame = |n: u64| -> Vec<u8> {
            let out = std::process::Command::new("ffmpeg")
                .args(["-v", "error", "-i"])
                .arg(&clip)
                .args(["-vf", &format!("select=eq(n\\,{n})"), "-frames:v", "1", "-pix_fmt", "nv12", "-f", "rawvideo", "-"])
                .output()
                .unwrap();
            out.stdout
        };
        let mut dec = Decoder::open(&clip, Output::Screen).unwrap();
        dec.set_fps(30.0);
        assert_eq!(dec.keyframes(), vec![0, 30, 60]);
        let mut check = |p: Option<Picture>, n: u64| {
            let p = p.unwrap_or_else(|| panic!("no frame {n}"));
            assert_eq!(p.index, n);
            assert!(p.gpu.as_ref().unwrap().nv12.memory() == Some(ffmpeg_frame(n).as_slice()), "frame {n} differs");
        };
        check(dec.frame(45).unwrap(), 45);
        check(dec.next().unwrap(), 46);
        check(dec.next().unwrap(), 47);
        check(dec.frame(10).unwrap(), 10);
        check(dec.frame(75).unwrap(), 75);
        check(dec.frame(89).unwrap(), 89);
        assert!(dec.next().unwrap().is_none(), "past the end");
        check(dec.frame(0).unwrap(), 0);
        let mut thumbs = Decoder::open_rgba(&clip, 160).unwrap();
        thumbs.set_fps(30.0);
        let t = thumbs.frame(20).unwrap().unwrap();
        assert_eq!((t.width, t.height, t.rgba.len()), (160, 90, 160 * 90 * 4));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A frame on the card read back, as tight NV12.
    fn read_back(device: &wgpu::Device, queue: &wgpu::Queue, f: &CardFrame, (w, h): (u32, u32)) -> Vec<u8> {
        let bytes = f.buffer().size();
        let read = device.create_buffer(&wgpu::BufferDescriptor { label: None, size: bytes, usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false });
        let mut enc = device.create_command_encoder(&Default::default());
        enc.copy_buffer_to_buffer(f.buffer(), 0, &read, 0, bytes);
        queue.submit([enc.finish()]);
        read.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        let _ = device.poll(wgpu::PollType::wait_indefinitely());
        let data = read.slice(..).get_mapped_range().unwrap();
        let pitch = f.pitch() as usize;
        let (w, h) = (w as usize, h as usize);
        let mut out = Vec::with_capacity(w * h * 3 / 2);
        for r in 0..h {
            out.extend_from_slice(&data[r * pitch..r * pitch + w]);
        }
        let chroma = f.chroma_at() as usize;
        for r in 0..h.div_ceil(2) {
            out.extend_from_slice(&data[chroma + r * pitch..chroma + r * pitch + w.div_ceil(2) * 2]);
        }
        out
    }

    /// Frames decoded on NVIDIA's card, into the renderer's buffers, are
    /// the CPU decoder's to the byte (H.264 decodes exactly), wherever
    /// they're asked for: reading on, jumps back and ahead, the last. A
    /// clip with B-frames, its height not a whole number of macroblocks.
    #[test]
    #[ignore = "needs NVIDIA's driver: cargo test -p capture card_frames -- --ignored"]
    fn card_frames_match_the_cpus() {
        if let Err(e) = load() {
            eprintln!("skipped: {e:#}");
            return;
        }
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor { backends: wgpu::Backends::VULKAN, ..wgpu::InstanceDescriptor::new_without_display_handle() });
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())).expect("a Vulkan card");
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).expect("a device");
        let card = match Card::new(&device) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("skipped: {e:#}");
                return;
            }
        };
        let dir = std::env::temp_dir().join(format!("hc-card-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("clip.mp4");
        let made = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=s=640x360:r=30:d=3"])
            .args(["-c:v", "libx264", "-g", "30", "-bf", "2", "-pix_fmt", "yuv420p"])
            .arg(&clip)
            .status()
            .unwrap();
        assert!(made.success());
        let mut on_card = Decoder::open_card(&clip, &card).unwrap();
        let mut on_cpu = Decoder::open(&clip, Output::Screen).unwrap();
        on_card.set_fps(30.0);
        on_cpu.set_fps(30.0);
        let size = (640, 360);
        let check = |a: Option<Picture>, b: Option<Picture>, n: u64| {
            let (a, b) = (a.unwrap_or_else(|| panic!("no frame {n} on the card")), b.unwrap_or_else(|| panic!("no frame {n}")));
            assert_eq!((a.index, b.index), (n, n));
            let Nv12::Card(f) = &a.gpu.as_ref().unwrap().nv12 else { panic!("frame {n} isn't on the card") };
            assert!(read_back(&device, &queue, f, size) == b.gpu.as_ref().unwrap().nv12.memory().unwrap(), "frame {n} differs");
        };
        for n in [45, 46, 47, 10, 75, 89, 0] {
            let (a, b) = if n == 46 || n == 47 { (on_card.next().unwrap(), on_cpu.next().unwrap()) } else { (on_card.frame(n).unwrap(), on_cpu.frame(n).unwrap()) };
            check(a, b, n);
        }
        assert!(on_card.next().unwrap().is_some_and(|p| p.index == 1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The CPU time this process has used, in seconds.
    fn cpu_seconds() -> f64 {
        // SAFETY: an output struct for the call.
        let mut u: libc::rusage = unsafe { std::mem::zeroed() };
        // SAFETY: as above.
        unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
        let t = |v: libc::timeval| v.tv_sec as f64 + v.tv_usec as f64 / 1e6;
        t(u.ru_utime) + t(u.ru_stime)
    }

    /// How fast frames come out, and the CPU time each takes, decoding on
    /// NVIDIA's card against on the CPU (as playback does, 6 threads).
    /// HC_CLIP: a clip.
    #[test]
    #[ignore = "benchmark: HC_CLIP=<mp4> cargo test --release -p capture card_decoding_speed -- --ignored --nocapture"]
    fn card_decoding_speed() {
        let clip = std::path::PathBuf::from(std::env::var("HC_CLIP").unwrap());
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor { backends: wgpu::Backends::VULKAN, ..wgpu::InstanceDescriptor::new_without_display_handle() });
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())).expect("a Vulkan card");
        let (device, _queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).expect("a device");
        let card = Card::new(&device).expect("NVIDIA's card");
        for on_card in [true, false] {
            let mut dec = if on_card { Decoder::open_card(&clip, &card).unwrap() } else { Decoder::open(&clip, Output::Screen).unwrap() };
            let (t, cpu) = (std::time::Instant::now(), cpu_seconds());
            let mut n = 0u64;
            while let Some(p) = dec.next().unwrap() {
                n += 1;
                drop(p);
                if n == 1800 {
                    break;
                }
            }
            let (secs, cpu) = (t.elapsed().as_secs_f64(), cpu_seconds() - cpu);
            eprintln!("{}: {n} frames, {:.0} fps, {:.2} ms of CPU a frame", if on_card { "on the card" } else { "on the CPU" }, n as f64 / secs, cpu / n as f64 * 1000.0);
        }
    }

    /// A real recording decodes as ffmpeg decodes it, at a few places, read
    /// on and jumped to: `HASTECLIP_TEST_CLIP=<mp4> cargo test -p capture
    /// real_clip -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs a recording"]
    fn real_clip_matches_ffmpeg() {
        let clip = std::path::PathBuf::from(std::env::var("HASTECLIP_TEST_CLIP").expect("HASTECLIP_TEST_CLIP"));
        let mut dec = Decoder::open(&clip, Output::Screen).unwrap();
        let fps = dec.fps;
        let ffmpeg_at = |i: u64| -> Vec<u8> {
            // ffmpeg's exact seek gives the first frame starting at or after it.
            let t = ((i as f64 - 0.25) / fps).max(0.0);
            std::process::Command::new("ffmpeg")
                .args(["-v", "error", "-ss", &format!("{t:.6}"), "-i"])
                .arg(&clip)
                .args(["-frames:v", "1", "-pix_fmt", "nv12", "-f", "rawvideo", "-"])
                .output()
                .unwrap()
                .stdout
        };
        let n = (dec.by_pts.len() as u64).saturating_sub(1);
        for i in [0, 1, 59, 60, 61, n / 2, n / 2 + 1, 30, n] {
            let started = std::time::Instant::now();
            let p = dec.frame(i).unwrap().unwrap();
            let took = started.elapsed();
            let same = p.gpu.as_ref().unwrap().nv12.memory() == Some(ffmpeg_at(i).as_slice());
            println!("frame {i}: {} in {:.1} ms", if same { "same as ffmpeg" } else { "DIFFERENT" }, took.as_secs_f64() * 1000.0);
            assert!(same, "frame {i} differs");
        }
    }

    /// A recording with a dropped frame in over a thousand runs a hair
    /// slower than its rate on average, which put frame 1 inside frame 0's
    /// slot as the slot was cut by time: frame(0) handed out frame 1, so
    /// frame 0 could never be had. Each slot's frame is now the one
    /// numbered for it.
    #[test]
    fn a_dropped_frame_leaves_every_frame_reachable() {
        if let Err(e) = load() {
            eprintln!("skipped: {e:#}");
            return;
        }
        let dir = std::env::temp_dir().join(format!("hc-avdec-drop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("dropped.mp4");
        // 20 s at 60 fps with frame 600 left out, its gap kept in the times
        // (one in 1200: the average rate is within a thousandth of 60).
        let made = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=s=64x36:r=60:d=20"])
            .args(["-vf", "select='not(eq(n\\,600))'", "-fps_mode", "passthrough"])
            .args(["-c:v", "libx264", "-preset", "ultrafast", "-g", "60", "-bf", "0", "-pix_fmt", "yuv420p"])
            .arg(&clip)
            .status()
            .unwrap();
        assert!(made.success());
        let mut dec = Decoder::open(&clip, Output::Screen).unwrap();
        assert!(dec.fps < 60.0, "the average rate is under 60 ({})", dec.fps);
        assert_eq!(dec.keyframes().first(), Some(&0));
        for i in [0, 1, 2, 60, 61, 599, 601, 1100] {
            let p = dec.frame(i).unwrap().unwrap();
            assert!(p.index <= i, "frame({i}) handed out frame {}", p.index);
        }
        assert_eq!(dec.frame(0).unwrap().unwrap().index, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A grey and a red come back as themselves, shrunk.
    #[test]
    fn nv12_shrinks_to_rgba() {
        let (sw, sh) = (8u32, 4u32);
        // Studio-range BT.709 for (200, 60, 40): Y 92, Cb 105, Cr 190.
        let mut nv12 = vec![92u8; (sw * sh) as usize];
        for _ in 0..(sw / 2 * sh / 2) {
            nv12.extend_from_slice(&[105, 190]);
        }
        let rgba = nv12_to_rgba(&nv12, (sw, sh), (4, 2), false);
        for px in rgba.chunks(4) {
            assert!((px[0] as i32 - 200).abs() <= 2 && (px[1] as i32 - 60).abs() <= 2 && (px[2] as i32 - 40).abs() <= 2, "{px:?}");
            assert_eq!(px[3], 255);
        }
    }
}
