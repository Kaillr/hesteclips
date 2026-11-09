//! The GPU side of video on macOS: frames in IOSurfaces opened as Metal
//! textures without copying, and the compositor that draws a frame's layers
//! (`crate::layers`: the screen, webcams, other displays, windows, pictures,
//! words, colours, web pages, videos) straight into the encoder's own
//! buffers, bottom first.
//!
//! The frame stays NV12 (studio range, BT.709) end to end: ScreenCaptureKit
//! delivers it, cameras and video files deliver it, VideoToolbox encodes it.
//! A layer in NV12 covers its place: where the screen fills the frame its
//! samples are copied as they are; in a box of its own it's resampled
//! (averaging every pixel an output pixel covers when shrinking, bilinear
//! when enlarging), luma and chroma each on their own grid. A layer in BGRA
//! (a picture, words, a colour, a web page) is blended over what's under it
//! by its alpha, as its colours are (gamma-encoded, as every compositor
//! blends them): a blend of R'G'B' is the same blend of Y'CbCr, which is
//! made from them by a sum. Each chroma sample takes the four pixels it
//! covers, so a layer goes on any pixel, words moved a pixel move a pixel.
//!
//! Every layer is drawn by one pass over each plane, each output sample
//! written once; past [`MAX_DRAWS`] layers, a pass more over what's made.
//!
//! With only the screen, filling the frame, nothing here runs: the screen's
//! own buffers go to the encoder.

use std::ffi::c_void;
use std::ptr::NonNull;

use anyhow::{Context, Result, bail};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_core_foundation::{CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight, CVPixelBufferGetIOSurface, CVPixelBufferGetPixelFormatType,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferPool, CVPixelBufferUnlockBaseAddress, kCVPixelBufferHeightKey,
    kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferMetalCompatibilityKey, kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey,
    kCVPixelFormatType_32BGRA, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};
use objc2_foundation::NSString;
use objc2_metal::{
    MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue, MTLComputeCommandEncoder, MTLComputePipelineState, MTLCreateSystemDefaultDevice, MTLDevice,
    MTLLibrary, MTLPixelFormat, MTLSize, MTLStorageMode, MTLTexture, MTLTextureDescriptor, MTLTextureUsage,
};

use crate::webcam::Placement;

/// Layers drawn by one pass: each takes two of its textures (an NV12
/// picture's planes, or a BGRA picture twice), and macOS lets a kernel have
/// 31; two more are what's under (the pass before's) and what's made.
pub(crate) const MAX_DRAWS: usize = 12;

/// How a layer's picture goes over what's under it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Alpha {
    /// It covers its place (an NV12 picture: a screen, a camera, a video).
    Cover,
    /// Blended by its alpha, its colours as they are (a picture, words, a
    /// colour: as images and the app's own drawing have them).
    Straight,
    /// Blended by its alpha, its colours already multiplied by it (what
    /// the window server's pictures have: a web page's).
    Premultiplied,
}

/// One layer drawn into a frame: its picture (NV12, studio range; or BGRA),
/// the part of it used (in its own pixels, as it is, before it's turned),
/// where that goes in the frame (whole pixels, inside the frame), and how
/// it's turned and flipped on the way (turned, then flipped, as
/// `crate::ScreenPlacement::unturn` has it).
#[derive(Clone, Copy)]
pub(crate) struct Draw<'a> {
    pub picture: &'a CVPixelBuffer,
    /// x0, y0, x1, y1.
    pub src: [f32; 4],
    /// x0, y0, x1, y1.
    pub dest: [f32; 4],
    pub turns: u8,
    pub flip_h: bool,
    pub flip_v: bool,
    /// An NV12 picture the same size and in the same place: its samples
    /// taken as they are.
    pub copy: bool,
    pub alpha: Alpha,
    /// An NV12 picture in BT.601 colours (a video file's, standard
    /// definition): made BT.709 as it's drawn.
    pub bt601: bool,
}

impl<'a> Draw<'a> {
    /// `picture` (NV12) covering `src` of it, at `dest`.
    pub(crate) fn cover(picture: &'a CVPixelBuffer, src: [f32; 4], dest: [f32; 4]) -> Self {
        Self { picture, src, dest, turns: 0, flip_h: false, flip_v: false, copy: false, alpha: Alpha::Cover, bt601: false }
    }

    /// `picture` (BGRA, `alpha` as it says) blended over what's under it.
    pub(crate) fn blended(picture: &'a CVPixelBuffer, src: [f32; 4], dest: [f32; 4], alpha: Alpha) -> Self {
        Self { alpha, ..Self::cover(picture, src, dest) }
    }

    /// As `p` turns and flips it.
    pub(crate) fn turned(self, turns: u8, flip_h: bool, flip_v: bool) -> Self {
        Self { turns: turns % 4, flip_h, flip_v, ..self }
    }

    /// Whether it covers all of a `w`×`h` frame, so nothing under it shows.
    fn covers(&self, w: u32, h: u32) -> bool {
        self.alpha == Alpha::Cover && self.dest[0] <= 0.0 && self.dest[1] <= 0.0 && self.dest[2] >= w as f32 && self.dest[3] >= h as f32
    }
}

/// [`Draw`] as the kernels read it.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct DrawGpu {
    dest: [f32; 4],
    src: [f32; 4],
    turns: u32,
    flip_h: u32,
    flip_v: u32,
    /// 0: NV12, covering; 1: BGRA, straight alpha; 2: BGRA, premultiplied.
    kind: u32,
    copy: u32,
    bt601: u32,
    _pad: [u32; 2],
}

