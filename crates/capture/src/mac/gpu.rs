//! The GPU side of video on macOS: frames in IOSurfaces opened as Metal
//! textures without copying, and the compositor that draws the webcam over
//! the screen's picture straight into the encoder's own buffers.
//!
//! Everything stays NV12 (studio range, BT.709) end to end: ScreenCaptureKit
//! delivers it, cameras deliver it, VideoToolbox encodes it. The compositor
//! never converts colour: outside the webcam's box it copies the screen's
//! samples as they are, inside it resamples the camera's (averaging every
//! camera pixel an output pixel covers when shrinking, bilinear when
//! enlarging), luma and chroma each on their own grid.
//!
//! Without a webcam nothing here runs: the screen's own buffers go to the
//! encoder.

use std::ffi::c_void;
use std::ptr::NonNull;

use anyhow::{Context, Result, bail};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_core_foundation::{CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetHeight, CVPixelBufferGetIOSurface, CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth, CVPixelBufferPool,
    kCVPixelBufferHeightKey, kCVPixelBufferIOSurfacePropertiesKey,
    kCVPixelBufferMetalCompatibilityKey, kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};
use objc2_foundation::NSString;
use objc2_metal::{
    MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue, MTLComputeCommandEncoder, MTLComputePipelineState, MTLCreateSystemDefaultDevice, MTLDevice,
    MTLLibrary, MTLPixelFormat, MTLSize, MTLStorageMode, MTLTexture, MTLTextureDescriptor, MTLTextureUsage,
};

use crate::webcam::Placement;

/// The system's GPU, a command queue and the compositor's kernels.
pub(crate) struct Gpu {
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    luma: Retained<ProtocolObject<dyn MTLComputePipelineState>>,
    chroma: Retained<ProtocolObject<dyn MTLComputePipelineState>>,
}

// Metal devices, queues and pipeline states are thread-safe.
unsafe impl Send for Gpu {}
unsafe impl Sync for Gpu {}

/// Where the camera's picture goes, for the kernels: in output pixels of the
/// plane being written, and the part of the camera's picture it shows, in
/// camera pixels of that plane (flipped by swapping ends).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Overlay {
    /// The box in the frame: x0, y0, x1, y1 (the part inside the frame).
    dest: [f32; 4],
    /// The matching part of the camera's picture as turned: x0, y0, x1, y1
    /// (x1 < x0 when mirrored).
    src: [f32; 4],
    /// The camera's own picture size, and quarter turns clockwise.
    cam: [f32; 2],
    turns: u32,
    _pad: u32,
}

impl Gpu {
    pub(crate) fn new() -> Result<Self> {
        let device = MTLCreateSystemDefaultDevice().context("no Metal device")?;
        let queue = device.newCommandQueue().context("no Metal command queue")?;
        let library = device
            .newLibraryWithSource_options_error(&NSString::from_str(KERNELS), None)
            .map_err(|e| anyhow::anyhow!("couldn't build the compositor: {}", e.localizedDescription()))?;
        let pipeline = |name: &str| -> Result<Retained<ProtocolObject<dyn MTLComputePipelineState>>> {
            let f = library.newFunctionWithName(&NSString::from_str(name)).context("missing kernel")?;
            device.newComputePipelineStateWithFunction_error(&f).map_err(|e| anyhow::anyhow!("couldn't build the compositor: {}", e.localizedDescription()))
        };
        Ok(Self { luma: pipeline("luma")?, chroma: pipeline("chroma")?, device, queue })
    }

    /// Plane `plane` of an NV12 buffer as a texture over the same memory.
    fn plane(&self, buf: &CVPixelBuffer, plane: usize, write: bool) -> Result<Retained<ProtocolObject<dyn MTLTexture>>> {
        let surface = CVPixelBufferGetIOSurface(Some(buf)).context("the frame isn't in an IOSurface")?;
        let (w, h) = (CVPixelBufferGetWidth(buf), CVPixelBufferGetHeight(buf));
        let (format, w, h) = if plane == 0 { (MTLPixelFormat::R8Unorm, w, h) } else { (MTLPixelFormat::RG8Unorm, w.div_ceil(2), h.div_ceil(2)) };
        unsafe {
            let desc = MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(format, w, h, false);
            desc.setUsage(if write { MTLTextureUsage::ShaderWrite | MTLTextureUsage::ShaderRead } else { MTLTextureUsage::ShaderRead });
            desc.setStorageMode(MTLStorageMode::Shared);
            self.device.newTextureWithDescriptor_iosurface_plane(&desc, &surface, plane).context("Metal couldn't open a frame")
        }
    }

