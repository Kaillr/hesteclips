//! Saving an edited clip's picture on the Mac's media engine and GPU, for
//! `media`'s export where the picture is made again (a new size, rate,
//! bitrate or crop, or frame blending), as on Linux (`linux::export`). The
//! frames are decoded by VideoToolbox (`decode`, the decoder playback uses:
//! NV12 in IOSurfaces), there cropped and scaled as they are by Metal, in
//! YUV, each plane on its own ([`Scaler`]: Lanczos, as ffmpeg's export
//! scaled, with Linux's weights); or, to blend them, mixed in light by the
//! recording's own blending (`blend`: blended just as live). Then they're
//! encoded by VideoToolbox's H.264 encoder, as ffmpeg's were
//! (`h264_videotoolbox`), into our MP4 writer (`mp4mux`). A frame neither
//! cropped, scaled nor blended goes to the encoder as it was decoded. The
//! sound is added after, by `media`, as on every platform.
//!
//! ffmpeg decoded on the CPU and cropped, scaled and blended there; only the
//! encoding was the media engine's.

use std::collections::VecDeque;
use std::ffi::{c_int, c_void};
use std::path::{Path, PathBuf};
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::rc::Rc;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_core_foundation::{CFArray, CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::{CMFormatDescription, CMSampleBuffer, CMTime, CMVideoFormatDescriptionGetH264ParameterSetAtIndex, kCMTimeInvalid, kCMVideoCodecType_H264};
use objc2_core_video::{CVPixelBuffer, kCVImageBufferColorPrimaries_ITU_R_709_2, kCVImageBufferTransferFunction_ITU_R_709_2, kCVImageBufferYCbCrMatrix_ITU_R_709_2};
use objc2_metal::{
    MTLBuffer, MTLCommandBuffer, MTLCommandBufferStatus, MTLCommandEncoder, MTLCommandQueue, MTLComputeCommandEncoder, MTLComputePipelineState,
    MTLCreateSystemDefaultDevice, MTLDevice, MTLResourceOptions, MTLSize, MTLTexture,
};
use objc2_video_toolbox::{
    VTCompressionSession, VTEncodeInfoFlags, VTSessionCopyProperty, VTSessionSetProperty, kVTCompressionPropertyKey_AllowFrameReordering,
    kVTCompressionPropertyKey_AverageBitRate, kVTCompressionPropertyKey_ColorPrimaries, kVTCompressionPropertyKey_DataRateLimits,
    kVTCompressionPropertyKey_ExpectedFrameRate, kVTCompressionPropertyKey_MaxKeyFrameInterval, kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration,
    kVTCompressionPropertyKey_ProfileLevel, kVTCompressionPropertyKey_RealTime, kVTCompressionPropertyKey_TransferFunction,
    kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder, kVTCompressionPropertyKey_YCbCrMatrix, kVTProfileLevel_H264_High_AutoLevel,
    kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder,
};

use super::blend::Blender;
use super::decode::{Decoder, Output};
use super::gpu::{FramePool, kernels, plane_texture};
use super::video::Buffer;
use crate::export::{Job, Timeline, lanczos};
use crate::mp4mux::{AvcConfig, Muxer, Spec};
use crate::shutter::Shutter;

/// Make `job`'s picture (no sound) at `dest`, an MP4. `Ok(false)` where it
/// can't be made here (a clip VideoToolbox can't decode, colours that would
/// have to be converted): ffmpeg makes it then.
pub(crate) fn render_video(job: &Job, dest: &Path, progress: &dyn Fn(f32)) -> Result<bool> {
    if job.out_fps == 0 || job.size.0 < 2 || job.size.1 < 2 || job.parts.is_empty() || job.fps <= 0.0 {
        return Ok(false);
    }
    let mut decoder = match Decoder::open(job.source, Output::Screen) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("export: VideoToolbox can't decode this clip ({e:#})");
            return Ok(false);
        }
    };
    // BT.601 (the app records BT.709; another app's clip may say so) would
    // have to be made the BT.709 the file says: ffmpeg does that.
    if decoder.bt601() {
        eprintln!("export: the clip's colours are BT.601");
        return Ok(false);
    }
    decoder.set_fps(job.fps);
    let source = decoder.source_size();
    let timeline = Timeline::new(job);
    if timeline.frames == 0 {
        bail!("nothing to save");
    }
    let whole = [0, 0, source.0, source.1];
    let crop = job.crop.unwrap_or(whole);
    let scaled = crop != whole || job.size != source;
    let shutter = job.blend.map(|(rate, shape)| Shutter::new(rate / job.out_fps, shape));
    // Scaling and blending on one queue: a frame's made before it's blended.
    let (mut scaler, mut blender) = (None, None);
    if scaled || shutter.is_some() {
        let device = MTLCreateSystemDefaultDevice().context("no Metal device")?;
        let queue = device.newCommandQueue().context("no Metal command queue")?;
        if scaled {
            scaler = Some(Scaler::new(&device, &queue, source, crop, job.size)?);
        }
        if let Some(s) = &shutter {
            blender = Some(Blender::queued(device, queue, job.size.0 as usize, job.size.1 as usize, s.frames_open())?);
        }
    }

    let partial = dest.with_extension("partial.mp4");
    let (tx, rx) = mpsc::channel();
    let writer = Writer::start(partial.clone(), job, rx);
    let encoder = match Encoder::start(job, tx) {
        Ok(e) => e,
        Err(e) => {
            let _ = writer.finish();
            let _ = std::fs::remove_file(&partial);
            return Err(e);
        }
    };
    eprintln!(
        "export: decoded and encoded by VideoToolbox ({}), {}",
        if encoder.hardware() { "the media engine" } else { "its software encoder" },
        match (&scaler, &blender) {
            (None, None) => "frames as decoded",
            (Some(_), None) => "cropped and scaled by Metal",
            (None, Some(_)) => "blended by Metal",
            (Some(_), Some(_)) => "cropped, scaled and blended by Metal",
        }
    );

    // The source frames wanted, in order, each once: decoded ahead on a
    // thread of their own while the GPU and the encoder work on the ones
    // before.
    let wanted = timeline.source_frames(job, shutter.as_ref());
    let (frames_tx, frames_rx) = mpsc::sync_channel::<Result<Frame>>(6);
    let decoding = thread::Builder::new().name("export decode".into()).spawn(move || {
        for i in wanted {
            // Playback short of the media engine's decoder (a clip being
            // watched meanwhile): it comes first.
            while crate::playback_short() {
                thread::sleep(Duration::from_millis(20));
            }
            let got = decoder.frame(i).and_then(|p| {
                let p = p.context("a frame the decoder doesn't have")?;
                let s = p.gpu.context("a frame not in NV12")?;
                Ok(Frame(retain(s.buffer())))
            });
            let failed = got.is_err();
            if frames_tx.send(got).is_err() || failed {
                break;
            }
        }
    });
    let decoding = match decoding {
        Ok(d) => d,
        Err(e) => {
            drop(encoder);
            let _ = writer.finish();
            let _ = std::fs::remove_file(&partial);
            return Err(e.into());
        }
    };

    let made = (|| -> Result<()> {
        // The next source frame, as the frame to encode (being made).
        let next = || -> Result<Made> {
            let Frame(decoded) = frames_rx.recv().map_err(|_| anyhow!("the decoder stopped"))??;
            match &scaler {
                Some(s) => s.scale(decoded),
                None => Ok(Made { frame: decoded, gpu: None }),
            }
        };
        let mut shown: Option<u64> = None;
        match (&shutter, &mut blender) {
            (None, _) => {
                let mut queue = Queue { encoder: &encoder, pending: VecDeque::new() };
                let mut picture: Option<Rc<Made>> = None;
                for k in 0..timeline.frames {
                    let i = timeline.source_frame(job, timeline.frame_time(k, job.out_fps));
                    if shown != Some(i) {
                        picture = Some(Rc::new(next()?));
                        shown = Some(i);
                    }
                    queue.push(picture.clone().context("no picture")?, k as i64)?;
                    progress(k as f32 / timeline.frames as f32);
                }
                queue.flush()?;
            }
            (Some(s), Some(blender)) => {
                let rate = job.blend.map_or(1, |b| b.0) as f64;
                let last = timeline.frames as i64 - 1;
                let mut closed = -1i64;
                let mut m = s.first_look(0);
                let mut queue = Queue { encoder: &encoder, pending: VecDeque::new() };
                let mut finish = |blender: &mut Blender, k: i64| -> Result<()> {
                    let made = blender.finish(k).context("a blended frame with nothing in it")?;
                    queue.push(Rc::new(Made { frame: retain(&made.0), gpu: blender.last_work() }), k)?;
                    progress(k as f32 / timeline.frames as f32);
                    Ok(())
                };
                while closed < last {
                    let (first, weights) = s.split(m);
                    if first > last {
                        break;
                    }
                    let i = timeline.source_frame(job, (m as f64 + 0.5) / rate);
                    if shown != Some(i) {
                        blender.hold(Arc::new(Buffer(next()?.frame)));
                        shown = Some(i);
                    }
                    for (k, w) in [(first, weights[0]), (first + 1, weights[1])] {
                        if w > 0.0 && (0..=last).contains(&k) {
                            blender.add(k, w);
                        }
                    }
                    // The frames no later look counts toward are done.
                    let (after, _) = s.split(m + 1);
                    while closed < (after - 1).min(last) {
                        closed += 1;
                        finish(blender, closed)?;
                    }
                    m += 1;
                }
                while closed < last {
                    closed += 1;
                    finish(blender, closed)?;
                }
                queue.flush()?;
            }
            (Some(_), None) => unreachable!("a blender for every shutter"),
        }
        Ok(())
    })();
    drop(frames_rx);
    let _ = decoding.join();
    // The encoder's frames all in, then the file finished: as many frames
    // as asked, in order (a frame dropped would stretch the one before it).
    let encoded = encoder.finish();
    let written = writer.finish();
    let result = made.and(encoded).and(written).and_then(|frames| {
        ensure!(frames == timeline.frames, "the encoder gave {frames} frames of {}", timeline.frames);
        Ok(std::fs::rename(&partial, dest)?)
    });
    if result.is_err() {
        let _ = std::fs::remove_file(&partial);
    }
    result.map(|()| true)
}