/// Where the screen's picture is drawn from (in the stream's frame) and to
/// (in the frame made), as x0, y0, x1, y1 in luma pixels; `copy` when it's
/// the same place, its samples taken as they are.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ScreenRects {
    pub src: [f32; 4],
    pub dest: [f32; 4],
    pub copy: bool,
    /// How it's turned and flipped from `src` to `dest` (see
    /// `crate::ScreenPlacement::unturn`).
    pub turns: u8,
    pub flip_h: bool,
    pub flip_v: bool,
}

impl ScreenRects {
    /// All of a `w`×`h` frame, as it is.
    pub(crate) fn copied(w: usize, h: usize) -> Self {
        let all = [0.0, 0.0, w as f32, h as f32];
        Self { src: all, dest: all, copy: true, turns: 0, flip_h: false, flip_v: false }
    }

    /// The screen's `picture` drawn so.
    pub(crate) fn draw(self, picture: &CVPixelBuffer) -> Draw<'_> {
        Draw { copy: self.copy, ..Draw::cover(picture, self.src, self.dest).turned(self.turns, self.flip_h, self.flip_v) }
    }
}

/// The system's GPU, a command queue and the compositor's kernels.
pub(crate) struct Compositor {
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    luma: Retained<ProtocolObject<dyn MTLComputePipelineState>>,
    chroma: Retained<ProtocolObject<dyn MTLComputePipelineState>>,
    /// In the kernels' unused texture slots.
    blank: Retained<ProtocolObject<dyn MTLTexture>>,
}

// Metal devices, queues, pipeline states and textures are thread-safe.
unsafe impl Send for Compositor {}
unsafe impl Sync for Compositor {}

impl Compositor {
    pub(crate) fn new() -> Result<Self> {
        let device = MTLCreateSystemDefaultDevice().context("no Metal device")?;
        let queue = device.newCommandQueue().context("no Metal command queue")?;
        let mut k = kernels(&device, KERNELS, &["layers_luma", "layers_chroma"])?.into_iter();
        let blank = unsafe {
            let desc = MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(MTLPixelFormat::R8Unorm, 1, 1, false);
            desc.setUsage(MTLTextureUsage::ShaderRead);
            desc.setStorageMode(MTLStorageMode::Private);
            device.newTextureWithDescriptor(&desc).context("no GPU memory")?
        };
        Ok(Self { luma: k.next().unwrap(), chroma: k.next().unwrap(), blank, device, queue })
    }

    /// Make a frame in `out` (NV12): `draws`, bottom first, on black. A
    /// buffer from `pool` holds what's made between passes, past
    /// [`MAX_DRAWS`] layers. Waits for the GPU (well under a millisecond at
    /// 1440p).
    pub(crate) fn compose(&self, draws: &[Draw], out: &CVPixelBuffer, pool: &FramePool) -> Result<()> {
        if CVPixelBufferGetPixelFormatType(out) != kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange {
            bail!("the compositor makes NV12 frames");
        }
        for d in draws {
            let f = CVPixelBufferGetPixelFormatType(d.picture);
            let ok = match d.alpha {
                Alpha::Cover => f == kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
                Alpha::Straight | Alpha::Premultiplied => f == kCVPixelFormatType_32BGRA,
            };
            if !ok {
                bail!("a layer's picture isn't NV12 (covering) or BGRA (blended)");
            }
        }
        let (w, h) = (CVPixelBufferGetWidth(out) as u32, CVPixelBufferGetHeight(out) as u32);
        // What's under the lowest layer that covers the frame doesn't show.
        let from = draws.iter().rposition(|d| d.covers(w, h)).unwrap_or(0);
        let draws = &draws[from..];
        let cmd = self.queue.commandBuffer().context("no command buffer")?;
        let mut under: Option<[Retained<ProtocolObject<dyn MTLTexture>>; 2]> = None;
        let batches: Vec<&[Draw]> = if draws.is_empty() { vec![&[]] } else { draws.chunks(MAX_DRAWS).collect() };
        // Kept till the GPU's done: each pass's frame, but the last's (`out`).
        let mut between = Vec::new();
        for (i, batch) in batches.iter().enumerate() {
            let target = if i + 1 == batches.len() {
                [plane_texture(&self.device, out, 0, true)?, plane_texture(&self.device, out, 1, true)?]
            } else {
                let b = pool.take()?;
                let t = [plane_texture(&self.device, &b, 0, true)?, plane_texture(&self.device, &b, 1, true)?];
                between.push(b);
                t
            };
            self.pass(&cmd, batch, under.as_ref(), &target)?;
            under = Some(target);
        }
        cmd.commit();
        cmd.waitUntilCompleted();
        drop(between);
        Ok(())
    }

