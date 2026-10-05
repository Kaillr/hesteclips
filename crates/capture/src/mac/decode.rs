//! Video decoding for playback, in-process: VideoToolbox's hardware decoder
//! fed straight from the file by our own index (`mp4read`). One decoder
//! stays open per clip, so playing, seeking and stepping never start anything
//! new (an ffmpeg process per play took 120-500 ms to show its first frame).
//!
//! Frames come out one of two ways ([`Output`]):
//! - **For the screen**: the decoder's own NV12, at the video's size, in
//!   IOSurface-backed buffers the app's renderer opens as Metal textures
//!   without copying (`Picture::gpu`); its shader converts to RGB as it
//!   scales. Asking the decoder for BGRA instead cost 2.3× the time: 3.5 ms a
//!   frame instead of 1.5 at 3600×2338 (285 fps against 664, M4 Pro).
//! - **As pixels** (thumbnails, scrub frames): BGRA at the size asked for,
//!   converted and scaled by the decoder's pixel transfer, read back as RGBA.
//!
//! Decoding is asynchronous: every frame from a keyframe up to a jump's
//! target is queued on the hardware at once, and the ones only needed as
//! references are decoded without output (no conversion, no buffer). The
//! file's index says which frame answers a request, so no frame past it is
//! decoded to find out.
//!
//! Frames are numbered by time from the start of the clip (as the editor and
//! the scrub proxy count them), on the grid of the app's frame rate.

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::ptr::{self, NonNull};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::{
    CMBlockBuffer, CMFormatDescription, CMSampleBuffer, CMSampleTimingInfo, CMTime, CMVideoFormatDescriptionCreate,
    kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms, kCMFormatDescriptionExtension_YCbCrMatrix, kCMTimeInvalid, kCMVideoCodecType_H264,
};
use objc2_core_video::{
    CVImageBuffer, CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
    CVPixelBufferGetIOSurface, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress, kCVPixelBufferHeightKey, kCVPixelBufferIOSurfacePropertiesKey,
    kCVPixelBufferMetalCompatibilityKey, kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey,
    kCVImageBufferYCbCrMatrix_ITU_R_601_4, kCVImageBufferYCbCrMatrix_ITU_R_709_2, kCVPixelFormatType_32BGRA,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};
use objc2_io_surface::IOSurfaceRef;
use objc2_video_toolbox::{
    VTDecodeFrameFlags, VTDecodeInfoFlags, VTDecompressionOutputCallbackRecord, VTDecompressionSession,
    VTSessionSetProperty, kVTDecompressionPropertyKey_PixelTransferProperties, kVTDecompressionPropertyKey_RealTime,
    kVTDownsamplingMode_Average, kVTPixelTransferPropertyKey_DownsamplingMode,
    kVTVideoDecoderSpecification_EnableHardwareAcceleratedVideoDecoder,
};

use crate::mp4read::Index;

/// Bytes read from the file at a time (frames are stored one after another):
/// one read per few dozen frames, not per frame.
const BLOCK: usize = 4 << 20;
/// Frames queued on the decoder ahead of the one asked for, decoding while
/// the caller handles it. More made jumps slower (each waits for the queue
/// to finish first: 14 ms to a keyframe with 2, 42 with 8, 73 with 16) and
/// reading in order no faster.
const QUEUE: usize = 2;

/// How decoded frames come out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    /// NV12 at the video's own size, on the GPU only (`Picture::gpu`).
    Screen,
    /// RGBA at this width (height keeps the aspect, even), in `Picture::rgba`.
    Rgba(u32),
}

/// A decoded picture: NV12 on the GPU in `gpu` ([`Output::Screen`]), or
/// opaque RGBA in `rgba`, rows top to bottom, no padding ([`Output::Rgba`]).
pub struct Picture {
    /// Frame number from the start of the clip.
    pub index: u64,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub gpu: Option<Surface>,
}