    /// Draw `camera` (cropped, placed and mirrored as `placement` says) over
    /// `screen` into `out`, all NV12 the same size as `out` except the camera.
    /// Waits for the GPU (well under a millisecond at 1440p).
    pub(crate) fn compose(&self, screen: &CVPixelBuffer, camera: Option<&CVPixelBuffer>, placement: Placement, out: &CVPixelBuffer) -> Result<()> {
        for b in [Some(screen), camera, Some(out)].into_iter().flatten() {
            if CVPixelBufferGetPixelFormatType(b) != kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange {
                bail!("the compositor takes NV12 frames");
            }
        }
        let (w, h) = (CVPixelBufferGetWidth(out) as u32, CVPixelBufferGetHeight(out) as u32);
        let cam_size = camera.map(|c| (CVPixelBufferGetWidth(c) as u32, CVPixelBufferGetHeight(c) as u32));
        let overlay = cam_size.and_then(|cs| overlay_rects(placement, cs, w, h));
        let cmd = self.queue.commandBuffer().context("no command buffer")?;
        let enc = cmd.computeCommandEncoder().context("no compute encoder")?;
        for plane in 0..2 {
            let (pipeline, scale) = if plane == 0 { (&self.luma, 1.0) } else { (&self.chroma, 0.5) };
            let src = self.plane(screen, plane, false)?;
            let dst = self.plane(out, plane, true)?;
            let cam = match camera {
                Some(c) => self.plane(c, plane, false)?,
                None => src.clone(),
            };
            // In this plane's pixels: chroma is half size both ways.
            let o = overlay.map(|o| Overlay { dest: o.dest.map(|v| v * scale), src: o.src.map(|v| v * scale), cam: o.cam.map(|v| v * scale), ..o }).unwrap_or_default();
            enc.setComputePipelineState(pipeline);
            unsafe {
                enc.setTexture_atIndex(Some(&src), 0);
                enc.setTexture_atIndex(Some(&cam), 1);
                enc.setTexture_atIndex(Some(&dst), 2);
                enc.setBytes_length_atIndex(NonNull::from(&o).cast::<c_void>(), std::mem::size_of::<Overlay>(), 0);
            }
            let size = MTLSize { width: dst.width(), height: dst.height(), depth: 1 };
            enc.dispatchThreads_threadsPerThreadgroup(size, MTLSize { width: 16, height: 16, depth: 1 });
        }
        enc.endEncoding();
        cmd.commit();
        cmd.waitUntilCompleted();
        Ok(())
    }
}

/// A pool of IOSurface-backed NV12 buffers for composed frames, the size of
/// the recording.
pub(crate) struct FramePool(CFRetained<CVPixelBufferPool>);
unsafe impl Send for FramePool {}
unsafe impl Sync for FramePool {}