/// A decoded frame, sent from the decoding thread (CoreVideo's buffers are
/// thread-safe reference-counted objects; nothing writes to a decoded one).
struct Frame(CFRetained<CVPixelBuffer>);
unsafe impl Send for Frame {}

/// A frame to encode, and the GPU's work making it (if it's still to do).
struct Made {
    frame: CFRetained<CVPixelBuffer>,
    gpu: Option<Retained<ProtocolObject<dyn MTLCommandBuffer>>>,
}

impl Made {
    /// The frame, once the GPU has made it.
    fn ready(&self) -> Result<&CVPixelBuffer> {
        if let Some(cmd) = &self.gpu {
            cmd.waitUntilCompleted();
            if cmd.status() != MTLCommandBufferStatus::Completed {
                bail!("the GPU couldn't make a frame: {}", cmd.error().map_or("no reason given".into(), |e| e.localizedDescription().to_string()));
            }
        }
        Ok(&self.frame)
    }
}

/// Frames the GPU may still be making before they're encoded, at most. Each
/// waited for as it was made, a frame took 1.5 to 2 ms (the GPU idle between
/// them, the work itself a fraction of that): blending 10 s of 540 fps down
/// to 60 took 6.2 s instead of 4.5, at 720p 4.1 instead of 3.4. (Exports only
/// scaled are held to the encoder's pace either way.)
const AHEAD: usize = 4;