/// A decoded frame's buffer: studio-range NV12 in an IOSurface (plane 0 Y,
/// plane 1 interleaved CbCr at half size), which the renderer opens as
/// textures without copying. The decoder reuses it once dropped.
pub struct Surface {
    buf: CFRetained<CVPixelBuffer>,
    /// BT.601 colours (else BT.709).
    pub bt601: bool,
}

// CoreVideo buffers are thread-safe reference-counted objects; nothing
// writes to one after the decoder hands it out.
unsafe impl Send for Surface {}
unsafe impl Sync for Surface {}

impl Surface {
    /// A frame from elsewhere (a capture's), studio-range NV12, BT.709.
    pub(crate) fn from_buffer(buf: CFRetained<CVPixelBuffer>) -> Self {
        Self { buf, bt601: false }
    }

    /// The IOSurface holding the pixels.
    pub fn io_surface(&self) -> Option<CFRetained<IOSurfaceRef>> {
        CVPixelBufferGetIOSurface(Some(&self.buf))
    }
}

/// What the output callback hands back, keyed by presentation time.
#[derive(Default)]
struct Out {
    frames: BTreeMap<i64, CFRetained<CVPixelBuffer>>,
    /// Frames queued for output not yet called back.
    pending: usize,
    /// A decode failed (status), for the next read to report.
    error: Option<i32>,
}

struct Shared {
    out: Mutex<Out>,
    ready: Condvar,
}

pub struct Decoder {
    session: CFRetained<VTDecompressionSession>,
    format: CFRetained<CMFormatDescription>,
    /// Owned by the session's callback: freed after the session is.
    shared: *const Shared,
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
    /// Frames come out in a different order than they go in (B-frames).
    reordered: bool,
    /// The video's own size.
    source: (u32, u32),
    /// The next sample (decode order) to hand the decoder.
    next_feed: usize,
    /// Samples from here to `next_feed` were queued for output.
    out_from: usize,
    /// Presentation time of the last frame handed out: decoded frames up to
    /// it are thrown away.
    handed: i64,
    /// The next frame to hand out (position in `by_pts`), when reading on.
    next_out: Option<usize>,
}

// The session is used from one thread at a time, which owns the decoder.
unsafe impl Send for Decoder {}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe {
            self.session.wait_for_asynchronous_frames();
            self.session.invalidate();
            // No callback can run after invalidate returns.
            drop(Box::from_raw(self.shared as *mut Shared));
        }
    }
}

impl Decoder {
    /// Open `path`, frames coming out as `output` says.
    pub fn open(path: &Path, output: Output) -> Result<Self> {
        let index = Index::read(path)?;
        ensure!(!index.samples.is_empty(), "no video frames");
        let keys = index.keyframes();
        ensure!(keys.first() == Some(&0), "the video doesn't start with a keyframe");
        let mut by_pts: Vec<usize> = (0..index.samples.len()).collect();
        by_pts.sort_by_key(|&i| index.samples[i].pts);
        let (sw, sh) = (index.width, index.height);
        let (width, height) = match output {
            Output::Screen => (sw, sh),
            Output::Rgba(w) => {
                let w = w.min(sw).max(2) / 2 * 2;
                (w, even_height(w, (sw, sh)))
            }
        };
        // BT.601 only when the file says so, as other players do.
        let bt601 = matches!(index.matrix, Some(5 | 6));

        let format = format_description(&index)?;
        let shared = Box::into_raw(Box::new(Shared { out: Mutex::new(Out::default()), ready: Condvar::new() }));
        let session = match create_session(&format, output, width, height, shared) {
            Ok(s) => s,
            Err(e) => {
                drop(unsafe { Box::from_raw(shared) });
                return Err(e);
            }
        };
        // Frames span about one interval; the caller usually knows the rate better.
        let reordered = index.samples.windows(2).any(|w| w[1].pts < w[0].pts);
        let fps = {
            let s = &index.samples;
            let span = (s[by_pts[by_pts.len() - 1]].pts - s[by_pts[0]].pts) as f64 / index.timescale.max(1) as f64;
            if by_pts.len() > 1 && span > 0.0 { (by_pts.len() - 1) as f64 / span } else { 60.0 }
        };
        Ok(Self {
            session,
            format,
            shared,
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
            reordered,
            source: (sw, sh),
            next_feed: 0,
            out_from: 0,
            handed: i64::MIN,
            next_out: Some(0),
        })
    }

