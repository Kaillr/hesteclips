//! Live frame blending on macOS: each frame is the average of a fixed number
//! of looks at the screen, evenly spaced over the frame's time (the blend
//! rate over the frame rate: 120 fps blending at 40 fps is 3), each weighed
//! the same, so every frame shows the same number of steps of motion.
//!
//! ScreenCaptureKit only sees what the display shows, so the blend rate is
//! at most the display's refresh rate (`super::display_refresh`): a game
//! drawing 300 pictures a second on a 120 Hz display gets 120 of them seen.
//!
//! A look takes the picture that was up at its time. When the screen changes
//! as fast as the blend rate, each look should get a different picture, but
//! with looks right next to the pictures' times, chance would decide which
//! picture a look gets, one twice and the next not at all. So the looks are
//! kept halfway between the pictures (locked to their pace). When the screen
//! changes faster, the looks pick evenly spaced ones of its pictures; slower,
//! some picture is seen twice (as it was up that long).
//!
//! The pictures are averaged as light, not as pixel values (which are
//! gamma-encoded): a screen showing them in turn gives off the average of
//! their light. Averaging the values would give a bright thing moving over a
//! dark background ghosts far too faint, and a dark thing over a bright one
//! ghosts too strong. The same as on Windows (`crate::win`).

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_metal::{
    MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue, MTLComputeCommandEncoder, MTLComputePipelineState, MTLCreateSystemDefaultDevice, MTLDevice,
    MTLPixelFormat, MTLSize, MTLStorageMode, MTLTexture, MTLTextureDescriptor, MTLTextureUsage,
};

use super::gpu::{FramePool, kernels, plane_texture};
use super::video::{Buffer, Frames, Latest};
use crate::mixer::Clock;
use crate::sck::{Encoder, host_now};

/// Adds looks at a picture to the frame's average, on the GPU, and turns the
/// average back into an NV12 frame to encode.
pub(crate) struct Blender {
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    add_pass: Retained<ProtocolObject<dyn MTLComputePipelineState>>,
    luma_pass: Retained<ProtocolObject<dyn MTLComputePipelineState>>,
    chroma_pass: Retained<ProtocolObject<dyn MTLComputePipelineState>>,
    /// The average so far, as light (16-bit float, so faint ghosts and dark
    /// colours keep their precision).
    light: Retained<ProtocolObject<dyn MTLTexture>>,
    /// Finished frames (the encoder holds each a while).
    pool: FramePool,
    /// The picture that's up.
    held: Option<Arc<Buffer>>,
    /// Looks in the average so far.
    count: u32,
    /// Looks at the picture that's up not added yet: added in one pass when
    /// another comes up or the frame is done (a screen slower than the blend
    /// rate is seen several times in a row).
    pending: u32,
}

// Metal devices, queues, pipelines and textures are thread-safe; the buffers
// are CoreVideo's (see `Buffer`).
unsafe impl Send for Blender {}

impl Blender {
    pub(crate) fn new(width: usize, height: usize) -> Result<Self> {
        let device = MTLCreateSystemDefaultDevice().context("no Metal device")?;
        let queue = device.newCommandQueue().context("no Metal command queue")?;
        let mut k = kernels(&device, KERNELS, &["add_look", "light_to_luma", "light_to_chroma"])?.into_iter();
        let light = unsafe {
            let desc = MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(MTLPixelFormat::RGBA16Float, width, height, false);
            desc.setUsage(MTLTextureUsage::ShaderRead | MTLTextureUsage::ShaderWrite);
            desc.setStorageMode(MTLStorageMode::Private);
            device.newTextureWithDescriptor(&desc).context("no GPU memory for frame blending")?
        };
        Ok(Self {
            add_pass: k.next().unwrap(),
            luma_pass: k.next().unwrap(),
            chroma_pass: k.next().unwrap(),
            light,
            pool: FramePool::new(width, height)?,
            held: None,
            count: 0,
            pending: 0,
            device,
            queue,
        })
    }