/// The frames for the encoder, in order, handed in a few behind the GPU,
/// each once it's made.
struct Queue<'a> {
    encoder: &'a Encoder,
    pending: VecDeque<(Rc<Made>, i64)>,
}

impl Queue<'_> {
    /// `made` as frame `k`.
    fn push(&mut self, made: Rc<Made>, k: i64) -> Result<()> {
        self.pending.push_back((made, k));
        while self.pending.len() > AHEAD {
            self.pop()?;
        }
        Ok(())
    }

    fn pop(&mut self) -> Result<()> {
        let Some((made, k)) = self.pending.pop_front() else { return Ok(()) };
        self.encoder.encode(made.ready()?, k)
    }

    /// Every frame handed in.
    fn flush(&mut self) -> Result<()> {
        while !self.pending.is_empty() {
            self.pop()?;
        }
        Ok(())
    }
}

fn retain(buf: &CVPixelBuffer) -> CFRetained<CVPixelBuffer> {
    unsafe { CFRetained::retain(NonNull::from(buf)) }
}

/// One way of scaling, for the kernels: each output sample's first source
/// sample and its weights (where they start in `firsts` and `weights`, and
/// how many a sample), the source samples it may read (`lo` to `hi`, the
/// edges repeated past them), and the samples made (`width` a row, `rows`).
/// Across, `row0` is the crop's first row in the source plane.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct Way {
    first_at: u32,
    weight_at: u32,
    taps: u32,
    lo: i32,
    hi: i32,
    row0: u32,
    width: u32,
    rows: u32,
}

/// Decoded frames cropped and scaled on the GPU as they are, NV12 to NV12,
/// each plane on its own, as Linux's export does (`linux::export`'s
/// `Nv12Scaler`): by Lanczos (3 lobes, as ffmpeg's export scaled), across
/// into rows of the new width, then down into a frame of the encoder's. The
/// same size is an exact copy.
struct Scaler {
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    across_luma: Retained<ProtocolObject<dyn MTLComputePipelineState>>,
    across_chroma: Retained<ProtocolObject<dyn MTLComputePipelineState>>,
    down_luma: Retained<ProtocolObject<dyn MTLComputePipelineState>>,
    down_chroma: Retained<ProtocolObject<dyn MTLComputePipelineState>>,
    firsts: Retained<ProtocolObject<dyn MTLBuffer>>,
    weights: Retained<ProtocolObject<dyn MTLBuffer>>,
    /// The crop's rows made the new width: luma, and the Cb, Cr pairs. The
    /// same for every frame: the next frame's made across only once this
    /// one's read down (Metal sees to it, frames not being waited for:
    /// buffers made by the device are tracked).
    luma_rows: Retained<ProtocolObject<dyn MTLBuffer>>,
    chroma_rows: Retained<ProtocolObject<dyn MTLBuffer>>,
    /// Luma across, chroma across, luma down, chroma down.
    ways: [Way; 4],
    /// The frames made (the encoder holds each a while).
    pool: FramePool,
}

// Metal devices, queues, pipelines and buffers are thread-safe.
unsafe impl Send for Scaler {}