    /// Frames at `width` wide, as RGBA (thumbnails, scrub frames).
    pub fn open_rgba(path: &Path, width: u32) -> Result<Self> {
        Self::open(path, Output::Rgba(width))
    }

    /// The video's own size.
    pub fn source_size(&self) -> (u32, u32) {
        self.source
    }

    /// The keyframes' numbers, ascending.
    pub fn keyframes(&self) -> Vec<u64> {
        let mut k: Vec<u64> = self.keys.iter().map(|&i| self.index_of(self.pts_sec(i))).collect();
        k.sort_unstable();
        k
    }

    /// Number frames on this rate's grid instead of the file's own (to match
    /// how the app counts them, from ffprobe's average rate).
    pub fn set_fps(&mut self, fps: f64) {
        if fps > 0.0 {
            self.fps = fps;
        }
    }

    /// How many frames can be out at once on the GPU, if limited (it isn't:
    /// buffers come from a pool that grows as needed).
    pub fn pool_size(&self) -> Option<usize> {
        None
    }

    /// "direct": frames always come from our own index.
    pub fn way(&self) -> &'static str {
        "direct"
    }

    /// The frame showing at `index`: the last one at or before it, so a
    /// dropped frame's slot shows the frame before the gap (as the scrub proxy
    /// and playback do). Decoded on from where the decoder is when that's
    /// close, else from the keyframe before it. `None` past the end.
    pub fn frame(&mut self, index: u64) -> Result<Option<Picture>> {
        // The last frame (presentation order) starting before the slot ends;
        // before the first frame, the first one shows.
        let end = self.time_of(index + 1) - 1e-9;
        let pos = self.by_pts.partition_point(|&i| self.pts_sec(i) < end).saturating_sub(1);
        self.at(pos)
    }

    /// The frame after the last one handed out, with its pixels. `None` at the end.
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
        // Already decoded (reading on, or asked again)?
        if let Some(buf) = self.take(pts) {
            return self.finish(pos, buf).map(Some);
        }
        // Queued for output and not handed out (or passed) since: on its way.
        let on_its_way = self.out_from <= sample && sample < self.next_feed && pts > self.handed;
        if !on_its_way {
            // Decoding on reaches it unless it's past the next keyframe (or behind).
            let key = self.keys[self.keys.partition_point(|&k| k <= sample) - 1];
            if !(key <= self.next_feed && self.next_feed <= sample) {
                self.reset()?;
                self.next_feed = key;
            }
            // Everything up to it in decode order, the ones before it only as
            // references. With B-frames every one comes out: frames decoded
            // before the target can be shown after it, and reading on needs them.
            self.out_from = if self.reordered { self.next_feed } else { sample };
            while self.next_feed <= sample {
                let i = self.next_feed;
                self.feed(i, i >= self.out_from)?;
                self.next_feed += 1;
            }
        }
        // Read ahead: the next few frames in order, decoding while the caller
        // handles this one.
        let ahead_end = (sample + 1 + QUEUE).min(self.index.samples.len());
        while self.next_feed < ahead_end {
            let i = self.next_feed;
            self.feed(i, true)?;
            self.next_feed += 1;
        }
        let buf = self.wait_for(pts)?;
        self.finish(pos, buf).map(Some)
    }

    /// Hand out frame `pos`.
    fn finish(&mut self, pos: usize, buf: CFRetained<CVPixelBuffer>) -> Result<Picture> {
        self.next_out = Some(pos + 1);
        // Forget decoded frames shown before it (a jump back leaves some).
        let pts = self.index.samples[self.by_pts[pos]].pts;
        self.handed = pts;
        self.shared().out.lock().unwrap().frames.retain(|&t, _| t > pts);
        let index = self.index_of(self.pts_sec(self.by_pts[pos]));
        Ok(match self.output {
            Output::Rgba(_) => Picture { index, width: self.width, height: self.height, rgba: bgra_to_rgba(&buf)?, gpu: None },
            Output::Screen => Picture { index, width: self.width, height: self.height, rgba: Vec::new(), gpu: Some(Surface { buf, bt601: self.bt601 }) },
        })
    }

    fn shared(&self) -> &Shared {
        unsafe { &*self.shared }
    }

    /// A decoded frame at `pts`, if it's in.
    fn take(&self, pts: i64) -> Option<CFRetained<CVPixelBuffer>> {
        self.shared().out.lock().unwrap().frames.remove(&pts)
    }

    /// Wait for the frame at `pts` to come out of the decoder.
    fn wait_for(&self, pts: i64) -> Result<CFRetained<CVPixelBuffer>> {
        let shared = self.shared();
        let mut out = shared.out.lock().unwrap();
        loop {
            if let Some(status) = out.error.take() {
                bail!("the decoder failed (VideoToolbox error {status})");
            }
            if let Some(b) = out.frames.remove(&pts) {
                return Ok(b);
            }
            if out.pending == 0 {
                bail!("the decoder gave no frame for {pts}");
            }
            let (o, timeout) = shared.ready.wait_timeout(out, Duration::from_secs(2)).unwrap();
            out = o;
            if timeout.timed_out() {
                bail!("the decoder stopped giving frames back");
            }
        }
    }

    /// Drop everything queued and decoded, to start again from a keyframe.
    fn reset(&mut self) -> Result<()> {
        unsafe { self.session.wait_for_asynchronous_frames() };
        let mut out = self.shared().out.lock().unwrap();
        out.frames.clear();
        out.pending = 0;
        out.error = None;
        drop(out);
        self.out_from = 0;
        self.handed = i64::MIN;
        Ok(())
    }

    /// Queue sample `i` (decode order) on the decoder; `output` false when it's
    /// only needed as a reference.
    fn feed(&mut self, i: usize, output: bool) -> Result<()> {
        let s = self.index.samples[i];
        let (start, end) = (s.offset, s.offset + s.size as u64);
        if start < self.block_at || end > self.block_at + self.block.len() as u64 {
            let file_len = self.file.metadata()?.len();
            let len = BLOCK.max(s.size as usize).min(file_len.saturating_sub(start) as usize).max(s.size as usize);
            self.block.resize(len, 0);
            self.file.seek(SeekFrom::Start(start))?;
            self.file.read_exact(&mut self.block)?;
            self.block_at = start;
        }
        let at = (start - self.block_at) as usize;
        let data = &self.block[at..at + s.size as usize];
        let sample = sample_buffer(&self.format, data, s.pts, self.index.timescale)?;
        let mut flags = VTDecodeFrameFlags::Frame_EnableAsynchronousDecompression;
        if !output {
            flags |= VTDecodeFrameFlags::Frame_DoNotOutputFrame;
        }
        if output {
            self.shared().out.lock().unwrap().pending += 1;
        }
        // A non-null frame tag marks the frames that come out (the callback
        // also runs for the others).
        let tag = if output { ptr::dangling_mut::<c_void>() } else { ptr::null_mut() };
        let status = unsafe { self.session.decode_frame(&sample, flags, tag, ptr::null_mut()) };
        if status != 0 {
            if output {
                self.shared().out.lock().unwrap().pending -= 1;
            }
            bail!("the decoder rejected a frame (VideoToolbox error {status})");
        }
        Ok(())
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

    /// Frame `index`'s start, in seconds on the file's clock.
    fn time_of(&self, index: u64) -> f64 {
        index as f64 / self.fps + self.start()
    }

    /// The first frame's time: frame 0.
    fn start(&self) -> f64 {
        self.pts_sec(self.by_pts[0])
    }
}

