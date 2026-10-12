//! Video decoding for playback, in-process: the GPU's hardware decoder fed
//! straight from the file (`direct`: H.264 MP4s, as recorded), else through
//! Media Foundation's source reader (anything else), then our own D3D11 video processor pass
//! that scales the NV12 frame and converts it to full-range RGBA. One reader
//! stays open per clip, so playing, seeking and stepping never start anything
//! new (an ffmpeg process per play took 120-500 ms to show its first frame;
//! this takes a few ms per frame in order).
//!
//! The source reader's own video processing ("advanced video processing")
//! isn't used: it re-times frames to an even rate (clip length / frame
//! count), so in a recording with dropped frames every timestamp drifts,
//! by 50 frames at 30 s in one real clip. The decoder's own output keeps the
//! file's timestamps.
//!
//! Frames either come back to the CPU as RGBA, or (`share_on`) stay on the
//! GPU in a pool of shareable textures that the app's renderer opens once and
//! draws directly: no copy back, no upload. Shared at the video's own size
//! they stay NV12 (copied, not converted; the renderer converts as it draws)
//! where the graphics card can share NV12.
//!
//! Feeding the decoder ourselves skips the reader's seek: a jump takes
//! 1.7-3× less time (a keyframe 15 ms instead of 45, with the picture's
//! conversion), reading in order about as fast (see `direct`).
//!
//! Frames are numbered by time from the start of the clip (as the editor and
//! the scrub proxy count them). Media Foundation shifts every timestamp by the
//! stream's reordering delay (B-frames), so the first frame's time is zero.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::{CloseHandle, GENERIC_ALL, HANDLE, RECT};
use windows::Win32::Graphics::Dxgi::{DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE, IDXGIResource1};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_TYPE, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601,
    DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_RATIONAL,
};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::core::{GUID, HSTRING, Interface};

use super::d3d::Gpu;
use super::system::{com_init, mf_startup};

const STREAM: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
/// Media Foundation's time unit: 100 ns.
const UNITS: f64 = 10_000_000.0;
/// Further ahead than this, seeking beats decoding forward: a seek costs
/// ~55 ms plus decoding from the keyframe (~1.6 ms a frame), so past ~60
/// frames (1 s at 60 fps) it's the cheaper way on average.
const MAX_SKIP: u64 = 60;

/// Shareable textures decoded frames go into (see [`Surfaces`]): as many as
/// fit in [`POOL_BYTES`], within these bounds. Frames are full resolution;
/// kept as the decoder gives them (NV12, 1.5 bytes a pixel) 24 at 4K are
/// ~300 MB. Converted to RGBA (4 bytes, where NV12 can't be shared) 24 at
/// 1440p are ~350 MB and 4K gets 16 (~530 MB).
const POOL_MIN: usize = 16;
const POOL_MAX: usize = 24;
const POOL_BYTES: u64 = 512 << 20;

/// A decoded picture, opaque RGBA: in `rgba` (rows top to bottom, no
/// padding), or on the GPU in `gpu` (and `rgba` is empty).
#[derive(Clone)]
pub struct Picture {
    /// Frame number from the start of the clip.
    pub index: u64,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub gpu: Option<Surface>,
}

/// A fixed set of shareable textures on the GPU. Each decoded frame takes
/// a free one and gives it back when its [`Surface`] is dropped; the one freed
/// longest ago is reused first, so a texture the screen may still be drawing
/// isn't overwritten.
pub struct Surfaces {
    /// Unique per pool, for caching what's been opened from it.
    pub id: usize,
    /// What the textures hold.
    pub format: SurfaceFormat,
    textures: Vec<ID3D11Texture2D>,
    handles: Vec<HANDLE>,
    free: Mutex<VecDeque<usize>>,
    /// Signalled after each frame is written, for the other device to wait
    /// on before reading it (with its NT handle). Without it the other API
    /// could draw a texture's previous contents (another, old frame).
    fence: Option<(ID3D11Fence, HANDLE)>,
    fence_value: std::sync::atomic::AtomicU64,
}
unsafe impl Send for Surfaces {}
unsafe impl Sync for Surfaces {}

impl Drop for Surfaces {
    fn drop(&mut self) {
        for h in self.handles.iter().chain(self.fence.as_ref().map(|(_, h)| h)) {
            unsafe {
                let _ = CloseHandle(*h);
            }
        }
    }
}

/// What a pool's textures hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceFormat {
    /// Full-range RGBA, converted.
    Rgba,
    /// The decoder's own picture: studio-range NV12, BT.601 colours or (false)
    /// BT.709. Drawing converts it.
    Nv12 { bt601: bool },
}

/// One texture of a [`Surfaces`] pool, holding a frame. Copies share it; it
/// goes back to the pool when the last one is dropped.
#[derive(Clone)]
pub struct Surface(Arc<Slot>);