    /// `picture` is the one that's up now.
    pub(crate) fn hold(&mut self, picture: Arc<Buffer>) {
        self.flush();
        self.held = Some(picture);
    }

    /// Add a look at the picture that's up to the frame's average.
    pub(crate) fn add(&mut self) {
        if self.held.is_some() {
            self.pending += 1;
        }
    }

    /// Mix the looks at the picture that's up into the average, at once.
    /// Waits for the GPU, so the picture's buffer can go back to the stream.
    fn flush(&mut self) {
        let steps = std::mem::take(&mut self.pending);
        let Some(held) = self.held.as_ref().filter(|_| steps > 0) else { return };
        self.count += steps;
        // Mixing in by its share of the total so far keeps it the average.
        let weight = steps as f32 / self.count as f32;
        if let Err(e) = self.run(|enc, this| {
            let luma = plane_texture(&this.device, &held.0, 0, false)?;
            let chroma = plane_texture(&this.device, &held.0, 1, false)?;
            enc.setComputePipelineState(&this.add_pass);
            unsafe {
                enc.setTexture_atIndex(Some(&luma), 0);
                enc.setTexture_atIndex(Some(&chroma), 1);
                enc.setTexture_atIndex(Some(&this.light), 2);
                enc.setBytes_length_atIndex(NonNull::from(&weight).cast::<c_void>(), 4, 0);
            }
            dispatch(enc, luma.width(), luma.height());
            Ok(())
        }) {
            eprintln!("frame blending: {e:#}");
        }
    }

    /// The frame is done: its average as a frame to encode, and the next
    /// starts empty. None if nothing was added (no picture yet).
    pub(crate) fn finish(&mut self) -> Option<Arc<Buffer>> {
        self.flush();
        if std::mem::take(&mut self.count) == 0 {
            return None;
        }
        let made = self.pool.take().and_then(|out| {
            self.run(|enc, this| {
                for (plane, pass) in [(0, &this.luma_pass), (1, &this.chroma_pass)] {
                    let dst = plane_texture(&this.device, &out, plane, true)?;
                    enc.setComputePipelineState(pass);
                    unsafe {
                        enc.setTexture_atIndex(Some(&this.light), 0);
                        enc.setTexture_atIndex(Some(&dst), 1);
                    }
                    dispatch(enc, dst.width(), dst.height());
                }
                Ok(())
            })?;
            Ok(out)
        });
        match made {
            Ok(out) => Some(Arc::new(Buffer(out))),
            Err(e) => {
                eprintln!("frame blending: {e:#}");
                None
            }
        }
    }

    /// Encode one compute pass with `f`, run it and wait for it.
    fn run(&self, f: impl FnOnce(&ProtocolObject<dyn MTLComputeCommandEncoder>, &Self) -> Result<()>) -> Result<()> {
        let cmd = self.queue.commandBuffer().context("no command buffer")?;
        let enc = cmd.computeCommandEncoder().context("no compute encoder")?;
        let result = f(&enc, self);
        enc.endEncoding();
        cmd.commit();
        cmd.waitUntilCompleted();
        result
    }
}

fn dispatch(enc: &ProtocolObject<dyn MTLComputeCommandEncoder>, width: usize, height: usize) {
    enc.dispatchThreads_threadsPerThreadgroup(MTLSize { width, height, depth: 1 }, MTLSize { width: 16, height: 16, depth: 1 });
}

/// NV12 (studio range, BT.709) to light and back. A look is mixed into the
/// average by its weight (1: it starts it). Back from light, chroma is the
/// average of the four pixels it covers.
const KERNELS: &str = r#"
#include <metal_stdlib>
using namespace metal;

float3 to_light(float3 c) { c = saturate(c); return select(pow((c + 0.055) / 1.055, 2.4), c / 12.92, c <= 0.04045); }
float3 from_light(float3 c) { c = saturate(c); return select(1.055 * pow(c, 1.0 / 2.4) - 0.055, c * 12.92, c <= 0.0031308); }