/// Height for `width` keeping `source`'s aspect, even.
fn even_height(width: u32, source: (u32, u32)) -> u32 {
    (((width as f64) * source.1 as f64 / source.0.max(1) as f64 / 2.0).round() as u32 * 2).max(2)
}

/// The decoder's description of the stream: H.264 with the file's own
/// `avcC` (its parameter sets, profile and NAL length).
fn format_description(index: &Index) -> Result<CFRetained<CMFormatDescription>> {
    let avcc = objc2_core_foundation::CFData::from_bytes(&index.avcc);
    let atoms = CFDictionary::<CFString, CFType>::from_slices(&[&*CFString::from_static_str("avcC")], &[avcc.as_ref()]);
    // The colour matrix the file states (`colr`), which overrides the
    // stream's own; BT.601 only when it says so.
    let matrix = match index.matrix {
        Some(5 | 6) => Some(unsafe { kCVImageBufferYCbCrMatrix_ITU_R_601_4 }),
        Some(1) => Some(unsafe { kCVImageBufferYCbCrMatrix_ITU_R_709_2 }),
        _ => None,
    };
    let mut keys = vec![unsafe { kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms }];
    let mut values: Vec<&CFType> = vec![atoms.as_ref()];
    if let Some(m) = matrix {
        keys.push(unsafe { kCMFormatDescriptionExtension_YCbCrMatrix });
        values.push(m.as_ref());
    }
    let extensions = CFDictionary::<CFString, CFType>::from_slices(&keys, &values);
    let mut out: *const CMFormatDescription = ptr::null();
    let status = unsafe {
        CMVideoFormatDescriptionCreate(
            None,
            kCMVideoCodecType_H264,
            index.width as i32,
            index.height as i32,
            Some(extensions.as_opaque()),
            NonNull::from(&mut out),
        )
    };
    let out = NonNull::new(out as *mut CMFormatDescription).filter(|_| status == 0).context("the video's H.264 setup isn't readable")?;
    Ok(unsafe { CFRetained::from_raw(out) })
}