struct Slot {
    pool: Arc<Surfaces>,
    slot: usize,
    /// The fence value signalled once this frame was written.
    ready: u64,
}

impl Surface {
    /// The pool and slot: the same texture every time, so open it once.
    pub fn key(&self) -> (usize, usize) {
        (self.0.pool.id, self.0.slot)
    }

    /// An NT handle to the texture, to open it on another device (D3D12).
    /// Owned by the pool; valid while it lives.
    pub fn handle(&self) -> isize {
        self.0.pool.handles[self.0.slot].0 as isize
    }

    /// What the texture holds.
    pub fn format(&self) -> SurfaceFormat {
        self.0.pool.format
    }

    /// The pool's shared fence (NT handle, owned by the pool) and the value
    /// that means this frame is written: wait for it before drawing it.
    /// `None` if the graphics card couldn't share a fence (then the frame was
    /// finished before it was handed over).
    pub fn fence(&self) -> Option<(isize, u64)> {
        self.0.pool.fence.as_ref().map(|(_, h)| (h.0 as isize, self.0.ready))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.pool.free.lock().unwrap().push_back(self.slot);
    }
}

/// Where decoded frames come from.
enum Source {
    /// Our own reading of the file, straight into the decoder.
    Direct(super::direct::Stream),
    Reader(IMFSourceReader),
}

pub struct Decoder {
    src: Source,
    _manager: IMFDXGIDeviceManager,
    gpu: Gpu,
    /// Output size.
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    /// The conversion to RGBA; none when frames stay NV12 (only copied).
    convert: Option<Convert>,
    /// Frames stay on the GPU, in these.
    pool: Option<Arc<Surfaces>>,
    /// The video's own size.
    source: (u32, u32),
    /// Its colours are BT.601 (else BT.709).
    bt601: bool,
    /// Media Foundation's time of frame 0.
    offset: i64,
    /// The frame the next read returns, when known (none after a seek).
    next_index: Option<u64>,
    /// A frame already read but not yet handed out.
    peeked: Option<(u64, IMFSample)>,
    /// Frames read in order whose pixels are on their way back from the
    /// GPU (see [`Self::next`]): their numbers and staging textures, oldest
    /// first, and the staging texture the next one goes to.
    coming: VecDeque<(u64, usize)>,
    next_staging: usize,
    /// Small frames for scrubbing (see [`Self::small_frames`]), once asked for.
    small: Option<(Arc<Surfaces>, Convert)>,
}

/// Frames read back at once while reading in order: the GPU converts and
/// copies the newest while the oldest is copied out (each waited for the GPU
/// on its own before).
const READ_AHEAD: usize = 3;

// The reader is used from one thread at a time, which owns the decoder.
unsafe impl Send for Decoder {}

impl Decoder {
    /// Open `path`, decoding to `width` wide (height keeps the aspect, even).
    /// With `share_on` (a graphics card's LUID), frames stay on that card in
    /// shareable textures instead of coming back as RGBA.
    ///
    /// Feeds the decoder directly when it can (see `direct`), else through the
    /// file reader; `HESTECLIPS_NO_DIRECT_DECODE=1` always uses the reader.
    pub fn open(path: &Path, width: u32, share_on: Option<u64>) -> Result<Self> {
        Self::open_with(path, width, share_on, std::env::var_os("HESTECLIPS_NO_DIRECT_DECODE").is_none())
    }

    /// The default graphics card's LUID (to share frames on it, as the app
    /// does with its renderer's; for benchmarks).
    pub fn default_luid() -> Option<u64> {
        com_init();
        Gpu::new().ok()?.adapter_luid()
    }

    /// Frames at `width` wide, back on the CPU as RGBA (thumbnails, scrub frames).
    pub fn open_rgba(path: &Path, width: u32) -> Result<Self> {
        Self::open(path, width, None)
    }

    /// [`Self::open`], feeding the decoder directly; an error if the file
    /// can't be read that way.
    pub fn open_direct(path: &Path, width: u32, share_on: Option<u64>) -> Result<Self> {
        let d = Self::open_with(path, width, share_on, true)?;
        if d.way() != "direct" {
            bail!("this file can't be decoded directly");
        }
        Ok(d)
    }

    /// [`Self::open`], always through Media Foundation's file reader (to
    /// compare with the direct way).
    pub fn open_file_reader(path: &Path, width: u32, share_on: Option<u64>) -> Result<Self> {
        Self::open_with(path, width, share_on, false)
    }