    /// One pass of up to [`MAX_DRAWS`] layers over `under` (the pass
    /// before's, else black), into `target`'s planes.
    fn pass(
        &self,
        cmd: &ProtocolObject<dyn MTLCommandBuffer>,
        draws: &[Draw],
        under: Option<&[Retained<ProtocolObject<dyn MTLTexture>>; 2]>,
        target: &[Retained<ProtocolObject<dyn MTLTexture>>; 2],
    ) -> Result<()> {
        // Each layer's two textures: an NV12 picture's planes, a BGRA one twice.
        let mut textures = Vec::with_capacity(draws.len() * 2);
        let mut gpu = Vec::with_capacity(draws.len().max(1));
        for d in draws {
            match d.alpha {
                Alpha::Cover => {
                    textures.push(plane_texture(&self.device, d.picture, 0, false)?);
                    textures.push(plane_texture(&self.device, d.picture, 1, false)?);
                }
                Alpha::Straight | Alpha::Premultiplied => {
                    let t = bgra_texture(&self.device, d.picture)?;
                    textures.push(t.clone());
                    textures.push(t);
                }
            }
            gpu.push(DrawGpu {
                dest: d.dest,
                src: d.src,
                turns: u32::from(d.turns % 4),
                flip_h: u32::from(d.flip_h),
                flip_v: u32::from(d.flip_v),
                kind: match d.alpha {
                    Alpha::Cover => 0,
                    Alpha::Straight => 1,
                    Alpha::Premultiplied => 2,
                },
                copy: u32::from(d.copy),
                bt601: u32::from(d.bt601),
                _pad: [0; 2],
            });
        }
        if gpu.is_empty() {
            gpu.push(DrawGpu::default());
        }
        let info: [u32; 4] = [draws.len() as u32, u32::from(under.is_some()), 0, 0];
        let enc = cmd.computeCommandEncoder().context("no compute encoder")?;
        for (plane, pipeline) in [(0usize, &self.luma), (1, &self.chroma)] {
            enc.setComputePipelineState(pipeline);
            unsafe {
                for slot in 0..MAX_DRAWS * 2 {
                    enc.setTexture_atIndex(Some(textures.get(slot).unwrap_or(&self.blank)), slot);
                }
                enc.setTexture_atIndex(Some(under.map_or(&self.blank, |u| &u[plane])), MAX_DRAWS * 2);
                enc.setTexture_atIndex(Some(&target[plane]), MAX_DRAWS * 2 + 1);
                enc.setBytes_length_atIndex(NonNull::new(gpu.as_ptr() as *mut c_void).context("no draws")?, std::mem::size_of_val(gpu.as_slice()), 0);
                enc.setBytes_length_atIndex(NonNull::from(&info).cast::<c_void>(), std::mem::size_of_val(&info), 1);
            }
            let size = MTLSize { width: target[plane].width(), height: target[plane].height(), depth: 1 };
            enc.dispatchThreads_threadsPerThreadgroup(size, MTLSize { width: 16, height: 16, depth: 1 });
        }
        enc.endEncoding();
        Ok(())
    }
}

/// Plane `plane` of an NV12 buffer as a texture over the same memory.
pub(crate) fn plane_texture(
    device: &ProtocolObject<dyn MTLDevice>,
    buf: &CVPixelBuffer,
    plane: usize,
    write: bool,
) -> Result<Retained<ProtocolObject<dyn MTLTexture>>> {
    let surface = CVPixelBufferGetIOSurface(Some(buf)).context("the frame isn't in an IOSurface")?;
    let (w, h) = (CVPixelBufferGetWidth(buf), CVPixelBufferGetHeight(buf));
    let (format, w, h) = if plane == 0 { (MTLPixelFormat::R8Unorm, w, h) } else { (MTLPixelFormat::RG8Unorm, w.div_ceil(2), h.div_ceil(2)) };
    unsafe {
        let desc = MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(format, w, h, false);
        desc.setUsage(if write { MTLTextureUsage::ShaderWrite | MTLTextureUsage::ShaderRead } else { MTLTextureUsage::ShaderRead });
        desc.setStorageMode(MTLStorageMode::Shared);
        device.newTextureWithDescriptor_iosurface_plane(&desc, &surface, plane).context("Metal couldn't open a frame")
    }
}

/// A BGRA buffer as a texture over the same memory (read as RGBA).
pub(crate) fn bgra_texture(device: &ProtocolObject<dyn MTLDevice>, buf: &CVPixelBuffer) -> Result<Retained<ProtocolObject<dyn MTLTexture>>> {
    let surface = CVPixelBufferGetIOSurface(Some(buf)).context("the picture isn't in an IOSurface")?;
    let (w, h) = (CVPixelBufferGetWidth(buf), CVPixelBufferGetHeight(buf));
    unsafe {
        let desc = MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(MTLPixelFormat::BGRA8Unorm, w, h, false);
        desc.setUsage(MTLTextureUsage::ShaderRead);
        desc.setStorageMode(MTLStorageMode::Shared);
        device.newTextureWithDescriptor_iosurface_plane(&desc, &surface, 0).context("Metal couldn't open a picture")
    }
}

/// Build Metal kernels from source: one pipeline per name, in order.
pub(crate) fn kernels(
    device: &ProtocolObject<dyn MTLDevice>,
    source: &str,
    names: &[&str],
) -> Result<Vec<Retained<ProtocolObject<dyn MTLComputePipelineState>>>> {
    let library = device
        .newLibraryWithSource_options_error(&NSString::from_str(source), None)
        .map_err(|e| anyhow::anyhow!("couldn't build GPU kernels: {}", e.localizedDescription()))?;
    names
        .iter()
        .map(|name| {
            let f = library.newFunctionWithName(&NSString::from_str(name)).context("missing kernel")?;
            device.newComputePipelineStateWithFunction_error(&f).map_err(|e| anyhow::anyhow!("couldn't build GPU kernels: {}", e.localizedDescription()))
        })
        .collect()
}

/// A pool of IOSurface-backed NV12 buffers for composed frames, the size of
/// the recording.
pub(crate) struct FramePool(CFRetained<CVPixelBufferPool>);
unsafe impl Send for FramePool {}
unsafe impl Sync for FramePool {}

impl FramePool {
    pub(crate) fn new(width: usize, height: usize) -> Result<Self> {
        Self::of(width, height, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange)
    }