/// A decompression session giving IOSurface-backed frames at `width`×`height`:
/// NV12 for the screen, else BGRA.
fn create_session(format: &CMFormatDescription, output: Output, width: u32, height: u32, shared: *const Shared) -> Result<CFRetained<VTDecompressionSession>> {
    let pixels = match output {
        Output::Screen => kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
        Output::Rgba(_) => kCVPixelFormatType_32BGRA,
    };
    let spec = CFDictionary::<CFString, CFType>::from_slices(
        &[unsafe { kVTVideoDecoderSpecification_EnableHardwareAcceleratedVideoDecoder }],
        &[CFBoolean::new(true).as_ref()],
    );
    let surface_props = CFDictionary::<CFString, CFType>::from_slices(&[], &[]);
    let attrs = CFDictionary::<CFString, CFType>::from_slices(
        &[
            unsafe { kCVPixelBufferPixelFormatTypeKey },
            unsafe { kCVPixelBufferWidthKey },
            unsafe { kCVPixelBufferHeightKey },
            unsafe { kCVPixelBufferIOSurfacePropertiesKey },
            unsafe { kCVPixelBufferMetalCompatibilityKey },
        ],
        &[
            CFNumber::new_i32(pixels as i32).as_ref(),
            CFNumber::new_i32(width as i32).as_ref(),
            CFNumber::new_i32(height as i32).as_ref(),
            surface_props.as_ref(),
            CFBoolean::new(true).as_ref(),
        ],
    );
    let callback = VTDecompressionOutputCallbackRecord { decompressionOutputCallback: Some(on_decoded), decompressionOutputRefCon: shared as *mut c_void };
    let mut out: *mut VTDecompressionSession = ptr::null_mut();
    let status = unsafe {
        VTDecompressionSession::create(None, format, Some(spec.as_opaque()), Some(attrs.as_opaque()), &callback, NonNull::from(&mut out))
    };
    let session = NonNull::new(out).filter(|_| status == 0).with_context(|| format!("couldn't create the H.264 decoder (VideoToolbox error {status})"))?;
    let session = unsafe { CFRetained::from_raw(session) };
    unsafe {
        // Not a real-time stream: decode as fast as it can.
        VTSessionSetProperty(&session, kVTDecompressionPropertyKey_RealTime, Some(CFBoolean::new(false)));
        // Downscaling averages every source pixel (no aliasing in thumbnails).
        let transfer = CFDictionary::<CFString, CFType>::from_slices(&[kVTPixelTransferPropertyKey_DownsamplingMode], &[kVTDownsamplingMode_Average.as_ref()]);
        VTSessionSetProperty(&session, kVTDecompressionPropertyKey_PixelTransferProperties, Some(transfer.as_ref()));
    }
    Ok(session)
}