    /// Which way frames come: "direct" or "file reader".
    pub fn way(&self) -> &'static str {
        match self.src {
            Source::Direct(_) => "direct",
            Source::Reader(_) => "file reader",
        }
    }

    fn open_with(path: &Path, width: u32, share_on: Option<u64>, direct: bool) -> Result<Self> {
        com_init();
        mf_startup()?;
        let gpu = match share_on {
            Some(luid) => Gpu::on_adapter(luid)?,
            None => Gpu::new()?,
        };
        if share_on.is_some_and(|l| gpu.adapter_luid() != Some(l)) {
            bail!("that graphics card isn't available to share frames on");
        }
        unsafe {
            let mut token = 0u32;
            let mut manager = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
            let manager = manager.context("no device manager")?;
            manager.ResetDevice(&gpu.device, token)?;

            let direct = if direct {
                match super::direct::Stream::open(path, &manager) {
                    Ok(d) => Some(d),
                    Err(e) => {
                        if std::env::var_os("HESTECLIPS_DEBUG_VIDEO").is_some() {
                            eprintln!("decoder: through the file reader ({e:#})");
                        }
                        None
                    }
                }
            } else {
                None
            };
            let (src, (sw, sh), fps, bt601) = match direct {
                Some(d) => {
                    let size = (d.index.width, d.index.height);
                    let fps = d.fps();
                    // BT.601 only when the file says so, as the reader does.
                    let bt601 = matches!(d.index.matrix, Some(5 | 6));
                    (Source::Direct(d), size, fps, bt601)
                }
                None => {
                    let mut attrs = None;
                    MFCreateAttributes(&mut attrs, 3)?;
                    let attrs = attrs.context("no attributes")?;
                    attrs.SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, &manager)?;
                    attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
                    let reader = MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), &attrs).context("couldn't open the video")?;
                    reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
                    reader.SetStreamSelection(STREAM, true)?;

                    let native = reader.GetNativeMediaType(STREAM, 0)?;
                    let size = native.GetUINT64(&MF_MT_FRAME_SIZE)?;
                    let (sw, sh) = ((size >> 32) as u32, size as u32);
                    let rate = native.GetUINT64(&MF_MT_FRAME_RATE).unwrap_or(60 << 32 | 1);
                    let fps = (rate >> 32) as f64 / (rate as u32).max(1) as f64;
                    // SD video is usually BT.601; everything else BT.709.
                    let bt601 = native.GetUINT32(&MF_MT_YUV_MATRIX).ok() == Some(MFVideoTransferMatrix_BT601.0 as u32);

                    // The decoder's own output: NV12 textures on the GPU, with the file's times.
                    let out = MFCreateMediaType()?;
                    out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
                    out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
                    reader.SetCurrentMediaType(STREAM, None, &out).context("the video can't be decoded to NV12")?;
                    (Source::Reader(reader), (sw, sh), fps, bt601)
                }
            };
            let width = width.min(sw).max(2) / 2 * 2;
            let height = even_height(width, (sw, sh));

            let (pool, convert) = outputs(&gpu, share_on.is_some(), (sw, sh), (width, height), fps, bt601)?;
            let mut d = Self {
                src,
                _manager: manager,
                gpu,
                width,
                height,
                fps,
                convert,
                pool,
                source: (sw, sh),
                bt601,
                offset: 0,
                next_index: None,
                peeked: None,
                coming: VecDeque::new(),
                next_staging: 0,
                small: None,
            };
            // The first frame's time is frame 0's; keep the frame for the first read.
            let (time, sample) = d.read_sample()?.context("the video has no frames")?;
            d.offset = time;
            d.peeked = Some((0, sample));
            d.next_index = Some(0);
            Ok(d)
        }
    }

    /// How many frames can be out at once (on the GPU), if shared.
    pub fn pool_size(&self) -> Option<usize> {
        self.pool.as_ref().map(|p| p.textures.len())
    }

    /// The video's own size.
    pub fn source_size(&self) -> (u32, u32) {
        self.source
    }

    /// The keyframes' numbers, ascending: a jump to one is a single frame to
    /// decode. Empty through the file reader, which doesn't say.
    pub fn keyframes(&self) -> Vec<u64> {
        let Source::Direct(d) = &self.src else { return Vec::new() };
        let mut k: Vec<u64> = d.key_times().map(|t| self.index_of(t)).collect();
        k.sort_unstable();
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

    /// The frame showing at `index`: the last one at or before it, so a
    /// dropped frame's slot shows the frame before the gap (as the scrub proxy
    /// and playback do). Decoded forward from where the reader is when that's
    /// close, else from the keyframe before it. `None` past the end.
    pub fn frame(&mut self, index: u64) -> Result<Option<Picture>> {
        self.frame_as(index, false)
    }

    /// [`Self::frame`], as a small frame (see [`Self::small_frames`]); call
    /// [`Self::flush`] before drawing it.
    pub fn frame_small(&mut self, index: u64) -> Result<Option<Picture>> {
        self.frame_as(index, true)
    }

    /// Send the small frames made since the last flush to the GPU: before any
    /// of them is drawn (the renderer would wait for them forever).
    pub fn flush(&self) {
        unsafe { self.gpu.context.Flush() };
    }

    /// [`Self::next`], as a small frame (see [`Self::small_frames`]); call
    /// [`Self::flush`] before drawing it.
    pub fn next_small(&mut self) -> Result<Option<Picture>> {
        match self.read()? {
            Some((i, sample)) => self.picture_as(i, &sample, true).map(Some),
            None => Ok(None),
        }
    }

    /// Frames for scrubbing back and forth, kept a while: `width` wide (at
    /// most the video's), as many as fit in `bytes` (16 to 2048), in textures
    /// of their own so keeping them doesn't hold up playback's. NV12 where
    /// the graphics card shares it (the conversion scales it), else RGBA.
    /// How many there are; an error unless frames are shared.
    pub fn small_frames(&mut self, width: u32, bytes: u64) -> Result<usize> {
        if let Some((pool, _)) = &self.small {
            return Ok(pool.textures.len());
        }
        if self.pool.is_none() {
            bail!("frames aren't shared on the GPU");
        }
        let w = width.min(self.source.0).max(2) / 2 * 2;
        let size = (w, even_height(w, self.source));
        let make = |format: SurfaceFormat, bytes_per_pixel: f64, out: DXGI_COLOR_SPACE_TYPE| -> Result<(Arc<Surfaces>, Convert)> {
            let count = (bytes as f64 / (size.0 as f64 * size.1 as f64 * bytes_per_pixel)).clamp(16.0, 2048.0) as usize;
            let pool = surfaces(&self.gpu, size.0, size.1, format, count, true)?;
            let convert = Convert::new(&self.gpu, self.source, size, self.fps, self.bt601, out, pool.textures.clone())?;
            Ok((Arc::new(pool), convert))
        };
        let nv12 = (shares_nv12(&self.gpu) && std::env::var_os("HESTECLIPS_RGBA_FRAMES").is_none())
            .then(|| make(SurfaceFormat::Nv12 { bt601: false }, 1.5, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709))
            .and_then(|r| r.inspect_err(|e| eprintln!("decoder: no small NV12 frames, RGBA instead: {e:#}")).ok());
        let small = match nv12 {
            Some(s) => s,
            None => make(SurfaceFormat::Rgba, 4.0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709)?,
        };
        let n = small.0.textures.len();
        self.small = Some(small);
        Ok(n)
    }

    fn frame_as(&mut self, index: u64, small: bool) -> Result<Option<Picture>> {
        // Frames read ahead are past: the decoder is where they ended.
        self.coming.clear();
        let near = match (&self.src, self.next_index) {
            (Source::Direct(d), Some(n)) => d.decode_on(self.time_of(n), self.time_of(index)),
            (Source::Reader(_), Some(n)) => n <= index && index - n <= MAX_SKIP,
            (_, None) => false,
        };
        if !near {
            self.seek(index)?;
        }
        let mut last: Option<(u64, IMFSample)> = None;
        loop {
            match self.read()? {
                Some((i, sample)) if i <= index => {
                    // The next frame's time known from the file's index: no
                    // need to decode it to know this is the answer.
                    if let Source::Direct(d) = &self.src {
                        if let Some(t) = d.upcoming() {
                            let next = (t != i64::MAX).then(|| self.index_of(t));
                            if next.is_none_or(|n| n > index) {
                                self.next_index = next;
                                return self.picture_as(i, &sample, small).map(Some);
                            }
                        }
                    }
                    last = Some((i, sample));
                }
                Some((i, sample)) => {
                    // Past it: that's the answer's end. Keep this one for the
                    // next read (it's the next frame, needed anyway).
                    let Some((li, ls)) = last else { return self.picture_as(i, &sample, small).map(Some) };
                    self.peeked = Some((i, sample));
                    self.next_index = Some(i);
                    return self.picture_as(li, &ls, small).map(Some);
                }
                None => return last.map(|(i, s)| self.picture_as(i, &s, small)).transpose(),
            }
        }
    }

    /// The frame after the last one handed out, with its pixels. `None` at
    /// the end. Frames that come back to the CPU are read [`READ_AHEAD`]
    /// ahead: each waits for the GPU to finish only frames it started
    /// earlier, not its own (every frame of a clip at 1280 px: 575 to 680 a
    /// second; at 640 px no faster, the decoder is what's slow there).
    pub fn next(&mut self) -> Result<Option<Picture>> {
        if self.pool.is_some() {
            return match self.read()? {
                Some((i, sample)) => self.picture(i, &sample).map(Some),
                None => Ok(None),
            };
        }
        while self.coming.len() < READ_AHEAD {
            let Some((i, sample)) = self.read()? else { break };
            let (texture, slice) = decoded_texture(&sample)?;
            let convert = self.convert.as_mut().context("no conversion")?;
            convert.run(&texture, slice, 0)?;
            let k = self.next_staging;
            convert.stage(&self.gpu, k)?;
            self.next_staging = (k + 1) % READ_AHEAD;
            self.coming.push_back((i, k));
        }
        let Some((index, k)) = self.coming.pop_front() else { return Ok(None) };
        let rgba = self.convert.as_ref().context("no conversion")?.read_staged(&self.gpu, k)?;
        Ok(Some(Picture { index, width: self.width, height: self.height, rgba, gpu: None }))
    }

    /// The frame whose slot on the grid contains `time` (the decoder's clock).
    fn index_of(&self, time: i64) -> u64 {
        (((time - self.offset) as f64 / UNITS) * self.fps + 1e-3).floor().max(0.0) as u64
    }

    /// Frame `index`'s time on the decoder's clock (100 ns).
    fn time_of(&self, index: u64) -> i64 {
        (index as f64 / self.fps * UNITS) as i64 + self.offset
    }

    fn seek(&mut self, index: u64) -> Result<()> {
        self.peeked = None;
        self.next_index = None;
        let t = self.time_of(index);
        match &mut self.src {
            Source::Direct(d) => d.seek(t)?,
            Source::Reader(r) => {
                let pos = PROPVARIANT::from(t);
                unsafe { r.SetCurrentPosition(&GUID::zeroed(), &pos)? };
            }
        }
        Ok(())
    }

    /// The next sample in presentation order, numbered.
    fn read(&mut self) -> Result<Option<(u64, IMFSample)>> {
        if let Some(p) = self.peeked.take() {
            self.next_index = Some(p.0 + 1);
            return Ok(Some(p));
        }
        let Some((time, sample)) = self.read_sample()? else {
            self.next_index = None;
            return Ok(None);
        };
        // The frame whose slot on the grid contains this time, rounding down
        // like the app does. Times are in whole 100 ns units, so a frame at
        // exactly k/60 s reads a hair early: allow a thousandth of a frame,
        // or every third frame of a 60 fps clip takes the previous one's
        // number (and the next number is skipped: judder).
        let index = self.index_of(time);
        self.next_index = Some(index + 1);
        Ok(Some((index, sample)))
    }

    fn read_sample(&mut self) -> Result<Option<(i64, IMFSample)>> {
        let reader = match &mut self.src {
            Source::Direct(d) => return d.read(),
            Source::Reader(r) => r,
        };
        loop {
            let mut flags = 0u32;
            let mut time = 0i64;
            let mut sample = None;
            unsafe { reader.ReadSample(STREAM, 0, None, Some(&mut flags), Some(&mut time), Some(&mut sample))? };
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                return Ok(None);
            }
            if let Some(s) = sample {
                return Ok(Some((time, s)));
            }
        }
    }

    /// Convert a decoded frame and copy it off the GPU.
    fn picture(&mut self, index: u64, sample: &IMFSample) -> Result<Picture> {
        self.picture_as(index, sample, false)
    }

    /// [`Self::picture`]; `small`: into the small frames' textures.
    fn picture_as(&mut self, index: u64, sample: &IMFSample, small: bool) -> Result<Picture> {
        let (texture, slice) = decoded_texture(sample)?;
        if small {
            let (pool, convert) = self.small.as_mut().context("no small frames")?;
            let pool = pool.clone();
            let slot = free_slot(&pool)?;
            convert.run(&texture, slice, slot)?;
            let (width, height) = convert.size;
            // Sent to the GPU with the next flush (see `Self::flush`).
            let surface = hand_over(&self.gpu, pool, slot, false)?;
            return Ok(Picture { index, width, height, rgba: Vec::new(), gpu: Some(surface) });
        }
        let Some(pool) = self.pool.clone() else {
            let convert = self.convert.as_mut().context("no conversion")?;
            convert.run(&texture, slice, 0)?;
            let rgba = convert.read(&self.gpu)?;
            return Ok(Picture { index, width: self.width, height: self.height, rgba, gpu: None });
        };
        let slot = free_slot(&pool)?;
        match self.convert.as_mut() {
            Some(c) => c.run(&texture, slice, slot)?,
            // NV12 as it is: the decoder's picture (its texture can be larger,
            // padded to whole macroblocks) copied into the pool's.
            None => unsafe {
                let region = D3D11_BOX { left: 0, top: 0, front: 0, right: self.width, bottom: self.height, back: 1 };
                self.gpu.context.CopySubresourceRegion(&pool.textures[slot], 0, 0, 0, 0, &texture, slice, Some(&region));
            },
        }
        let surface = hand_over(&self.gpu, pool, slot, true)?;
        Ok(Picture { index, width: self.width, height: self.height, rgba: Vec::new(), gpu: Some(surface) })
    }
}