    /// Buffers `width`×`height` in `format`, IOSurface-backed, for Metal.
    fn of(width: usize, height: usize, format: u32) -> Result<Self> {
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
                CFNumber::new_i32(format as i32).as_ref(),
                CFNumber::new_i32(width as i32).as_ref(),
                CFNumber::new_i32(height as i32).as_ref(),
                surface_props.as_ref(),
                objc2_core_foundation::CFBoolean::new(true).as_ref(),
            ],
        );
        let mut pool: *mut CVPixelBufferPool = std::ptr::null_mut();
        let status = unsafe { CVPixelBufferPool::create(None, None, Some(attrs.as_opaque()), NonNull::from(&mut pool)) };
        let pool = NonNull::new(pool).filter(|_| status == 0).context("couldn't make frame buffers")?;
        Ok(Self(unsafe { CFRetained::from_raw(pool) }))
    }

    /// A free buffer (the pool grows when every one is in use).
    pub(crate) fn take(&self) -> Result<CFRetained<CVPixelBuffer>> {
        let mut buf: *mut CVPixelBuffer = std::ptr::null_mut();
        let status = unsafe { CVPixelBufferPool::create_pixel_buffer(None, &self.0, NonNull::from(&mut buf)) };
        let buf = NonNull::new(buf).filter(|_| status == 0).context("no frame buffer")?;
        Ok(unsafe { CFRetained::from_raw(buf) })
    }
}

/// A picture (`width`×`height`, BGRA rows `bgra`, straight alpha) in a
/// buffer of its own the compositor reads.
pub(crate) fn bgra_buffer(width: u32, height: u32, bgra: &[u8]) -> Result<CFRetained<CVPixelBuffer>> {
    let (w, h) = (width.max(1) as usize, height.max(1) as usize);
    anyhow::ensure!(bgra.len() >= w * h * 4, "the picture's pixels are missing");
    let buf = FramePool::of(w, h, kCVPixelFormatType_32BGRA)?.take()?;
    fill_bgra(&buf, w, h, bgra)?;
    Ok(buf)
}

/// `bgra` (`w`×`h`, rows without padding) into a BGRA buffer that size.
pub(crate) fn fill_bgra(buf: &CVPixelBuffer, w: usize, h: usize, bgra: &[u8]) -> Result<()> {
    unsafe {
        if CVPixelBufferLockBaseAddress(buf, CVPixelBufferLockFlags(0)) != 0 {
            bail!("couldn't fill a picture");
        }
        let base = CVPixelBufferGetBaseAddress(buf) as *mut u8;
        let stride = CVPixelBufferGetBytesPerRow(buf);
        let (bw, bh) = (CVPixelBufferGetWidth(buf).min(w), CVPixelBufferGetHeight(buf).min(h));
        if !base.is_null() {
            for row in 0..bh {
                std::ptr::copy_nonoverlapping(bgra.as_ptr().add(row * w * 4), base.add(row * stride), bw * 4);
            }
        }
        CVPixelBufferUnlockBaseAddress(buf, CVPixelBufferLockFlags(0));
    }
    Ok(())
}

/// Where a webcam's picture comes from and goes, for a frame of `frame`
/// pixels: the part of the camera's own picture used (unturned, as it
/// comes), and its box clipped to the frame, on whole pixels; drawn
/// turned and flipped as `p` says ([`Draw::turned`]). The picture fills
/// its box with its own shape (`Placement::fill_crop`), as cropped. None
/// if nothing of it is in the frame.
pub(crate) fn camera_rects(p: Placement, (cw, ch): (u32, u32), (fw, fh): (u32, u32)) -> Option<([f32; 4], [f32; 4])> {
    if p.is_hidden() || cw == 0 || ch == 0 {
        return None;
    }
    let (tw, th) = p.turned((cw, ch));
    let [cl, ct, cr, cb] = p.fill_crop((tw, th), (fw, fh));
    // One axis: the box `[b0, b1]`, the frame `size` long; the crop of the
    // turned picture `[c0, c1]` of `n`: the part of the box in the frame,
    // on whole pixels, and the part of the turned picture it shows.
    let axis = |b0: f32, b1: f32, size: f32, c0: f32, c1: f32, n: f32, flip: bool| -> Option<(f32, f32, f32, f32)> {
        if b1 - b0 < 1.0 || c1 - c0 <= 0.0 {
            return None;
        }
        let (v0, v1) = (b0.max(0.0).round(), b1.min(size).round());
        if v1 - v0 < 1.0 {
            return None;
        }
        let (t0, t1) = ((v0 - b0) / (b1 - b0), (v1 - b0) / (b1 - b0));
        // Shown flipped, the crop runs the other way.
        let (a, b) = if flip { (n - c1, n - c0) } else { (c0, c1) };
        let (s0, s1) = (a + t0 * (b - a), a + t1 * (b - a));
        let (u0, u1) = if flip { (n - s1, n - s0) } else { (s0, s1) };
        Some((v0, v1, u0, u1))
    };
    let (tw, th) = (tw as f32, th as f32);
    let (dx0, dx1, ux0, ux1) = axis(p.x * fw as f32, (p.x + p.w) * fw as f32, fw as f32, cl * tw, (1.0 - cr) * tw, tw, p.flip_h)?;
    let (dy0, dy1, uy0, uy1) = axis(p.y * fh as f32, (p.y + p.h) * fh as f32, fh as f32, ct * th, (1.0 - cb) * th, th, p.flip_v)?;
    // The turned picture's part, in the camera's own picture.
    let own = (cw as f32, ch as f32);
    let a = p.unturn((ux0, uy0), own);
    let b = p.unturn((ux1, uy1), own);
    Some(([a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1)], [dx0, dy0, dx1, dy1]))
}