float3 rgb_of(float y, float2 cbcr) {
    float l = (y * 255.0 - 16.0) / 219.0;
    float2 c = (cbcr * 255.0 - 128.0) / 224.0;
    return float3(l + 1.5748 * c.y, l - 0.1873 * c.x - 0.4681 * c.y, l + 1.8556 * c.x);
}

float3 ycc_of(float3 rgb) {
    float l = dot(rgb, float3(0.2126, 0.7152, 0.0722));
    return float3(16.0 + 219.0 * l, 128.0 + 224.0 * (rgb.b - l) / 1.8556, 128.0 + 224.0 * (rgb.r - l) / 1.5748) / 255.0;
}

kernel void add_look(texture2d<float, access::read> luma [[texture(0)]],
                     texture2d<float, access::read> chroma [[texture(1)]],
                     texture2d<float, access::read_write> light [[texture(2)]],
                     constant float& weight [[buffer(0)]],
                     uint2 gid [[thread_position_in_grid]]) {
    if (gid.x >= light.get_width() || gid.y >= light.get_height()) return;
    float3 c = to_light(rgb_of(luma.read(gid).x, chroma.read(gid / 2).xy));
    float3 a = weight >= 1.0 ? c : mix(light.read(gid).rgb, c, weight);
    light.write(float4(a, 1.0), gid);
}

kernel void light_to_luma(texture2d<float, access::read> light [[texture(0)]],
                          texture2d<float, access::write> out [[texture(1)]],
                          uint2 gid [[thread_position_in_grid]]) {
    if (gid.x >= out.get_width() || gid.y >= out.get_height()) return;
    out.write(float4(ycc_of(from_light(light.read(gid).rgb)).x, 0.0, 0.0, 1.0), gid);
}

kernel void light_to_chroma(texture2d<float, access::read> light [[texture(0)]],
                            texture2d<float, access::write> out [[texture(1)]],
                            uint2 gid [[thread_position_in_grid]]) {
    if (gid.x >= out.get_width() || gid.y >= out.get_height()) return;
    uint2 last = uint2(light.get_width(), light.get_height()) - 1;
    float3 sum = 0.0;
    for (uint y = 0; y < 2; y++) {
        for (uint x = 0; x < 2; x++) {
            sum += from_light(light.read(min(gid * 2 + uint2(x, y), last)).rgb);
        }
    }
    out.write(float4(ycc_of(sum / 4.0).yz, 0.0, 1.0), gid);
}
"#;

// ---------------------------------------------------------------------------
// Pacer
// ---------------------------------------------------------------------------

/// How long after a frame's time it's made: a picture reaches us a fraction
/// of a millisecond after the screen showed it (its time), and it must still
/// get into its frame. A guess, with room for a busy system.
const BLEND_LATE: f64 = 0.006;
/// How much of the way the looks move toward halfway between the screen's
/// pictures, on each picture. A guess: slow enough that the noise in
/// pictures' times averages out (a few hundred milliseconds), quick enough to
/// follow a screen that drifts. The same as on Windows.
const PHASE_GAIN: f64 = 0.03;

struct BlendPacer {
    t0: f64,
    fps: u32,
    /// Seconds between looks (one over the blend rate), and looks per frame.
    step: f64,
    looks: i64,
    /// Look `m` is at `t0 + phase + (m + 0.5) * step`; frame `k` is looks
    /// `k * looks` to `(k + 1) * looks - 1`.
    phase: f64,
    /// The next look to take.
    next: i64,
    /// The picture that's up (its number), and the one the last look took.
    held: Option<u64>,
    last_taken: Option<u64>,
    /// Different pictures in the frame being made.
    different: u32,
    blender: Blender,
    frames: Frames,
    encoder: SendEncoder,
    skipped: u64,
    stats: BlendStats,
}

struct SendEncoder(Encoder);
unsafe impl Send for SendEncoder {}

impl BlendPacer {
    fn look_time(&self, m: i64) -> f64 {
        self.t0 + self.phase + (m as f64 + 0.5) * self.step
    }

    /// When the frame being made is done: its last look.
    fn frame_end(&self) -> f64 {
        let last = (self.next.div_euclid(self.looks) + 1) * self.looks - 1;
        self.look_time(last)
    }