/// A free texture of `pool`: there's always one unless the app holds on to
/// frames (then waits a moment for one).
fn free_slot(pool: &Surfaces) -> Result<usize> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(s) = pool.free.lock().unwrap().pop_front() {
            return Ok(s);
        }
        if Instant::now() > deadline {
            bail!("no free frame texture");
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// `slot` of `pool`, just written (queued on the GPU), as a frame for the
/// renderer: it waits for the pool's fence before reading it. `flush`: send
/// the work to the GPU now (else the caller flushes before it's drawn).
fn hand_over(gpu: &Gpu, pool: Arc<Surfaces>, slot: usize, flush: bool) -> Result<Surface> {
    let ready = match &pool.fence {
        // The other device waits for this value before reading the frame.
        Some((fence, _)) => {
            let v = pool.fence_value.fetch_add(1, Ordering::Relaxed) + 1;
            unsafe {
                let ctx4: ID3D11DeviceContext4 = gpu.context.cast()?;
                ctx4.Signal(fence, v)?;
                if flush {
                    gpu.context.Flush();
                }
            }
            v
        }
        // No shared fence: finish writing here before handing it over.
        None => {
            gpu.wait_idle()?;
            0
        }
    };
    Ok(Surface(Arc::new(Slot { pool, slot, ready })))
}

/// The decoder's texture holding `sample`'s picture, and its array slice.
fn decoded_texture(sample: &IMFSample) -> Result<(ID3D11Texture2D, u32)> {
    unsafe {
        let buffer = sample.GetBufferByIndex(0)?;
        let dxgi: IMFDXGIBuffer = buffer.cast().context("the decoder didn't give a GPU frame")?;
        let mut texture: Option<ID3D11Texture2D> = None;
        dxgi.GetResource(&ID3D11Texture2D::IID, &mut texture as *mut _ as *mut _)?;
        Ok((texture.context("no texture")?, dxgi.GetSubresourceIndex()?))
    }
}

/// Height for `width` keeping `source`'s aspect, even.
fn even_height(width: u32, source: (u32, u32)) -> u32 {
    (((width as f64) * source.1 as f64 / source.0.max(1) as f64 / 2.0).round() as u32 * 2).max(2)
}

/// What frames go into at `size` (a shared pool, or one texture read back)
/// and the conversion pass into them, if they're converted. Shared at the
/// video's own size, they stay NV12 when the graphics card can share that:
/// a copy instead of a conversion, and 1.5 bytes a pixel instead of 4.
/// `HESTECLIPS_RGBA_FRAMES=1` always converts.
fn outputs(gpu: &Gpu, shared: bool, source: (u32, u32), size: (u32, u32), fps: f64, bt601: bool) -> Result<(Option<Arc<Surfaces>>, Option<Convert>)> {
    let nv12 = shared && size == source && size.0 % 2 == 0 && size.1 % 2 == 0 && shares_nv12(gpu) && std::env::var_os("HESTECLIPS_RGBA_FRAMES").is_none();
    if nv12 {
        match surfaces(gpu, size.0, size.1, SurfaceFormat::Nv12 { bt601 }, pool_count(size, 1.5), false) {
            Ok(pool) => return Ok((Some(Arc::new(pool)), None)),
            Err(e) => eprintln!("decoder: can't share NV12 frames, converting them: {e:#}"),
        }
    }
    let pool = if shared { Some(Arc::new(surfaces(gpu, size.0, size.1, SurfaceFormat::Rgba, pool_count(size, 4.0), true)?)) } else { None };
    let targets: Vec<ID3D11Texture2D> = match &pool {
        Some(p) => p.textures.clone(),
        None => vec![gpu.texture(size.0, size.1, DXGI_FORMAT_R8G8B8A8_UNORM, D3D11_BIND_RENDER_TARGET.0 as u32, D3D11_USAGE_DEFAULT, 0)?],
    };
    let convert = Convert::new(gpu, source, size, fps, bt601, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, targets)?;
    Ok((pool, Some(convert)))
}

/// Playback's textures: as many as fit in [`POOL_BYTES`], within bounds.
fn pool_count(size: (u32, u32), bytes_per_pixel: f64) -> usize {
    (POOL_BYTES as f64 / (size.0 as f64 * size.1 as f64 * bytes_per_pixel).max(1.0)).clamp(POOL_MIN as f64, POOL_MAX as f64) as usize
}

/// The graphics card can share NV12 textures with another device (Windows
/// 10 1803 and later, with a driver that says so).
fn shares_nv12(gpu: &Gpu) -> bool {
    let mut options = D3D11_FEATURE_DATA_D3D11_OPTIONS4::default();
    let asked = unsafe {
        gpu.device.CheckFeatureSupport(
            D3D11_FEATURE_D3D11_OPTIONS4,
            &mut options as *mut _ as *mut _,
            std::mem::size_of::<D3D11_FEATURE_DATA_D3D11_OPTIONS4>() as u32,
        )
    };
    asked.is_ok() && options.ExtendedNV12SharedTextureSupported.as_bool()
}

/// A pool of `count` shared textures; `render_target`: the video processor
/// draws into them (else frames are copied in).
fn surfaces(gpu: &Gpu, width: u32, height: u32, format: SurfaceFormat, count: usize, render_target: bool) -> Result<Surfaces> {
    static NEXT: AtomicUsize = AtomicUsize::new(1);
    let dxgi = match format {
        SurfaceFormat::Rgba => DXGI_FORMAT_R8G8B8A8_UNORM,
        SurfaceFormat::Nv12 { .. } => windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12,
    };
    let bind = D3D11_BIND_SHADER_RESOURCE.0 | if render_target { D3D11_BIND_RENDER_TARGET.0 } else { 0 };
    let mut textures = Vec::new();
    let mut handles = Vec::new();
    for _ in 0..count {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: dxgi,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: bind as u32,
            CPUAccessFlags: 0,
            MiscFlags: (D3D11_RESOURCE_MISC_SHARED.0 | D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0) as u32,
        };
        unsafe {
            let mut tex = None;
            gpu.device.CreateTexture2D(&desc, None, Some(&mut tex))?;
            let tex = tex.context("no texture")?;
            let res: IDXGIResource1 = tex.cast()?;
            let handle = res.CreateSharedHandle(None, (DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE).0 | GENERIC_ALL.0, None)?;
            textures.push(tex);
            handles.push(handle);
        }
    }
    let fence = unsafe {
        (|| -> Result<(ID3D11Fence, HANDLE)> {
            let device5: ID3D11Device5 = gpu.device.cast()?;
            let mut fence: Option<ID3D11Fence> = None;
            device5.CreateFence(0, D3D11_FENCE_FLAG_SHARED, &mut fence)?;
            let fence = fence.context("no fence")?;
            let handle = fence.CreateSharedHandle(None, GENERIC_ALL.0, None)?;
            Ok((fence, handle))
        })()
        .inspect_err(|e| eprintln!("decoder: no shared fence, waiting on the CPU instead: {e:#}"))
        .ok()
    };
    Ok(Surfaces {
        id: NEXT.fetch_add(1, Ordering::Relaxed),
        format,
        textures,
        handles,
        free: Mutex::new((0..count).collect()),
        fence,
        fence_value: std::sync::atomic::AtomicU64::new(0),
    })
}