impl FramePool {
    pub(crate) fn new(width: usize, height: usize) -> Result<Self> {
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
                CFNumber::new_i32(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange as i32).as_ref(),
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

/// Where the webcam's picture comes from and goes, in luma pixels, for a
/// frame of `width`×`height`: its box clipped to the frame, and the part of
/// the cropped picture that shows there (cut by the same share, from the
/// other side when mirrored). None if nothing of it is in the frame.
fn overlay_rects(p: Placement, (cw, ch): (u32, u32), width: u32, height: u32) -> Option<Overlay> {
    if p.is_hidden() {
        return None;
    }
    let (fw, fh) = (width as f32, height as f32);
    let axis = |d0: f32, d1: f32, size: f32, s0: f32, s1: f32, flip: bool| -> Option<(f32, f32, f32, f32)> {
        if d1 - d0 < 1.0 || s1 - s0 < 1.0 {
            return None;
        }
        // Even edges, so the chroma grid (half size) lines up.
        let (v0, v1) = ((d0.max(0.0) / 2.0).round() * 2.0, (d1.min(size) / 2.0).round() * 2.0);
        if v1 - v0 < 2.0 {
            return None;
        }
        let (t0, t1) = ((v0 - d0) / (d1 - d0), (v1 - d0) / (d1 - d0));
        let span = s1 - s0;
        let (a, b) = if flip { (s1 - t0 * span, s1 - t1 * span) } else { (s0 + t0 * span, s0 + t1 * span) };
        Some((v0, v1, a, b))
    };
    // Never stretched: the camera's picture fills the box with its own shape.
    let cam = [cw as f32, ch as f32];
    let (cw, ch) = p.turned((cw, ch));
    let [cl, ct, cr, cb] = p.fill_crop((cw, ch), (width, height));
    let (dx0, dx1, sx0, sx1) = axis(p.x * fw, (p.x + p.w) * fw, fw, cl * cw as f32, (1.0 - cr) * cw as f32, p.flip_h)?;
    let (dy0, dy1, sy0, sy1) = axis(p.y * fh, (p.y + p.h) * fh, fh, ct * ch as f32, (1.0 - cb) * ch as f32, p.flip_v)?;
    Some(Overlay { dest: [dx0, dy0, dx1, dy1], src: [sx0, sy0, sx1, sy1], cam, turns: (p.turns % 4) as u32, _pad: 0 })
}

/// One kernel per plane: copy the screen's sample, or inside the webcam's
/// box resample the camera's. Shrinking averages every camera pixel the
/// output pixel covers (up to 8×8 taps: a 1080p camera in a box a quarter of
/// a 1440p frame is ~3× smaller); enlarging is bilinear.
const KERNELS: &str = r#"
#include <metal_stdlib>
using namespace metal;

struct Overlay {
    float4 dest;
    float4 src;
    float2 cam;
    uint turns;
    uint pad;
};

// A point of the turned picture, in the camera's own.
float2 unturn(float2 p, constant Overlay& o) {
    switch (o.turns) {
        case 1: return float2(p.y, o.cam.y - p.x);
        case 2: return o.cam - p;
        case 3: return float2(o.cam.x - p.y, p.x);
        default: return p;
    }
}

template <typename T>
T resample(texture2d<float, access::read> cam, float2 p0, float2 p1, T zero) {
    // The output pixel covers camera pixels [p0, p1) (either way round).
    float2 lo = min(p0, p1), hi = max(p0, p1);
    float2 span = hi - lo;
    uint2 size = uint2(cam.get_width(), cam.get_height());
    if (span.x <= 1.0 && span.y <= 1.0) {
        // Enlarging: bilinear at the centre.
        float2 c = (lo + hi) * 0.5 - 0.5;
        float2 f = fract(c);
        int2 i = int2(floor(c));
        auto at = [&](int2 q) { return T(cam.read(uint2(clamp(q, int2(0), int2(size) - 1))).xy); };
        return mix(mix(at(i), at(i + int2(1, 0)), f.x), mix(at(i + int2(0, 1)), at(i + int2(1, 1)), f.x), f.y);
    }
    // Shrinking: area average over up to 8×8 taps spread across the span.
    int2 n = int2(clamp(ceil(span), 1.0, 8.0));
    float2 step = span / float2(n);
    T sum = zero;
    for (int y = 0; y < n.y; y++) {
        for (int x = 0; x < n.x; x++) {
            float2 c = lo + (float2(x, y) + 0.5) * step;
            sum += T(cam.read(uint2(clamp(int2(c), int2(0), int2(size) - 1))).xy);
        }
    }
    return sum / float(n.x * n.y);
}

kernel void luma(texture2d<float, access::read> screen [[texture(0)]],
                 texture2d<float, access::read> cam [[texture(1)]],
                 texture2d<float, access::write> out [[texture(2)]],
                 constant Overlay& o [[buffer(0)]],
                 uint2 gid [[thread_position_in_grid]]) {
    if (gid.x >= out.get_width() || gid.y >= out.get_height()) return;
    float2 p = float2(gid);
    if (p.x >= o.dest.x && p.x < o.dest.z && p.y >= o.dest.y && p.y < o.dest.w) {
        float2 scale = (o.src.zw - o.src.xy) / (o.dest.zw - o.dest.xy);
        float2 p0 = o.src.xy + (p - o.dest.xy) * scale;
        float y = resample<float2>(cam, unturn(p0, o), unturn(p0 + scale, o), float2(0)).x;
        out.write(float4(y, 0, 0, 1), gid);
    } else {
        out.write(screen.read(gid), gid);
    }
}

kernel void chroma(texture2d<float, access::read> screen [[texture(0)]],
                   texture2d<float, access::read> cam [[texture(1)]],
                   texture2d<float, access::write> out [[texture(2)]],
                   constant Overlay& o [[buffer(0)]],
                   uint2 gid [[thread_position_in_grid]]) {
    if (gid.x >= out.get_width() || gid.y >= out.get_height()) return;
    float2 p = float2(gid);
    if (p.x >= o.dest.x && p.x < o.dest.z && p.y >= o.dest.y && p.y < o.dest.w) {
        float2 scale = (o.src.zw - o.src.xy) / (o.dest.zw - o.dest.xy);
        float2 p0 = o.src.xy + (p - o.dest.xy) * scale;
        float2 cbcr = resample<float2>(cam, unturn(p0, o), unturn(p0 + scale, o), float2(0));
        out.write(float4(cbcr, 0, 1), gid);
    } else {
        out.write(screen.read(gid), gid);
    }
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webcam_box_in_frame() {
        let p = Placement { x: 0.75, y: 0.5, w: 0.25, h: 0.25, crop: [0.0; 4], flip_h: false, flip_v: false, turns: 0 };
        let o = overlay_rects(p, (1280, 720), 1920, 1080).unwrap();
        assert_eq!(o.dest, [1440.0, 540.0, 1920.0, 810.0]);
        assert_eq!(o.src, [0.0, 0.0, 1280.0, 720.0]);
    }

    #[test]
    fn mirrored_and_clipped() {
        // Half off the right edge, mirrored: the half that shows is the
        // camera's right half, drawn right to left.
        let p = Placement { x: 0.875, y: 0.0, w: 0.25, h: 0.25, crop: [0.0; 4], flip_h: true, flip_v: false, turns: 0 };
        let o = overlay_rects(p, (1280, 720), 1920, 1080).unwrap();
        assert_eq!(o.dest[0], 1680.0);
        assert_eq!(o.dest[2], 1920.0);
        assert_eq!((o.src[0], o.src[2]), (1280.0, 640.0));
    }

    #[test]
    fn turned_picture_takes_the_turned_shape() {
        // A 1280x720 camera turned a quarter is a 720x1280 picture: here in a
        // 720x1280-pixel box of a 1920x1080 frame, all of it showing.
        let p = Placement { x: 0.0, y: 0.0, w: 720.0 / 1920.0, h: 1280.0 / 1080.0, crop: [0.0; 4], flip_h: false, flip_v: false, turns: 1 };
        let o = overlay_rects(p, (1280, 720), 1920, 1080).unwrap();
        assert_eq!((o.cam, o.turns), ([1280.0, 720.0], 1));
        // Clipped by the frame's bottom: 1080 of the 1280 rows show.
        assert_eq!((o.src[0], o.src[2]), (0.0, 720.0));
        assert_eq!((o.src[1], o.src[3]), (0.0, 1080.0));
    }

    /// Turned clockwise on the real GPU: the camera's left half (dark) ends
    /// up as the picture's top half.
    #[test]
    fn compose_turns() {
        let gpu = Gpu::new().unwrap();
        let screen = FramePool::new(640, 360).unwrap().take().unwrap();
        plane_fill(&screen, 100, 128, 128);
        let cam = FramePool::new(320, 180).unwrap().take().unwrap();
        plane_fill(&cam, 200, 128, 128);
        // Left half of the camera dark.
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
        let out = FramePool::new(640, 360).unwrap().take().unwrap();
        // A 180x320 box (the turned camera's shape) at the frame's top-left.
        let p = Placement { x: 0.0, y: 0.0, w: 180.0 / 640.0, h: 320.0 / 360.0, crop: [0.0; 4], flip_h: false, flip_v: false, turns: 1 };
        gpu.compose(&screen, Some(&cam), p, &out).unwrap();
        let (y, _) = planes(&out);
        assert_eq!(y[40 * 640 + 90], 20, "top half is the camera's left");
        assert_eq!(y[280 * 640 + 90], 200, "bottom half is the camera's right");
    }

    #[test]
    fn hidden_draws_nothing() {
        assert!(overlay_rects(Placement::hidden(), (1280, 720), 1920, 1080).is_none());
    }

    /// The compositor on the real GPU: outside the box the screen's samples
    /// are copied exactly; inside, a flat-colour camera comes out that colour.
    #[test]
    fn compose_copies_and_draws() {
        let gpu = Gpu::new().unwrap();
        let fill = |w: usize, h: usize, y: u8, cb: u8, cr: u8| {
            let b = FramePool::new(w, h).unwrap().take().unwrap();
            plane_fill(&b, y, cb, cr);
            b
        };
        let screen = fill(640, 360, 100, 110, 120);
        let cam = fill(320, 180, 200, 50, 60);
        let out = FramePool::new(640, 360).unwrap().take().unwrap();
        let p = Placement { x: 0.5, y: 0.5, w: 0.25, h: 0.25, crop: [0.0; 4], flip_h: false, flip_v: false, turns: 0 };
        gpu.compose(&screen, Some(&cam), p, &out).unwrap();
        let (y, c) = planes(&out);
        assert_eq!(y[10 * 640 + 10], 100);
        assert_eq!(c[(10 * 320 + 10) * 2..][..2], [110, 120]);
        assert_eq!(y[(200) * 640 + 360], 200);
        assert_eq!(c[(100 * 320 + 180) * 2..][..2], [50, 60]);
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