    /// Take every look before `t`: each sees the picture that's up.
    fn look_until(&mut self, t: f64) {
        let mut burst = 0;
        while self.look_time(self.next) < t && burst < 10_000 {
            self.look();
            burst += 1;
        }
    }

    fn look(&mut self) {
        if let Some(id) = self.held {
            self.blender.add();
            if self.last_taken != Some(id) {
                self.different += 1;
                self.last_taken = Some(id);
            }
        }
        self.next += 1;
        if self.next % self.looks == 0 {
            self.close(self.next / self.looks - 1);
        }
    }

    /// Frame `k` is done: encode it (with the webcam over it).
    fn close(&mut self, k: i64) {
        let different = std::mem::take(&mut self.different);
        let Some(average) = self.blender.finish() else { return };
        self.stats.frame(different, self.looks as u32);
        let frame = self.frames.make(average);
        if !self.encoder.0.encode(&frame.0, k, self.fps) {
            self.skipped += 1;
            if self.skipped.is_power_of_two() {
                eprintln!("video encoder is behind: {} frames skipped so far", self.skipped);
            }
        }
    }

    /// A new picture came up at `at`: the looks before it saw the one before.
    /// Then the looks move toward halfway between pictures.
    fn arrive(&mut self, at: f64, picture: Arc<Buffer>, id: u64) {
        if at < self.look_time(self.next - 1) {
            self.stats.late += 1;
        }
        self.look_until(at);
        // Pictures belong on whole steps of the looks (halfway between two).
        let steps = (at - self.t0 - self.phase) / self.step;
        self.phase += (steps - steps.round()) * self.step * PHASE_GAIN;
        // A whole step either way is the same lock, one look over.
        if self.phase > self.step / 2.0 {
            self.phase -= self.step;
        } else if self.phase < -self.step / 2.0 {
            self.phase += self.step;
        }
        self.blender.hold(picture);
        self.held = Some(id);
        self.stats.pictures += 1;
    }
}

/// What the blending saw, printed every few seconds (to the log): how many
/// pictures the screen showed, how many different ones each frame got (all
/// its looks, when the screen changes at least at the blend rate), and how
/// many came too late for their frame.
struct BlendStats {
    since: Instant,
    pictures: u32,
    frames: u32,
    full: u32,
    fewest: u32,
    most: u32,
    late: u32,
}

impl BlendStats {
    fn new() -> Self {
        Self { since: Instant::now(), pictures: 0, frames: 0, full: 0, fewest: u32::MAX, most: 0, late: 0 }
    }

    fn frame(&mut self, different: u32, looks: u32) {
        self.frames += 1;
        self.full += (different == looks) as u32;
        self.fewest = self.fewest.min(different);
        self.most = self.most.max(different);
        if self.since.elapsed() >= Duration::from_secs(5) {
            eprintln!(
                "frame blending: the screen showed {:.0} pictures/s; {looks} looks per frame, {}-{} different pictures in each, {:.0}% of frames with {looks}; {} pictures too late, over {} frames",
                self.pictures as f64 / self.since.elapsed().as_secs_f64(),
                self.fewest,
                self.most,
                self.full as f64 * 100.0 / self.frames as f64,
                self.late,
                self.frames
            );
            *self = Self::new();
        }
    }
}