/// The GPU pass from a decoded NV12 frame to RGBA at the output size, and the
/// staging textures it's read back through.
struct Convert {
    video: ID3D11VideoContext,
    vdev: ID3D11VideoDevice,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    /// What it draws into: one texture (read back) or the shared pool's.
    targets: Vec<ID3D11Texture2D>,
    outputs: Vec<ID3D11VideoProcessorOutputView>,
    /// CPU-readable copies of the target, made the first time they're
    /// needed (frames shared on the GPU never are).
    staging: Vec<ID3D11Texture2D>,
    /// Input views by decoder texture and array slice (the decoder cycles
    /// through a fixed set of surfaces).
    inputs: HashMap<(usize, u32), ID3D11VideoProcessorInputView>,
    source: (u32, u32),
    size: (u32, u32),
}

impl Convert {
    /// From the decoder's NV12 to `targets` (`out`: RGB, or YCbCr into NV12 ones).
    fn new(gpu: &Gpu, source: (u32, u32), size: (u32, u32), fps: f64, bt601: bool, out: DXGI_COLOR_SPACE_TYPE, targets: Vec<ID3D11Texture2D>) -> Result<Self> {
        let vdev: ID3D11VideoDevice = gpu.device.cast().context("the graphics card has no video processor")?;
        let video: ID3D11VideoContext = gpu.context.cast()?;
        let rate = DXGI_RATIONAL { Numerator: fps.round().max(1.0) as u32, Denominator: 1 };
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: source.0,
            InputHeight: source.1,
            OutputFrameRate: rate,
            OutputWidth: size.0,
            OutputHeight: size.1,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_QUALITY,
        };
        unsafe {
            let enumerator = vdev.CreateVideoProcessorEnumerator(&desc)?;
            let processor = vdev.CreateVideoProcessor(&enumerator, 0)?;
            let out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
            };
            let mut outputs = Vec::new();
            for t in &targets {
                let mut view = None;
                vdev.CreateVideoProcessorOutputView(t, &enumerator, &out_desc, Some(&mut view))?;
                outputs.push(view.context("no output view")?);
            }