impl Scaler {
    /// For frames `size` big, `crop` of them used (left, top, width, height:
    /// even), made `frame` big (even), on `queue`.
    fn new(
        device: &Retained<ProtocolObject<dyn MTLDevice>>,
        queue: &Retained<ProtocolObject<dyn MTLCommandQueue>>,
        size: (u32, u32),
        crop: [u32; 4],
        frame: (u32, u32),
    ) -> Result<Self> {
        let [cx, cy, cw, ch] = crop;
        ensure!(cw >= 2 && ch >= 2 && cx + cw <= size.0 && cy + ch <= size.1, "a crop outside the picture");
        let (fw, fh) = frame;
        let (device, queue) = (device.clone(), queue.clone());
        let mut k = kernels(&device, SCALE_KERNELS, &["across_luma", "across_chroma", "down_luma", "down_chroma"])?.into_iter();
        let (across_luma, across_chroma, down_luma, down_chroma) = (k.next().unwrap(), k.next().unwrap(), k.next().unwrap(), k.next().unwrap());
        // Luma across, chroma across, luma down, chroma down: from where in
        // the source, how many samples, into how many.
        let plan = [(cx, cw, fw), (cx / 2, cw / 2, fw / 2), (cy, ch, fh), (cy / 2, ch / 2, fh / 2)];
        let (mut firsts, mut weights) = (Vec::new(), Vec::new());
        let mut ways = [Way::default(); 4];
        for (n, &(at, len, out)) in plan.iter().enumerate() {
            let (f, w, taps) = lanczos(at, len, out);
            ways[n] = Way { first_at: firsts.len() as u32, weight_at: weights.len() as u32, taps, lo: at as i32, hi: (at + len - 1) as i32, ..Way::default() };
            firsts.extend_from_slice(&f);
            weights.extend_from_slice(&w);
        }
        // Across: the crop's rows, the frame's width; down: the frame.
        (ways[0].row0, ways[0].width, ways[0].rows) = (cy, fw, ch);
        (ways[1].row0, ways[1].width, ways[1].rows) = (cy / 2, fw / 2, ch / 2);
        (ways[2].width, ways[2].rows) = (fw, fh);
        (ways[3].width, ways[3].rows) = (fw / 2, fh / 2);
        let firsts = shared_buffer(&device, &firsts)?;
        let weights = shared_buffer(&device, &weights)?;
        let private = |bytes: usize| device.newBufferWithLength_options(bytes.max(4), MTLResourceOptions::StorageModePrivate).context("no GPU memory for scaling");
        let luma_rows = private(fw as usize * ch as usize * 4)?;
        let chroma_rows = private((fw / 2) as usize * (ch / 2) as usize * 8)?;
        let pool = FramePool::new(fw as usize, fh as usize)?;
        Ok(Self { device, queue, across_luma, across_chroma, down_luma, down_chroma, firsts, weights, luma_rows, chroma_rows, ways, pool })
    }

    /// `src` (NV12, the size given) cropped and scaled into a new frame, on
    /// the GPU: not waited for ([`Made::ready`]).
    fn scale(&self, src: CFRetained<CVPixelBuffer>) -> Result<Made> {
        let out = self.pool.take()?;
        let planes = [plane_texture(&self.device, &src, 0, false)?, plane_texture(&self.device, &src, 1, false)?];
        let made = [plane_texture(&self.device, &out, 0, true)?, plane_texture(&self.device, &out, 1, true)?];
        let cmd = self.queue.commandBuffer().context("no command buffer")?;
        // Across, then down (a pass of its own: it reads what across wrote).
        let passes: [[Step; 2]; 2] = [
            [(&self.across_luma, &planes[0], &self.luma_rows, &self.ways[0]), (&self.across_chroma, &planes[1], &self.chroma_rows, &self.ways[1])],
            [(&self.down_luma, &made[0], &self.luma_rows, &self.ways[2]), (&self.down_chroma, &made[1], &self.chroma_rows, &self.ways[3])],
        ];
        for pass in passes {
            let enc = cmd.computeCommandEncoder().context("no compute encoder")?;
            for (pipeline, texture, rows, way) in pass {
                enc.setComputePipelineState(pipeline);
                unsafe {
                    enc.setTexture_atIndex(Some(texture), 0);
                    enc.setBytes_length_atIndex(NonNull::from(way).cast::<c_void>(), std::mem::size_of::<Way>(), 0);
                    enc.setBuffer_offset_atIndex(Some(rows), 0, 1);
                    enc.setBuffer_offset_atIndex(Some(&self.firsts), 0, 2);
                    enc.setBuffer_offset_atIndex(Some(&self.weights), 0, 3);
                }
                let size = MTLSize { width: way.width as usize, height: way.rows as usize, depth: 1 };
                enc.dispatchThreads_threadsPerThreadgroup(size, MTLSize { width: 16, height: 16, depth: 1 });
            }
            enc.endEncoding();
        }
        // The decoded frame kept from going back to the decoder until the
        // GPU lets go of the block, done with it.
        let src = Frame(src);
        let keep = RcBlock::new(move |_: NonNull<ProtocolObject<dyn MTLCommandBuffer>>| {
            let _ = &src;
        });
        unsafe { cmd.addCompletedHandler(RcBlock::as_ptr(&keep)) };
        cmd.commit();
        Ok(Made { frame: out, gpu: Some(cmd) })
    }
}

/// A kernel run over a plane: its pipeline, the plane's texture (read across,
/// written down), the rows between and how it scales.
type Step<'a> = (&'a ProtocolObject<dyn MTLComputePipelineState>, &'a ProtocolObject<dyn MTLTexture>, &'a ProtocolObject<dyn MTLBuffer>, &'a Way);

/// A buffer the GPU reads, holding `data`.
fn shared_buffer<T: Copy>(device: &ProtocolObject<dyn MTLDevice>, data: &[T]) -> Result<Retained<ProtocolObject<dyn MTLBuffer>>> {
    let bytes = std::mem::size_of_val(data);
    let zero = [0u8; 4];
    let ptr = if bytes == 0 { zero.as_ptr().cast::<c_void>() } else { data.as_ptr().cast::<c_void>() };
    unsafe { device.newBufferWithBytes_length_options(NonNull::new(ptr as *mut c_void).unwrap(), bytes.max(4), MTLResourceOptions::StorageModeShared) }
        .context("no GPU memory for scaling")
}

/// Across: each of the crop's rows (luma, then the Cb, Cr pairs' rows) made
/// the frame's width, a sample a thread. Down: each sample of the frame from
/// the rows made across, rounded as Linux's (to even). In 0–255 throughout,
/// as Linux's shader is.
const SCALE_KERNELS: &str = r#"
#include <metal_stdlib>
using namespace metal;