/// One kernel per plane, every layer in one pass: at each sample, the
/// layers bottom first (on what's under, or black), each over what's
/// there where its box is. An NV12 layer covers it (its own sample,
/// resampled into its box: averaging every pixel an output pixel covers
/// when shrinking, up to 8×8 taps; bilinear when enlarging); a BGRA one is
/// blended by its alpha. A chroma sample takes the four pixels it covers:
/// of an NV12 layer, as many as are in its box; of a BGRA one, each
/// pixel's colour by its alpha.
const KERNELS: &str = r#"
#include <metal_stdlib>
using namespace metal;

#define MAX_TEX 24

struct Draw {
    float4 dest;
    float4 src;
    uint turns;
    uint flip_h;
    uint flip_v;
    uint kind;
    uint copy;
    uint bt601;
    uint2 pad;
};

typedef array<texture2d<float, access::read>, MAX_TEX> Textures;

constant float BLACK_Y = 16.0 / 255.0;
constant float2 BLACK_C = float2(128.0 / 255.0);

// A point of where a layer's drawn (0 to 1 across and down it) in the part
// of its picture used: flipped back, then turned back (it's turned, then
// flipped), as `ScreenPlacement::unturn`.
float2 src_point(constant Draw& d, float2 q) {
    if (d.flip_h != 0) q.x = 1.0 - q.x;
    if (d.flip_v != 0) q.y = 1.0 - q.y;
    switch (d.turns) {
        case 1: q = float2(q.y, 1.0 - q.x); break;
        case 2: q = 1.0 - q; break;
        case 3: q = float2(1.0 - q.y, q.x); break;
        default: break;
    }
    return d.src.xy + q * (d.src.zw - d.src.xy);
}

// `t` over the pixels [p0, p1) (either way round): bilinear at the middle
// when that's a pixel or less (enlarging), else the average of up to 8×8
// taps spread over it. `premul`: each tap's colour by its alpha first (a
// picture with straight alpha, averaged as light covering it is).
float4 resample(texture2d<float, access::read> t, float2 p0, float2 p1, bool premul) {
    float2 lo = min(p0, p1), hi = max(p0, p1);
    float2 span = hi - lo;
    int2 last = int2(t.get_width(), t.get_height()) - 1;
    auto at = [&](int2 q) {
        float4 c = t.read(uint2(clamp(q, int2(0), last)));
        if (premul) c.rgb *= c.a;
        return c;
    };
    if (span.x <= 1.0 && span.y <= 1.0) {
        float2 c = (lo + hi) * 0.5 - 0.5;
        float2 f = fract(c);
        int2 i = int2(floor(c));
        return mix(mix(at(i), at(i + int2(1, 0)), f.x), mix(at(i + int2(0, 1)), at(i + int2(1, 1)), f.x), f.y);
    }
    int2 n = int2(clamp(ceil(span), 1.0, 8.0));
    float2 step = span / float2(n);
    float4 sum = 0.0;
    for (int y = 0; y < n.y; y++) {
        for (int x = 0; x < n.x; x++) {
            sum += at(int2(lo + (float2(x, y) + 0.5) * step));
        }
    }
    return sum / float(n.x * n.y);
}

// Studio-range Y'CbCr to R'G'B' and back, BT.709 or BT.601.
float3 rgb_of(float y, float2 cbcr, bool bt601) {
    float l = (y * 255.0 - 16.0) / 219.0;
    float2 c = (cbcr * 255.0 - 128.0) / 224.0;
    if (bt601) return float3(l + 1.402 * c.y, l - 0.344136 * c.x - 0.714136 * c.y, l + 1.772 * c.x);
    return float3(l + 1.5748 * c.y, l - 0.1873 * c.x - 0.4681 * c.y, l + 1.8556 * c.x);
}
float luma_of(float3 rgb) { return (16.0 + 219.0 * dot(saturate(rgb), float3(0.2126, 0.7152, 0.0722))) / 255.0; }
float2 chroma_of(float3 rgb) {
    rgb = saturate(rgb);
    float l = dot(rgb, float3(0.2126, 0.7152, 0.0722));
    return float2(128.0 + 224.0 * (rgb.b - l) / 1.8556, 128.0 + 224.0 * (rgb.r - l) / 1.5748) / 255.0;
}

// Where the pixels [p, p1) of the frame (inside the layer's box) are in
// its picture: the corners of their footprint.
void footprint(constant Draw& d, float2 p, float2 p1, thread float2& s0, thread float2& s1) {
    float2 size = d.dest.zw - d.dest.xy;
    s0 = src_point(d, (p - d.dest.xy) / size);
    s1 = src_point(d, (p1 - d.dest.xy) / size);
}

// A BGRA layer's colour over the frame's pixel `p`: straight R'G'B' and alpha.
float4 bgra_at(texture2d<float, access::read> t, constant Draw& d, float2 p) {
    float2 s0, s1;
    footprint(d, p, p + 1.0, s0, s1);
    float4 c = resample(t, s0, s1, d.kind == 1);
    c.rgb = c.a > 0.0 ? c.rgb / c.a : float3(0.0);
    return c;
}

bool inside(float2 p, float4 r) {
    return p.x >= r.x && p.x < r.z && p.y >= r.y && p.y < r.w;
}