/// Like `sck::spawn_pacer`, for frame blending: `looks` looks at the screen
/// per frame, averaged.
pub(crate) fn spawn_blend_pacer(
    fps: u32,
    looks: u32,
    latest: Arc<Latest>,
    blender: Blender,
    frames: Frames,
    encoder: Encoder,
    clock: Arc<Clock>,
) -> (Arc<AtomicBool>, JoinHandle<()>) {
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    latest.queue_arrivals();
    let encoder = SendEncoder(encoder);
    let handle = thread::spawn(move || {
        let t0 = host_now();
        clock.set(t0);
        let mut p = BlendPacer {
            t0,
            fps,
            step: 1.0 / (fps.max(1) * looks.max(1)) as f64,
            looks: looks.max(1) as i64,
            phase: 0.0,
            next: 0,
            held: None,
            last_taken: None,
            different: 0,
            blender,
            frames,
            encoder,
            skipped: 0,
            stats: BlendStats::new(),
        };
        let mut seen = latest.seq();
        if let Some(b) = latest.get() {
            p.blender.hold(b);
            p.held = Some(seen);
        }
        while !stop2.load(Ordering::Relaxed) {
            let wait = p.frame_end() + BLEND_LATE - host_now();
            if wait > 0.0 {
                latest.wait(seen, Duration::from_secs_f64(wait.min(0.05)));
            }
            for a in latest.drain() {
                if a.seq > seen {
                    seen = a.seq;
                    p.arrive(a.at, a.buffer, a.seq);
                }
            }
            // Put up some other way (the away screen): take it from the capture.
            let now = latest.seq();
            if now > seen {
                seen = now;
                if let Some(b) = latest.get() {
                    p.arrive(host_now(), b, now);
                }
            }
            // Looks whose time is past (and late pictures' too) see what's up.
            p.look_until(host_now() - BLEND_LATE);
        }
        p.encoder.0.finish();
    });
    (stop, handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_core_video::*;

    fn filled(w: usize, h: usize, y: u8, cb: u8, cr: u8) -> Arc<Buffer> {
        let b = FramePool::new(w, h).unwrap().take().unwrap();
        unsafe {
            CVPixelBufferLockBaseAddress(&b, CVPixelBufferLockFlags(0));
            for (plane, vals) in [(0usize, vec![y]), (1, vec![cb, cr])] {
                let base = CVPixelBufferGetBaseAddressOfPlane(&b, plane) as *mut u8;
                let stride = CVPixelBufferGetBytesPerRowOfPlane(&b, plane);
                let cols = CVPixelBufferGetWidthOfPlane(&b, plane) * vals.len();
                for r in 0..CVPixelBufferGetHeightOfPlane(&b, plane) {
                    for x in 0..cols {
                        *base.add(r * stride + x) = vals[x % vals.len()];
                    }
                }
            }
            CVPixelBufferUnlockBaseAddress(&b, CVPixelBufferLockFlags(0));
        }
        Arc::new(Buffer(b))
    }

    fn sample(b: &CVPixelBuffer) -> (u8, u8, u8) {
        unsafe {
            CVPixelBufferLockBaseAddress(b, CVPixelBufferLockFlags::ReadOnly);
            let y = *(CVPixelBufferGetBaseAddressOfPlane(b, 0) as *const u8);
            let c = CVPixelBufferGetBaseAddressOfPlane(b, 1) as *const u8;
            let out = (y, *c, *c.add(1));
            CVPixelBufferUnlockBaseAddress(b, CVPixelBufferLockFlags::ReadOnly);
            out
        }
    }

    /// One picture blended with itself comes out as it went in.
    #[test]
    fn one_picture_is_unchanged() {
        let mut b = Blender::new(64, 32).unwrap();
        b.hold(filled(64, 32, 120, 100, 150));
        b.add();
        b.add();
        let out = b.finish().unwrap();
        let (y, cb, cr) = sample(&out.0);
        assert!(y.abs_diff(120) <= 1 && cb.abs_diff(100) <= 1 && cr.abs_diff(150) <= 1, "{y} {cb} {cr}");
        assert!(b.finish().is_none(), "the next frame starts empty");
    }

    /// White seen one look in four over black: a quarter of white's light,
    /// which is far brighter than a quarter of its value.
    #[test]
    fn averages_light() {
        let mut b = Blender::new(64, 32).unwrap();
        b.hold(filled(64, 32, 235, 128, 128));
        b.add();
        b.hold(filled(64, 32, 16, 128, 128));
        b.add();
        b.add();
        b.add();
        let (y, cb, cr) = sample(&b.finish().unwrap().0);
        // sRGB of 0.25 light is 0.537: 16 + 219 * 0.537 = 134.
        assert!(y.abs_diff(134) <= 1, "{y}");
        assert_eq!((cb, cr), (128, 128));
    }
}