struct Way {
    uint first_at;
    uint weight_at;
    uint taps;
    int lo;
    int hi;
    uint row0;
    uint width;
    uint rows;
};

kernel void across_luma(texture2d<float, access::read> src [[texture(0)]],
                        constant Way& w [[buffer(0)]],
                        device float* rows [[buffer(1)]],
                        device const int* firsts [[buffer(2)]],
                        device const float* weights [[buffer(3)]],
                        uint2 gid [[thread_position_in_grid]]) {
    if (gid.x >= w.width || gid.y >= w.rows) return;
    int first = firsts[w.first_at + gid.x];
    device const float* k = weights + w.weight_at + gid.x * w.taps;
    float sum = 0.0;
    for (uint t = 0; t < w.taps; t++) {
        uint x = uint(clamp(first + int(t), w.lo, w.hi));
        sum += src.read(uint2(x, w.row0 + gid.y)).r * 255.0 * k[t];
    }
    rows[gid.y * w.width + gid.x] = sum;
}

kernel void across_chroma(texture2d<float, access::read> src [[texture(0)]],
                          constant Way& w [[buffer(0)]],
                          device float2* rows [[buffer(1)]],
                          device const int* firsts [[buffer(2)]],
                          device const float* weights [[buffer(3)]],
                          uint2 gid [[thread_position_in_grid]]) {
    if (gid.x >= w.width || gid.y >= w.rows) return;
    int first = firsts[w.first_at + gid.x];
    device const float* k = weights + w.weight_at + gid.x * w.taps;
    float2 sum = 0.0;
    for (uint t = 0; t < w.taps; t++) {
        uint x = uint(clamp(first + int(t), w.lo, w.hi));
        sum += src.read(uint2(x, w.row0 + gid.y)).rg * 255.0 * k[t];
    }
    rows[gid.y * w.width + gid.x] = sum;
}

kernel void down_luma(texture2d<float, access::write> out [[texture(0)]],
                      constant Way& w [[buffer(0)]],
                      device const float* rows [[buffer(1)]],
                      device const int* firsts [[buffer(2)]],
                      device const float* weights [[buffer(3)]],
                      uint2 gid [[thread_position_in_grid]]) {
    if (gid.x >= w.width || gid.y >= w.rows) return;
    int first = firsts[w.first_at + gid.y];
    device const float* k = weights + w.weight_at + gid.y * w.taps;
    float sum = 0.0;
    for (uint t = 0; t < w.taps; t++) {
        uint r = uint(clamp(first + int(t), w.lo, w.hi) - w.lo);
        sum += rows[r * w.width + gid.x] * k[t];
    }
    out.write(float4(clamp(rint(sum), 0.0, 255.0) / 255.0, 0.0, 0.0, 1.0), gid);
}

kernel void down_chroma(texture2d<float, access::write> out [[texture(0)]],
                        constant Way& w [[buffer(0)]],
                        device const float2* rows [[buffer(1)]],
                        device const int* firsts [[buffer(2)]],
                        device const float* weights [[buffer(3)]],
                        uint2 gid [[thread_position_in_grid]]) {
    if (gid.x >= w.width || gid.y >= w.rows) return;
    int first = firsts[w.first_at + gid.y];
    device const float* k = weights + w.weight_at + gid.y * w.taps;
    float2 sum = 0.0;
    for (uint t = 0; t < w.taps; t++) {
        uint r = uint(clamp(first + int(t), w.lo, w.hi) - w.lo);
        sum += rows[r * w.width + gid.x] * k[t];
    }
    out.write(float4(clamp(rint(sum), 0.0, 255.0) / 255.0, 0.0, 1.0), gid);
}
"#;

/// An encoded frame: AVCC (4-byte lengths), its number, whether it's a
/// keyframe, and (keyframes) the parameter sets it needs.
struct Encoded {
    data: Vec<u8>,
    frame: i64,
    key: bool,
    config: Option<AvcConfig>,
}

/// Where VideoToolbox's callback puts what it encodes.
struct Sink {
    tx: Sender<Encoded>,
    fps: u32,
    /// A frame failed or was dropped: the export is no good.
    failed: AtomicBool,
}

/// VideoToolbox's H.264 encoder, set as ffmpeg's `h264_videotoolbox` was for
/// an export (`-b:v`, `-profile:v high`, and to fit a size `-maxrate`), but
/// with the recording's keyframes (a second apart, not ffmpeg's 12 frames).
/// Not real time: an export waits for every frame.
struct Encoder {
    session: CFRetained<VTCompressionSession>,
    /// Owned here; freed once the session is invalidated (no callback after).
    sink: *const Sink,
    fps: u32,
    completed: bool,
}