            // Studio-range YCbCr in, as the file says; out full-range RGB (what
            // the screen wants), or studio-range YCbCr scaled.
            let in_space: DXGI_COLOR_SPACE_TYPE = if bt601 { DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601 } else { DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709 };
            let video1: ID3D11VideoContext1 = video.cast().context("the graphics card's video processor is too old")?;
            video1.VideoProcessorSetStreamColorSpace1(&processor, 0, in_space);
            video1.VideoProcessorSetOutputColorSpace1(&processor, out);
            let src = RECT { left: 0, top: 0, right: source.0 as i32, bottom: source.1 as i32 };
            let dst = RECT { left: 0, top: 0, right: size.0 as i32, bottom: size.1 as i32 };
            video.VideoProcessorSetStreamFrameFormat(&processor, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            video.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&src));
            video.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&dst));
            video.VideoProcessorSetOutputTargetRect(&processor, true, Some(&dst));
            video.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            video.VideoProcessorSetOutputAlphaFillMode(&processor, D3D11_VIDEO_PROCESSOR_ALPHA_FILL_MODE_OPAQUE, 0);
            Ok(Self { video, vdev, enumerator, processor, targets, outputs, staging: Vec::new(), inputs: HashMap::new(), source, size })
        }
    }

    /// Convert array slice `slice` of the decoder's `texture` into target `to`.
    fn run(&mut self, texture: &ID3D11Texture2D, slice: u32, to: usize) -> Result<()> {
        let key = (texture.as_raw() as usize, slice);
        if !self.inputs.contains_key(&key) {
            let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: slice } },
            };
            let mut view = None;
            unsafe { self.vdev.CreateVideoProcessorInputView(texture, &self.enumerator, &desc, Some(&mut view))? };
            // A new decoder (after some seeks) means new surfaces: drop stale views.
            if self.inputs.len() > 64 {
                self.inputs.clear();
            }
            self.inputs.insert(key, view.context("no input view")?);
        }
        let input = self.inputs[&key].clone();
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            ppPastSurfaces: std::ptr::null_mut(),
            pInputSurface: std::mem::ManuallyDrop::new(Some(input)),
            ppFutureSurfaces: std::ptr::null_mut(),
            ppPastSurfacesRight: std::ptr::null_mut(),
            pInputSurfaceRight: std::mem::ManuallyDrop::new(None),
            ppFutureSurfacesRight: std::ptr::null_mut(),
        };
        let mut streams = [stream];
        let result = unsafe { self.video.VideoProcessorBlt(&self.processor, &self.outputs[to], 0, &streams) };
        unsafe { std::mem::ManuallyDrop::drop(&mut streams[0].pInputSurface) };
        result?;
        let _ = self.source;
        Ok(())
    }

    /// The (single) target's pixels: tight RGBA rows.
    fn read(&mut self, gpu: &Gpu) -> Result<Vec<u8>> {
        self.stage(gpu, 0)?;
        self.read_staged(gpu, 0)
    }

    /// Start copying the (single) target into staging texture `k`; the GPU
    /// does it after what's queued, without waiting here.
    fn stage(&mut self, gpu: &Gpu, k: usize) -> Result<()> {
        while self.staging.len() <= k {
            let t = gpu.texture(self.size.0, self.size.1, DXGI_FORMAT_R8G8B8A8_UNORM, 0, D3D11_USAGE_STAGING, D3D11_CPU_ACCESS_READ.0 as u32)?;
            self.staging.push(t);
        }
        unsafe {
            gpu.context.CopyResource(&self.staging[k], &self.targets[0]);
            gpu.context.Flush();
        }
        Ok(())
    }

    /// Staging texture `k`'s pixels, once the GPU has put them there: tight
    /// RGBA rows.
    fn read_staged(&self, gpu: &Gpu, k: usize) -> Result<Vec<u8>> {
        let staging = self.staging.get(k).context("nothing staged")?;
        let (w, h) = self.size;
        let row = w as usize * 4;
        let mut rgba = Vec::with_capacity(row * h as usize);
        unsafe {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            gpu.context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            let src = mapped.pData as *const u8;
            // Written whole below, not zeroed first.
            rgba.set_len(row * h as usize);
            if mapped.RowPitch as usize == row {
                std::ptr::copy_nonoverlapping(src, rgba.as_mut_ptr(), rgba.len());
            } else {
                for y in 0..h as usize {
                    std::ptr::copy_nonoverlapping(src.add(y * mapped.RowPitch as usize), rgba[y * row..].as_mut_ptr(), row);
                }
            }
            gpu.context.Unmap(staging, 0);
        }
        if rgba.is_empty() {
            bail!("empty frame");
        }
        Ok(rgba)
    }
}