kernel void layers_luma(Textures tex [[texture(0)]],
                        texture2d<float, access::read> under [[texture(24)]],
                        texture2d<float, access::write> out [[texture(25)]],
                        constant Draw* draws [[buffer(0)]],
                        constant uint4& info [[buffer(1)]],
                        uint2 gid [[thread_position_in_grid]]) {
    if (gid.x >= out.get_width() || gid.y >= out.get_height()) return;
    float2 p = float2(gid);
    float y = info.y != 0 ? under.read(gid).x : BLACK_Y;
    for (uint k = 0; k < info.x; k++) {
        constant Draw& d = draws[k];
        if (!inside(p + 0.5, d.dest)) continue;
        if (d.kind == 0) {
            if (d.copy != 0) { y = tex[2 * k].read(gid).x; continue; }
            float2 s0, s1;
            footprint(d, p, p + 1.0, s0, s1);
            float v = resample(tex[2 * k], s0, s1, false).x;
            if (d.bt601 != 0) v = luma_of(rgb_of(v, resample(tex[2 * k + 1], s0 * 0.5, s1 * 0.5, false).xy, true));
            y = v;
        } else {
            float4 c = bgra_at(tex[2 * k], d, p);
            y = mix(y, luma_of(c.rgb), c.a);
        }
    }
    out.write(float4(y, 0.0, 0.0, 1.0), gid);
}