impl Encoder {
    fn start(job: &Job, tx: Sender<Encoded>) -> Result<Self> {
        let (w, h) = job.size;
        let fps = job.out_fps;
        unsafe {
            // The media engine where it can be, else the software encoder (a
            // size the hardware won't take): ffmpeg would have failed there.
            let spec = CFDictionary::<CFString, CFType>::from_slices(&[kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder], &[CFBoolean::new(true).as_ref()]);
            let sink = Box::into_raw(Box::new(Sink { tx, fps, failed: AtomicBool::new(false) }));
            let mut out: *mut VTCompressionSession = ptr::null_mut();
            let status = VTCompressionSession::create(
                None,
                w as i32,
                h as i32,
                kCMVideoCodecType_H264,
                Some(spec.as_opaque()),
                None,
                None,
                Some(on_encoded),
                sink.cast(),
                NonNull::from(&mut out),
            );
            let Some(session) = NonNull::new(out).filter(|_| status == 0) else {
                drop(Box::from_raw(sink));
                bail!("couldn't start VideoToolbox's H.264 encoder (error {status})");
            };
            let session = CFRetained::from_raw(session);
            let encoder = Self { session, sink, fps, completed: false };
            let gop = crate::keyframe_every(fps);
            let set = |key: &CFString, value: &CFType| -> Result<()> {
                let status = VTSessionSetProperty(&encoder.session, key, Some(value));
                ensure!(status == 0, "VideoToolbox refused {key} (error {status})");
                Ok(())
            };
            set(kVTCompressionPropertyKey_RealTime, CFBoolean::new(false))?;
            set(kVTCompressionPropertyKey_ProfileLevel, kVTProfileLevel_H264_High_AutoLevel)?;
            // No B-frames (as ffmpeg's): frames come out in order.
            set(kVTCompressionPropertyKey_AllowFrameReordering, CFBoolean::new(false))?;
            set(kVTCompressionPropertyKey_AverageBitRate, &CFNumber::new_i64(job.kbps as i64 * 1000))?;
            set(kVTCompressionPropertyKey_ExpectedFrameRate, &CFNumber::new_i32(fps as i32))?;
            set(kVTCompressionPropertyKey_MaxKeyFrameInterval, &CFNumber::new_i32(gop as i32))?;
            set(kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration, &CFNumber::new_f64(gop as f64 / fps.max(1) as f64))?;
            set(kVTCompressionPropertyKey_ColorPrimaries, kCVImageBufferColorPrimaries_ITU_R_709_2)?;
            set(kVTCompressionPropertyKey_TransferFunction, kCVImageBufferTransferFunction_ITU_R_709_2)?;
            set(kVTCompressionPropertyKey_YCbCrMatrix, kCVImageBufferYCbCrMatrix_ITU_R_709_2)?;
            if job.fit {
                // As ffmpeg's -maxrate: no more than the bitrate in any second.
                let limits = CFArray::<CFNumber>::from_retained_objects(&[CFNumber::new_i64(job.kbps as i64 * 1000 / 8), CFNumber::new_f64(1.0)]);
                set(kVTCompressionPropertyKey_DataRateLimits, limits.as_ref())?;
            }
            let status = encoder.session.prepare_to_encode_frames();
            ensure!(status == 0, "VideoToolbox's encoder couldn't get ready (error {status})");
            Ok(encoder)
        }
    }

    fn sink(&self) -> &Sink {
        unsafe { &*self.sink }
    }

    /// Whether the media engine encodes (else VideoToolbox's software encoder).
    fn hardware(&self) -> bool {
        let mut value: *const CFType = ptr::null();
        let status = unsafe { VTSessionCopyProperty(&self.session, kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder, None, (&mut value as *mut *const CFType).cast()) };
        let Some(value) = NonNull::new(value as *mut CFType).filter(|_| status == 0) else { return false };
        let value = unsafe { CFRetained::from_raw(value) };
        value.downcast_ref::<CFBoolean>().is_some_and(|b| b.as_bool())
    }

    /// Encode `frame` as frame `n`. VideoToolbox holds the call while it's
    /// full: an export skips nothing.
    fn encode(&self, frame: &CVPixelBuffer, n: i64) -> Result<()> {
        ensure!(!self.sink().failed.load(Ordering::Relaxed), "VideoToolbox's encoder failed");
        let fps = self.fps as i32;
        let status = unsafe { self.session.encode_frame(frame, CMTime::new(n, fps), CMTime::new(1, fps), None, ptr::null_mut(), ptr::null_mut()) };
        ensure!(status == 0, "VideoToolbox's encoder refused a frame (error {status})");
        Ok(())
    }

    /// Encode what's in flight; everything has been handed to the writer.
    fn finish(mut self) -> Result<()> {
        let status = unsafe { self.session.complete_frames(kCMTimeInvalid) };
        self.completed = true;
        ensure!(status == 0, "VideoToolbox's encoder couldn't finish (error {status})");
        ensure!(!self.sink().failed.load(Ordering::Relaxed), "VideoToolbox's encoder failed");
        Ok(())
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            if !self.completed {
                self.session.complete_frames(kCMTimeInvalid);
            }
            self.session.invalidate();
            // No callback can run after invalidate returns; dropping the
            // sender ends the writer's input.
            drop(Box::from_raw(self.sink as *mut Sink));
        }
    }
}

/// VideoToolbox's output callback (on its own thread): the frame to the writer.
unsafe extern "C-unwind" fn on_encoded(ctx: *mut c_void, _frame: *mut c_void, status: i32, flags: VTEncodeInfoFlags, sample: *mut CMSampleBuffer) {
    let Some(sink) = (unsafe { (ctx as *const Sink).as_ref() }) else { return };
    let sample = unsafe { sample.as_ref() }.filter(|_| status == 0 && !flags.contains(VTEncodeInfoFlags::FrameDropped));
    match sample.map(|s| encoded(s, sink.fps)) {
        Some(Ok(e)) => {
            let _ = sink.tx.send(e);
        }
        Some(Err(e)) => {
            eprintln!("export: {e:#}");
            sink.failed.store(true, Ordering::Relaxed);
        }
        None => {
            eprintln!("export: VideoToolbox dropped a frame (error {status})");
            sink.failed.store(true, Ordering::Relaxed);
        }
    }
}