/// One frame's bytes (AVCC, as stored) as a sample buffer at `pts`.
fn sample_buffer(format: &CMFormatDescription, data: &[u8], pts: i64, timescale: u32) -> Result<CFRetained<CMSampleBuffer>> {
    unsafe {
        let len = data.len();
        let mut block: *mut CMBlockBuffer = ptr::null_mut();
        // The block allocates its own memory; the data is copied in (the
        // decoder may still read it after this returns).
        let status = CMBlockBuffer::create_with_memory_block(None, ptr::null_mut(), len, None, ptr::null(), 0, len, 0, NonNull::from(&mut block));
        let block = CFRetained::from_raw(NonNull::new(block).filter(|_| status == 0).context("no memory for a frame")?);
        let status = CMBlockBuffer::replace_data_bytes(NonNull::new_unchecked(data.as_ptr() as *mut c_void), &block, 0, len);
        ensure!(status == 0, "couldn't copy a frame");
        let timing = CMSampleTimingInfo { duration: kCMTimeInvalid, presentationTimeStamp: CMTime::new(pts, timescale as i32), decodeTimeStamp: kCMTimeInvalid };
        let mut sample: *mut CMSampleBuffer = ptr::null_mut();
        let status = CMSampleBuffer::create_ready(None, Some(&block), Some(format), 1, 1, &timing, 1, &len, NonNull::from(&mut sample));
        Ok(CFRetained::from_raw(NonNull::new(sample).filter(|_| status == 0).context("couldn't wrap a frame")?))
    }
}

/// VideoToolbox's output callback (on its own thread): file the frame by its time.
unsafe extern "C-unwind" fn on_decoded(
    refcon: *mut c_void,
    frame_tag: *mut c_void,
    status: i32,
    flags: VTDecodeInfoFlags,
    image: *mut CVImageBuffer,
    pts: CMTime,
    _duration: CMTime,
) {
    let Some(shared) = (unsafe { (refcon as *const Shared).as_ref() }) else { return };
    let mut out = shared.out.lock().unwrap();
    if !frame_tag.is_null() {
        out.pending = out.pending.saturating_sub(1);
    }
    match NonNull::new(image) {
        Some(img) if status == 0 && !flags.contains(VTDecodeInfoFlags::FrameDropped) => {
            let buf = unsafe { CFRetained::retain(img) };
            out.frames.insert(pts.value, buf);
        }
        _ if status != 0 => out.error = Some(status),
        _ => {}
    }
    drop(out);
    shared.ready.notify_all();
}

/// A BGRA buffer's pixels as tight RGBA rows.
fn bgra_to_rgba(buf: &CVPixelBuffer) -> Result<Vec<u8>> {
    let (w, h) = (CVPixelBufferGetWidth(buf), CVPixelBufferGetHeight(buf));
    let mut rgba = vec![0u8; w * h * 4];
    unsafe {
        ensure!(CVPixelBufferLockBaseAddress(buf, CVPixelBufferLockFlags::ReadOnly) == 0, "couldn't read a frame");
        let base = CVPixelBufferGetBaseAddress(buf) as *const u8;
        let stride = CVPixelBufferGetBytesPerRow(buf);
        for y in 0..h {
            let src = std::slice::from_raw_parts(base.add(y * stride), w * 4);
            let dst = &mut rgba[y * w * 4..(y + 1) * w * 4];
            for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                d.copy_from_slice(&[s[2], s[1], s[0], 255]);
            }
        }
        CVPixelBufferUnlockBaseAddress(buf, CVPixelBufferLockFlags::ReadOnly);
    }
    Ok(rgba)
}