kernel void layers_chroma(Textures tex [[texture(0)]],
                          texture2d<float, access::read> under [[texture(24)]],
                          texture2d<float, access::write> out [[texture(25)]],
                          constant Draw* draws [[buffer(0)]],
                          constant uint4& info [[buffer(1)]],
                          uint2 gid [[thread_position_in_grid]]) {
    if (gid.x >= out.get_width() || gid.y >= out.get_height()) return;
    // The luma pixels this sample covers (one row or column at a frame's
    // odd edge).
    float2 b0 = float2(gid) * 2.0;
    float2 b1 = min(b0 + 2.0, float2(out.get_width(), out.get_height()) * 2.0);
    float pixels = (b1.x - b0.x) * (b1.y - b0.y);
    float2 c = info.y != 0 ? under.read(gid).xy : BLACK_C;
    for (uint k = 0; k < info.x; k++) {
        constant Draw& d = draws[k];
        float2 o0 = max(b0, d.dest.xy), o1 = min(b1, d.dest.zw);
        if (o1.x <= o0.x || o1.y <= o0.y) continue;
        if (d.kind == 0) {
            float2 v;
            if (d.copy != 0) {
                v = tex[2 * k + 1].read(gid).xy;
            } else {
                // Its chroma over the part of the sample in its box.
                float2 s0, s1;
                footprint(d, o0, o1, s0, s1);
                v = resample(tex[2 * k + 1], s0 * 0.5, s1 * 0.5, false).xy;
                if (d.bt601 != 0) v = chroma_of(rgb_of(resample(tex[2 * k], s0, s1, false).x, v, true));
            }
            c = mix(c, v, (o1.x - o0.x) * (o1.y - o0.y) / pixels);
        } else {
            // Each pixel's colour by its alpha, over what's there.
            float a = 0.0;
            float2 sum = 0.0;
            for (float py = o0.y; py < o1.y; py += 1.0) {
                for (float px = o0.x; px < o1.x; px += 1.0) {
                    float4 rgba = bgra_at(tex[2 * k], d, float2(px, py));
                    a += rgba.a;
                    sum += rgba.a * chroma_of(rgba.rgb);
                }
            }
            c = c * (1.0 - a / pixels) + sum / pixels;
        }
    }
    out.write(float4(c, 0.0, 1.0), gid);
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn p(x: f32, y: f32, w: f32, h: f32) -> Placement {
        Placement { x, y, w, h, crop: [0.0; 4], flip_h: false, flip_v: false, turns: 0, stretch: 1.0 }
    }

    #[test]
    fn webcam_box_in_frame() {
        let (src, dest) = camera_rects(p(0.75, 0.5, 0.25, 0.25), (1280, 720), (1920, 1080)).unwrap();
        assert_eq!(dest, [1440.0, 540.0, 1920.0, 810.0]);
        assert_eq!(src, [0.0, 0.0, 1280.0, 720.0]);
    }

    #[test]
    fn mirrored_and_clipped() {
        // Half off the right edge, mirrored: the half that shows is the
        // camera's right half (drawn mirrored).
        let (src, dest) = camera_rects(Placement { flip_h: true, ..p(0.875, 0.0, 0.25, 0.25) }, (1280, 720), (1920, 1080)).unwrap();
        assert_eq!((dest[0], dest[2]), (1680.0, 1920.0));
        assert_eq!((src[0], src[2]), (640.0, 1280.0));
    }

    #[test]
    fn turned_picture_takes_the_turned_shape() {
        // A 1280x720 camera turned a quarter is a 720x1280 picture: here in a
        // 720x1280-pixel box of a 1920x1080 frame. Clipped by the frame's
        // bottom, its top 1080 rows show: turned back, the camera's left
        // 1080 columns (turned clockwise, its left is the top).
        let q = Placement { turns: 1, ..p(0.0, 0.0, 720.0 / 1920.0, 1280.0 / 1080.0) };
        let (src, dest) = camera_rects(q, (1280, 720), (1920, 1080)).unwrap();
        assert_eq!(dest, [0.0, 0.0, 720.0, 1080.0]);
        assert_eq!(src, [0.0, 0.0, 1080.0, 720.0]);
    }

    #[test]
    fn hidden_draws_nothing() {
        assert!(camera_rects(Placement::hidden(), (1280, 720), (1920, 1080)).is_none());
        assert!(camera_rects(p(1.2, 0.0, 0.2, 0.2), (1280, 720), (1920, 1080)).is_none(), "off the frame");
    }

    fn nv12(w: usize, h: usize, y: u8, cb: u8, cr: u8) -> CFRetained<CVPixelBuffer> {
        let b = FramePool::new(w, h).unwrap().take().unwrap();
        plane_fill(&b, y, cb, cr);
        b
    }

    /// The webcam turned clockwise on the real GPU: the camera's left half
    /// (dark) ends up as the picture's top half.
    #[test]
    fn compose_turns() {
        let gpu = Compositor::new().unwrap();
        let pool = FramePool::new(640, 360).unwrap();
        let screen = nv12(640, 360, 100, 128, 128);
        let cam = nv12(320, 180, 200, 128, 128);
        unsafe {
            use objc2_core_video::*;
            CVPixelBufferLockBaseAddress(&cam, CVPixelBufferLockFlags(0));
            let base = CVPixelBufferGetBaseAddressOfPlane(&cam, 0) as *mut u8;
            let stride = CVPixelBufferGetBytesPerRowOfPlane(&cam, 0);
            for y in 0..180 {
                std::ptr::write_bytes(base.add(y * stride), 20, 160);
            }
            CVPixelBufferUnlockBaseAddress(&cam, CVPixelBufferLockFlags(0));
        }
        let out = pool.take().unwrap();
        // A 180x320 box (the turned camera's shape) at the frame's top-left.
        let q = Placement { turns: 1, ..p(0.0, 0.0, 180.0 / 640.0, 320.0 / 360.0) };
        let (src, dest) = camera_rects(q, (320, 180), (640, 360)).unwrap();
        let draws = [ScreenRects::copied(640, 360).draw(&screen), Draw::cover(&cam, src, dest).turned(q.turns, q.flip_h, q.flip_v)];
        gpu.compose(&draws, &out, &pool).unwrap();
        let (y, _) = planes(&out);
        assert_eq!(y[40 * 640 + 90], 20, "top half is the camera's left");
        assert_eq!(y[280 * 640 + 90], 200, "bottom half is the camera's right");
        assert_eq!(y[200 * 640 + 400], 100, "the screen beside it");
    }

    /// The screen turned and mirrored on the real GPU: turned clockwise
    /// and fitted, its left half (dark) is the top of an upright band, black
    /// beside it; mirrored, its left half is on the right.
    #[test]
    fn compose_turns_and_mirrors_the_screen() {
        let gpu = Compositor::new().unwrap();
        let pool = FramePool::new(640, 360).unwrap();
        let screen = nv12(640, 360, 200, 128, 128);
        unsafe {
            use objc2_core_video::*;
            CVPixelBufferLockBaseAddress(&screen, CVPixelBufferLockFlags(0));
            let base = CVPixelBufferGetBaseAddressOfPlane(&screen, 0) as *mut u8;
            let stride = CVPixelBufferGetBytesPerRowOfPlane(&screen, 0);
            for y in 0..360 {
                std::ptr::write_bytes(base.add(y * stride), 20, 320);
            }
            CVPixelBufferUnlockBaseAddress(&screen, CVPixelBufferLockFlags(0));
        }
        let out = pool.take().unwrap();
        let rects = |q: crate::ScreenPlacement| {
            let edges = |[x, y, w, h]: [u32; 4]| [x as f32, y as f32, (x + w) as f32, (y + h) as f32];
            let (src, dest) = q.place_turned((640, 360), (640, 360));
            ScreenRects { src: edges(src), dest: edges(dest), copy: false, turns: q.turns, flip_h: q.flip_h, flip_v: q.flip_v }
        };
        let turned = crate::ScreenPlacement { turns: 1, ..crate::ScreenPlacement::whole(crate::Framing::Fit) };
        gpu.compose(&[rects(turned).draw(&screen)], &out, &pool).unwrap();
        let (y, _) = planes(&out);
        assert_eq!(y[40 * 640 + 320], 20, "the band's top: the screen's left");
        assert_eq!(y[320 * 640 + 320], 200, "its bottom: the screen's right");
        assert_eq!(y[180 * 640 + 20], 16, "black beside it");
        let mirrored = crate::ScreenPlacement { flip_h: true, ..crate::ScreenPlacement::whole(crate::Framing::Stretch) };
        gpu.compose(&[rects(mirrored).draw(&screen)], &out, &pool).unwrap();
        let (y, _) = planes(&out);
        assert_eq!(y[180 * 640 + 100], 200, "the left: the screen's right");
        assert_eq!(y[180 * 640 + 540], 20, "the right: the screen's left");
    }

    /// On the real GPU: outside the box the screen's samples are copied
    /// exactly; inside, a flat-colour camera comes out that colour.
    #[test]
    fn compose_copies_and_draws() {
        let gpu = Compositor::new().unwrap();
        let pool = FramePool::new(640, 360).unwrap();
        let screen = nv12(640, 360, 100, 110, 120);
        let cam = nv12(320, 180, 200, 50, 60);
        let out = pool.take().unwrap();
        let (src, dest) = camera_rects(p(0.5, 0.5, 0.25, 0.25), (320, 180), (640, 360)).unwrap();
        gpu.compose(&[ScreenRects::copied(640, 360).draw(&screen), Draw::cover(&cam, src, dest)], &out, &pool).unwrap();
        let (y, c) = planes(&out);
        assert_eq!(y[10 * 640 + 10], 100);
        assert_eq!(c[(10 * 320 + 10) * 2..][..2], [110, 120]);
        assert_eq!(y[200 * 640 + 360], 200);
        assert_eq!(c[(100 * 320 + 180) * 2..][..2], [50, 60]);
    }

    /// A picture with straight alpha blended over the screen: opaque
    /// white is white, half-transparent red half over grey (as R'G'B'),
    /// clear shows what's under; a box starting on an odd pixel colours
    /// the chroma sample it half covers half as much.
    #[test]
    fn compose_blends_pictures_by_their_alpha() {
        let gpu = Compositor::new().unwrap();
        let pool = FramePool::new(64, 32).unwrap();
        let grey = nv12(64, 32, 126, 128, 128);
        // 12×8, in three bands 4 wide: opaque white, red at half alpha, clear.
        let band = [[255u8, 255, 255, 255], [0, 0, 255, 128], [0, 255, 0, 0]];
        let px: Vec<u8> = (0..8).flat_map(|_| (0..12).flat_map(move |x| band[x / 4])).collect();
        let pic = bgra_buffer(12, 8, &px).unwrap();
        let out = pool.take().unwrap();
        // At its own size, from (8, 8).
        let draws = [ScreenRects::copied(64, 32).draw(&grey), Draw::blended(&pic, [0.0, 0.0, 12.0, 8.0], [8.0, 8.0, 20.0, 16.0], Alpha::Straight)];
        gpu.compose(&draws, &out, &pool).unwrap();
        let (y, c) = planes(&out);
        assert_eq!(y[10 * 64 + 9], 235, "opaque white");
        let mid = (126.0 + (16.0 + 219.0 * 0.2126)) / 2.0;
        assert!((y[10 * 64 + 13] as f32 - mid).abs() <= 1.5, "half red over grey: {} not {mid}", y[10 * 64 + 13]);
        assert_eq!(y[10 * 64 + 17], 126, "clear: what's under");
        let cr = c[(5 * 32 + 6) * 2 + 1];
        assert!(cr > 160, "half red has red in it: {cr}");
        // Moved a pixel right, starting on an odd one: the sample at its
        // left edge has half of it.
        let draws = [ScreenRects::copied(64, 32).draw(&grey), Draw::blended(&pic, [0.0, 0.0, 12.0, 8.0], [9.0, 8.0, 21.0, 16.0], Alpha::Straight)];
        gpu.compose(&draws, &out, &pool).unwrap();
        let (y, c) = planes(&out);
        assert_eq!((y[10 * 64 + 8], y[10 * 64 + 9]), (126, 235), "it moved a pixel");
        // The sample over the white band's last pixel and the red one's
        // first: half as much red as one all over the red.
        let half = c[(5 * 32 + 6) * 2 + 1];
        assert!((half as i32 - (128 + (cr as i32 - 128) / 2)).abs() <= 2, "{half}, against {cr} all over it");
    }

    /// More layers than one pass draws: each still goes over the ones
    /// under it, the last in front.
    #[test]
    fn more_layers_than_a_pass() {
        let gpu = Compositor::new().unwrap();
        let pool = FramePool::new(64, 32).unwrap();
        let shades: Vec<_> = (0..MAX_DRAWS + 3).map(|i| nv12(8, 8, 40 + i as u8 * 10, 128, 128)).collect();
        let draws: Vec<Draw> = shades.iter().enumerate().map(|(i, b)| Draw::cover(b, [0.0, 0.0, 8.0, 8.0], [i as f32 * 2.0, 0.0, i as f32 * 2.0 + 8.0, 8.0])).collect();
        let out = pool.take().unwrap();
        gpu.compose(&draws, &out, &pool).unwrap();
        let (y, _) = planes(&out);
        let top = draws.len() - 1;
        assert_eq!(y[4 * 64 + top * 2 + 7], 40 + top as u8 * 10, "the last on top");
        assert_eq!(y[4 * 64 + 1], 40, "the first where nothing covers it");
        assert_eq!(y[4 * 64 + 60], 16, "black where none is");
    }

    fn plane_fill(b: &CVPixelBuffer, y: u8, cb: u8, cr: u8) {
        use objc2_core_video::*;
        unsafe {
            CVPixelBufferLockBaseAddress(b, CVPixelBufferLockFlags(0));
            for (plane, vals) in [(0usize, vec![y]), (1, vec![cb, cr])] {
                let base = CVPixelBufferGetBaseAddressOfPlane(b, plane) as *mut u8;
                let stride = CVPixelBufferGetBytesPerRowOfPlane(b, plane);
                let rows = CVPixelBufferGetHeightOfPlane(b, plane);
                let cols = CVPixelBufferGetWidthOfPlane(b, plane) * vals.len();
                for r in 0..rows {
                    for x in 0..cols {
                        *base.add(r * stride + x) = vals[x % vals.len()];
                    }
                }
            }
            CVPixelBufferUnlockBaseAddress(b, CVPixelBufferLockFlags(0));
        }
    }

    fn planes(b: &CVPixelBuffer) -> (Vec<u8>, Vec<u8>) {
        use objc2_core_video::*;
        unsafe {
            CVPixelBufferLockBaseAddress(b, CVPixelBufferLockFlags::ReadOnly);
            let mut out = Vec::new();
            for plane in 0..2 {
                let base = CVPixelBufferGetBaseAddressOfPlane(b, plane) as *const u8;
                let stride = CVPixelBufferGetBytesPerRowOfPlane(b, plane);
                let rows = CVPixelBufferGetHeightOfPlane(b, plane);
                let cols = CVPixelBufferGetWidthOfPlane(b, plane) * (plane + 1);
                let mut v = Vec::new();
                for r in 0..rows {
                    v.extend_from_slice(std::slice::from_raw_parts(base.add(r * stride), cols));
                }
                out.push(v);
            }
            CVPixelBufferUnlockBaseAddress(b, CVPixelBufferLockFlags::ReadOnly);
            (out.remove(0), out.remove(0))
        }
    }
}