/// An encoded sample's bytes, number and parameter sets.
fn encoded(sample: &CMSampleBuffer, fps: u32) -> Result<Encoded> {
    let block = unsafe { sample.data_buffer() }.context("an encoded frame with no data")?;
    let len = unsafe { block.data_length() };
    let mut data = vec![0u8; len];
    if len > 0 {
        let status = unsafe { block.copy_data_bytes(0, len, NonNull::new(data.as_mut_ptr().cast()).unwrap()) };
        ensure!(status == 0, "couldn't read an encoded frame (error {status})");
    }
    let pts = unsafe { sample.presentation_time_stamp() };
    let frame = (pts.value as f64 * fps as f64 / pts.timescale.max(1) as f64).round() as i64;
    let key = crate::sck::is_keyframe(sample);
    let config = match key {
        true => {
            let format = unsafe { sample.format_description() }.context("a keyframe with no format")?;
            Some(avc_config(&format)?)
        }
        false => None,
    };
    Ok(Encoded { data, frame, key, config })
}

/// The SPS and PPS the encoder's frames need (it writes 4-byte lengths).
fn avc_config(format: &CMFormatDescription) -> Result<AvcConfig> {
    let set = |i: usize| -> Result<Vec<u8>> {
        let (mut at, mut len, mut count, mut length_size): (*const u8, usize, usize, c_int) = (ptr::null(), 0, 0, 0);
        let status = unsafe { CMVideoFormatDescriptionGetH264ParameterSetAtIndex(format, i, &mut at, &mut len, &mut count, &mut length_size) };
        ensure!(status == 0 && !at.is_null() && count >= 2, "the encoder's parameter sets aren't readable (error {status})");
        ensure!(length_size == 4, "the encoder wrote {length_size}-byte lengths");
        Ok(unsafe { std::slice::from_raw_parts(at, len) }.to_vec())
    };
    Ok(AvcConfig { sps: set(0)?, pps: set(1)? })
}

/// Writes the encoded frames into the MP4 as they come, on a thread of its
/// own; says how many it wrote.
struct Writer(JoinHandle<Result<u64>>);

impl Writer {
    fn start(path: PathBuf, job: &Job, rx: Receiver<Encoded>) -> Self {
        let spec = Spec { width: job.size.0, height: job.size.1, fps: job.out_fps, audio_titles: Vec::new(), audio_bitrate: 0, comment: String::new(), mov: false, reorder: 0 };
        Self(thread::spawn(move || {
            let mut mux: Option<Muxer> = None;
            let mut frames = 0u64;
            for e in rx {
                ensure!(e.frame == frames as i64, "the encoder gave frame {} where {frames} was next", e.frame);
                if mux.is_none() {
                    ensure!(e.key, "the encoder's first frame isn't a keyframe");
                    mux = Some(Muxer::create(&path, &spec, e.config.clone().context("no parameter sets")?, true)?);
                }
                let mux = mux.as_mut().context("no file")?;
                mux.video(Arc::from(e.data), e.frame as f64 / spec.fps as f64, e.key)?;
                frames += 1;
            }
            mux.context("nothing was encoded")?.finish()?;
            Ok(frames)
        }))
    }

    /// Wait for the last frame (once the encoder's gone) and the file.
    fn finish(self) -> Result<u64> {
        self.0.join().map_err(|_| anyhow!("the file writer crashed"))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_core_video::*;

    /// A frame of `w`×`h` NV12 whose samples are `f(x, y, plane)`.
    fn frame(w: usize, h: usize, f: impl Fn(usize, usize, usize) -> u8) -> CFRetained<CVPixelBuffer> {
        let b = FramePool::new(w, h).unwrap().take().unwrap();
        unsafe {
            CVPixelBufferLockBaseAddress(&b, CVPixelBufferLockFlags(0));
            for plane in 0..2 {
                let base = CVPixelBufferGetBaseAddressOfPlane(&b, plane) as *mut u8;
                let stride = CVPixelBufferGetBytesPerRowOfPlane(&b, plane);
                let cols = CVPixelBufferGetWidthOfPlane(&b, plane) * (plane + 1);
                for y in 0..CVPixelBufferGetHeightOfPlane(&b, plane) {
                    for x in 0..cols {
                        *base.add(y * stride + x) = f(x, y, plane);
                    }
                }
            }
            CVPixelBufferUnlockBaseAddress(&b, CVPixelBufferLockFlags(0));
        }
        b
    }

    /// A frame's planes, tight: luma, then the Cb, Cr pairs.
    fn planes(b: &CVPixelBuffer) -> [Vec<u8>; 2] {
        unsafe {
            CVPixelBufferLockBaseAddress(b, CVPixelBufferLockFlags::ReadOnly);
            let out = [0, 1].map(|plane| {
                let base = CVPixelBufferGetBaseAddressOfPlane(b, plane) as *const u8;
                let stride = CVPixelBufferGetBytesPerRowOfPlane(b, plane);
                let cols = CVPixelBufferGetWidthOfPlane(b, plane) * (plane + 1);
                (0..CVPixelBufferGetHeightOfPlane(b, plane)).flat_map(|y| std::slice::from_raw_parts(base.add(y * stride), cols).to_vec()).collect()
            });
            CVPixelBufferUnlockBaseAddress(b, CVPixelBufferLockFlags::ReadOnly);
            out
        }
    }

    fn scaler(size: (u32, u32), crop: [u32; 4], frame: (u32, u32)) -> Scaler {
        let device = MTLCreateSystemDefaultDevice().unwrap();
        let queue = device.newCommandQueue().unwrap();
        Scaler::new(&device, &queue, size, crop, frame).unwrap()
    }

    /// Detail everywhere: no two neighbours alike, in either plane.
    fn busy(x: usize, y: usize, plane: usize) -> u8 {
        ((x * 37 + y * 101 + plane * 59 + (x * y) % 13 * 17) % 220 + 16) as u8
    }

    /// The scaling as Linux's shader does it, on the CPU: `samples` (one
    /// plane, `comps` values a sample, `w` samples a row) cropped to
    /// `crop` and made `out` big.
    fn scaled_on_cpu(samples: &[u8], w: usize, comps: usize, crop: [u32; 4], out: (u32, u32)) -> Vec<u8> {
        let [cx, cy, cw, ch] = crop;
        let (across, down) = (lanczos(cx, cw, out.0), lanczos(cy, ch, out.1));
        let mut rows = vec![0f32; out.0 as usize * ch as usize * comps];
        for y in 0..ch as usize {
            for x in 0..out.0 as usize {
                for c in 0..comps {
                    let mut sum = 0.0;
                    for t in 0..across.2 as usize {
                        let sx = (across.0[x] + t as i32).clamp(cx as i32, (cx + cw - 1) as i32) as usize;
                        sum += samples[((cy as usize + y) * w + sx) * comps + c] as f32 * across.1[x * across.2 as usize + t];
                    }
                    rows[(y * out.0 as usize + x) * comps + c] = sum;
                }
            }
        }
        let mut made = vec![0u8; (out.0 * out.1) as usize * comps];
        for y in 0..out.1 as usize {
            for x in 0..out.0 as usize {
                for c in 0..comps {
                    let mut sum = 0.0;
                    for t in 0..down.2 as usize {
                        let r = (down.0[y] + t as i32).clamp(cy as i32, (cy + ch - 1) as i32) as usize - cy as usize;
                        sum += rows[(r * out.0 as usize + x) * comps + c] * down.1[y * down.2 as usize + t];
                    }
                    made[(y * out.0 as usize + x) * comps + c] = sum.round_ties_even().clamp(0.0, 255.0) as u8;
                }
            }
        }
        made
    }

    /// Cropped and scaled on the GPU as Linux's shader does it (here on the
    /// CPU), each plane: shrunk, grown and cropped. Off by one at most where
    /// the GPU's float sums round the other way.
    #[test]
    fn scaled_as_linux_scales() {
        let src = frame(320, 180, busy);
        let [luma, chroma] = planes(&src);
        for (crop, out) in [([0, 0, 320, 180], (160, 90)), ([0, 0, 320, 180], (480, 270)), ([64, 32, 192, 108], (128, 72)), ([2, 4, 256, 144], (320, 180))] {
            let made = scaler((320, 180), crop, out).scale(src.clone()).unwrap();
            let [y, c] = planes(made.ready().unwrap());
            let want_y = scaled_on_cpu(&luma, 320, 1, crop, out);
            let [cx, cy, cw, ch] = crop;
            let want_c = scaled_on_cpu(&chroma, 160, 2, [cx / 2, cy / 2, cw / 2, ch / 2], (out.0 / 2, out.1 / 2));
            for (got, want, plane) in [(&y, &want_y, "luma"), (&c, &want_c, "chroma")] {
                assert_eq!(got.len(), want.len());
                let worst = got.iter().zip(want).map(|(a, b)| a.abs_diff(*b)).max().unwrap();
                let off = got.iter().zip(want).filter(|(a, b)| a != b).count();
                assert!(worst <= 1 && off * 1000 <= got.len(), "{crop:?} to {out:?}, {plane}: {off} samples off, by up to {worst}");
            }
        }
    }

    /// Frames made one after another, none waited for, each come out as
    /// its own: the rows between are the same buffer for all of them.
    #[test]
    fn frames_not_waited_for_are_each_their_own() {
        let s = scaler((320, 180), [0, 0, 320, 180], (160, 90));
        let sources: Vec<_> = (0..8).map(|n| frame(320, 180, move |x, y, p| busy(x + n * 7, y + n * 3, p))).collect();
        let made: Vec<Made> = sources.iter().map(|f| s.scale(f.clone()).unwrap()).collect();
        for (src, m) in sources.iter().zip(&made) {
            let [luma, _] = planes(src);
            let [y, _] = planes(m.ready().unwrap());
            let want = scaled_on_cpu(&luma, 320, 1, [0, 0, 320, 180], (160, 90));
            assert!(y.iter().zip(&want).all(|(a, b)| a.abs_diff(*b) <= 1), "a frame came out mixed with another");
        }
    }

    /// A crop at the same size is the samples themselves.
    #[test]
    fn a_crop_is_its_samples() {
        let src = frame(320, 180, busy);
        let made = scaler((320, 180), [32, 20, 200, 120], (200, 120)).scale(src).unwrap();
        let [y, c] = planes(made.ready().unwrap());
        assert!((0..120).all(|r| (0..200).all(|x| y[r * 200 + x] == busy(x + 32, r + 20, 0))), "luma copied");
        assert!((0..60).all(|r| (0..200).all(|x| c[r * 200 + x] == busy(x + 32, r + 10, 1))), "chroma copied");
    }
}
