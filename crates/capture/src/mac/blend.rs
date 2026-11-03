//! Live frame blending on macOS: each frame is the average of a fixed number
//! of looks at the screen, evenly spaced over the frame's time (the blend
//! rate over the frame rate: 120 fps blending at 40 fps is 3), each weighed
//! the same, so every frame shows the same number of steps of motion. Or,
//! blended softly, of the looks from the middle of the frame before to the
//! middle of the one after, the nearer the frame's middle the more (see
//! [`crate::BlendShape`]): two frames are made at once.
//!
//! ScreenCaptureKit only sees what the display shows, so the blend rate is
//! at most the display's refresh rate (`super::display_refresh`): a game
//! drawing 300 pictures a second on a 120 Hz display gets 120 of them seen.
//!
//! A look takes the picture that was up at its time. When the screen changes
//! as fast as the blend rate, each look should get a different picture, but
//! with looks right next to the pictures' times, chance would decide which
//! picture a look gets, one twice and the next not at all. So the looks are
//! kept halfway between the pictures, locked to their pace as on Windows and
//! Linux (`crate::shutter::follow_picture`). When the screen changes faster,
//! the looks pick evenly spaced ones of its pictures; slower, some picture
//! is seen twice (as it was up that long).
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
use block2::RcBlock;
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
    /// The averages being made, as light (16-bit float, so faint ghosts and
    /// dark colours keep their precision): one per frame open at once, by
    /// its number (even and odd, blended softly).
    light: Vec<Retained<ProtocolObject<dyn MTLTexture>>>,
    /// Finished frames (the encoder holds each a while).
    pool: FramePool,
    /// The picture that's up.
    held: Option<Arc<Buffer>>,
    /// Per average: the looks' weight in it so far.
    weight: [f32; 2],
    /// Per average: the weight of looks at the picture that's up not added
    /// yet: added in one pass when another comes up or the frame is done (a
    /// screen slower than the blend rate is seen several times in a row).
    pending: [f32; 2],
    /// Waits for the GPU at each step (live: a picture goes back to the
    /// stream at once), else (exports) never: see [`Self::queued`].
    waits: bool,
    /// What the GPU was last given, not waiting for it.
    last: Option<Retained<ProtocolObject<dyn MTLCommandBuffer>>>,
}

// Metal devices, queues, pipelines and textures are thread-safe; the buffers
// are CoreVideo's (see `Buffer`).
unsafe impl Send for Blender {}

impl Blender {
    /// `open` frames made at once.
    pub(crate) fn new(width: usize, height: usize, open: usize) -> Result<Self> {
        let device = MTLCreateSystemDefaultDevice().context("no Metal device")?;
        let queue = device.newCommandQueue().context("no Metal command queue")?;
        Self::on(device, queue, width, height, open, true)
    }

    /// For exports: on `queue`, where the pictures it's given are made (so
    /// they're made before it reads them), never waiting for the GPU: each
    /// picture is kept until the GPU is done with it, and a frame finished
    /// is made once [`Self::last_work`] is done.
    pub(crate) fn queued(
        device: Retained<ProtocolObject<dyn MTLDevice>>,
        queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
        width: usize,
        height: usize,
        open: usize,
    ) -> Result<Self> {
        Self::on(device, queue, width, height, open, false)
    }

    fn on(
        device: Retained<ProtocolObject<dyn MTLDevice>>,
        queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
        width: usize,
        height: usize,
        open: usize,
        waits: bool,
    ) -> Result<Self> {
        let mut k = kernels(&device, KERNELS, &["add_look", "light_to_luma", "light_to_chroma"])?.into_iter();
        let light = (0..open.clamp(1, 2))
            .map(|_| unsafe {
                let desc = MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(MTLPixelFormat::RGBA16Float, width, height, false);
                desc.setUsage(MTLTextureUsage::ShaderRead | MTLTextureUsage::ShaderWrite);
                desc.setStorageMode(MTLStorageMode::Private);
                device.newTextureWithDescriptor(&desc).context("no GPU memory for frame blending")
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            add_pass: k.next().unwrap(),
            luma_pass: k.next().unwrap(),
            chroma_pass: k.next().unwrap(),
            light,
            pool: FramePool::new(width, height)?,
            held: None,
            weight: [0.0; 2],
            pending: [0.0; 2],
            waits,
            last: None,
            device,
            queue,
        })
    }

    /// The GPU work given last (not waiting for it): once it's done, so is
    /// the frame last finished.
    pub(crate) fn last_work(&self) -> Option<Retained<ProtocolObject<dyn MTLCommandBuffer>>> {
        self.last.clone()
    }

    /// `picture` is the one that's up now.
    pub(crate) fn hold(&mut self, picture: Arc<Buffer>) {
        self.flush();
        self.held = Some(picture);
    }

    /// Which average frame `k` is made in.
    fn slot(&self, k: i64) -> usize {
        k.rem_euclid(self.light.len() as i64) as usize
    }

    /// Add a look at the picture that's up to frame `k`'s average, by `weight`.
    pub(crate) fn add(&mut self, k: i64, weight: f32) {
        if self.held.is_some() {
            let i = self.slot(k);
            self.pending[i] += weight;
        }
    }

    /// Mix the looks at the picture that's up into the averages, at once.
    /// Waits for the GPU, so the picture's buffer can go back to the stream
    /// (exports keep it instead: [`Self::queued`]).
    fn flush(&mut self) {
        let pending = std::mem::take(&mut self.pending);
        let Some(held) = self.held.clone().filter(|_| pending.iter().any(|w| *w > 0.0)) else { return };
        // Mixing in by its share of the total so far keeps it the average.
        let mut mixes = Vec::new();
        for (i, weight) in pending.into_iter().enumerate().filter(|(_, w)| *w > 0.0) {
            self.weight[i] += weight;
            mixes.push((i, weight / self.weight[i]));
        }
        if let Err(e) = self.run(Some(held.clone()), |enc, this| {
            let luma = plane_texture(&this.device, &held.0, 0, false)?;
            let chroma = plane_texture(&this.device, &held.0, 1, false)?;
            enc.setComputePipelineState(&this.add_pass);
            for (i, share) in &mixes {
                unsafe {
                    enc.setTexture_atIndex(Some(&luma), 0);
                    enc.setTexture_atIndex(Some(&chroma), 1);
                    enc.setTexture_atIndex(Some(&this.light[*i]), 2);
                    enc.setBytes_length_atIndex(NonNull::from(share).cast::<c_void>(), 4, 0);
                }
                dispatch(enc, luma.width(), luma.height());
            }
            Ok(())
        }) {
            crate::eprintln_later!("frame blending: {e:#}");
        }
    }

    /// Frame `k` is done: its average as a frame to encode, and the average
    /// starts empty for the frame after next. None if nothing was added (no
    /// picture yet).
    pub(crate) fn finish(&mut self, k: i64) -> Option<Arc<Buffer>> {
        self.flush();
        let i = self.slot(k);
        if std::mem::take(&mut self.weight[i]) == 0.0 {
            return None;
        }
        let made = self.pool.take().and_then(|out| {
            self.run(None, |enc, this| {
                for (plane, pass) in [(0, &this.luma_pass), (1, &this.chroma_pass)] {
                    let dst = plane_texture(&this.device, &out, plane, true)?;
                    enc.setComputePipelineState(pass);
                    unsafe {
                        enc.setTexture_atIndex(Some(&this.light[i]), 0);
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
                crate::eprintln_later!("frame blending: {e:#}");
                None
            }
        }
    }

    /// Encode one compute pass with `f`, run it and wait for it; or, not
    /// waiting (exports), keep `reads` until the GPU is done with it.
    fn run(&mut self, reads: Option<Arc<Buffer>>, f: impl FnOnce(&ProtocolObject<dyn MTLComputeCommandEncoder>, &Self) -> Result<()>) -> Result<()> {
        let cmd = self.queue.commandBuffer().context("no command buffer")?;
        let enc = cmd.computeCommandEncoder().context("no compute encoder")?;
        let result = f(&enc, self);
        enc.endEncoding();
        if self.waits {
            cmd.commit();
            cmd.waitUntilCompleted();
        } else {
            if let Some(reads) = reads {
                // Kept until the GPU, done, lets go of the block.
                let keep = RcBlock::new(move |_: NonNull<ProtocolObject<dyn MTLCommandBuffer>>| {
                    let _ = &reads;
                });
                unsafe { cmd.addCompletedHandler(RcBlock::as_ptr(&keep)) };
            }
            cmd.commit();
            self.last = Some(cmd);
        }
        result
    }
}

/// What a frame is made of: its looks blended on the graphics card, or (one
/// look a frame) the picture its look took, as it is.
pub(crate) enum Mix {
    Blend(Blender),
    /// Not blended: each frame is the picture that was up at its time,
    /// taken by its one look, with no graphics card work.
    One {
        held: Option<Arc<Buffer>>,
        /// The frame a look was for, and the picture it took.
        took: Option<(i64, Arc<Buffer>)>,
    },
}

impl Mix {
    /// One look a frame.
    pub(crate) fn one() -> Self {
        Self::One { held: None, took: None }
    }

    /// `picture` is the one that's up now.
    fn hold(&mut self, picture: Arc<Buffer>) {
        match self {
            Self::Blend(b) => b.hold(picture),
            Self::One { held, .. } => *held = Some(picture),
        }
    }

    /// A look at the picture that's up, for frame `k`, by `weight`.
    fn add(&mut self, k: i64, weight: f32) {
        match self {
            Self::Blend(b) => b.add(k, weight),
            Self::One { held, took } => {
                if let Some(picture) = held {
                    *took = Some((k, picture.clone()));
                }
            }
        }
    }

    /// Frame `k` is done: what it's made of. None if nothing was added to
    /// it (no picture yet).
    fn finish(&mut self, k: i64) -> Option<Arc<Buffer>> {
        match self {
            Self::Blend(b) => b.finish(k),
            Self::One { took, .. } => took.take().filter(|(frame, _)| *frame == k).map(|(_, picture)| picture),
        }
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

struct BlendPacer {
    t0: f64,
    fps: u32,
    /// Seconds between looks (one over the blend rate), and looks per frame.
    step: f64,
    looks: i64,
    /// Look `m` is at `t0 + phase + (m + 0.5) * step`; frame `k`'s own time
    /// is looks `k * looks` to `(k + 1) * looks - 1`.
    phase: f64,
    /// A picture the stream timed has come up (it set `phase`).
    timed: bool,
    /// Which frames each look counts toward.
    shutter: crate::shutter::Shutter,
    /// The next look to take.
    next: i64,
    /// The last frame done (made or empty): later looks don't count toward it.
    closed: i64,
    /// The picture that's up (its number), and the one the last look took.
    held: Option<u64>,
    last_taken: Option<u64>,
    /// Different pictures in the frame's own time so far.
    different: u32,
    mix: Mix,
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

    /// When the next frame to make is done: its last look.
    fn frame_end(&self) -> f64 {
        self.look_time(self.shutter.last_look(self.closed + 1))
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
        let (first, weights) = self.shutter.split(self.next);
        if let Some(id) = self.held {
            for (k, weight) in [(first, weights[0]), (first + 1, weights[1])] {
                if k > self.closed && weight > 0.0 {
                    self.mix.add(k, weight);
                }
            }
            if self.last_taken != Some(id) {
                self.different += 1;
                self.last_taken = Some(id);
            }
        }
        self.next += 1;
        if self.next % self.looks == 0 {
            let different = std::mem::take(&mut self.different);
            if self.held.is_some() {
                self.stats.frame(different, self.looks as u32);
            }
        }
        // The frames no later look counts toward are done.
        let (first, _) = self.shutter.split(self.next);
        while self.closed < first - 1 {
            self.close(self.closed + 1);
        }
    }

    /// Frame `k` is done: encode it (with the webcam over it).
    fn close(&mut self, k: i64) {
        self.closed = k;
        let Some(made) = self.mix.finish(k) else { return };
        let frame = self.frames.make(made);
        if self.encoder.0.encode(&frame.0, k, self.fps) {
            crate::count_encoded();
        } else {
            crate::count_skipped(1);
            self.skipped += 1;
            if self.skipped.is_power_of_two() {
                crate::eprintln_later!("video encoder is behind: {} frames skipped so far", self.skipped);
            }
        }
    }

    /// A new picture came up at `at`: the looks before it saw the one before.
    /// Then the looks move toward halfway between pictures. `timed`: `at` is
    /// when the stream says the screen showed it (else when it was noticed,
    /// as the away screen is).
    fn arrive(&mut self, at: f64, picture: Arc<Buffer>, id: u64, timed: bool) {
        crate::count_picture();
        if at < self.look_time(self.next - 1) {
            self.stats.late += 1;
            crate::count_missed(1);
        }
        self.look_until(at);
        // Pictures belong on whole steps of the looks (halfway between two);
        // the first the stream timed puts them there.
        let first = timed && !self.timed;
        self.timed |= timed;
        self.phase = crate::shutter::follow_picture(self.phase, self.step, at - self.t0, first);
        self.mix.hold(picture);
        self.held = Some(id);
        self.stats.pictures += 1;
    }
}

/// What the blending saw, printed every few seconds (to the log): how many
/// pictures the screen showed, how many different ones each frame got (all
/// its looks, when the screen changes at least at the blend rate), and how
/// many came too late for their frame. With one look a frame (not blended),
/// a frame with none is a picture repeated. Written by another thread, as
/// on Windows: the pacer has a few milliseconds between pictures.
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
            crate::eprintln_later!(
                "{}: the screen showed {:.0} pictures/s; {looks} looks per frame, {}-{} different pictures in each, {:.0}% of frames with {looks}; {} pictures too late, over {} frames",
                if looks == 1 { "frame timing" } else { "frame blending" },
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

/// The pacer: `looks` looks at the screen per frame, each taking the picture
/// that was up at its time (by when the screen showed it), kept halfway
/// between the screen's pictures; the frame is made of them as `mix` says
/// (blended, or one look's picture as it is). Frame `k` is stamped `k / fps`
/// on the host clock, the audio's: a frame is made every 1/fps whether the
/// screen changed or not (constant frame rate), and when the encoder can't
/// take one it's skipped, not queued, so video never falls behind.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_blend_pacer(
    fps: u32,
    looks: u32,
    shutter: crate::shutter::Shutter,
    latest: Arc<Latest>,
    mix: Mix,
    frames: Frames,
    encoder: Encoder,
    clock: Arc<Clock>,
) -> (Arc<AtomicBool>, JoinHandle<()>) {
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    latest.queue_arrivals();
    let encoder = SendEncoder(encoder);
    let handle = thread::spawn(move || {
        crate::blending(fps, looks);
        let t0 = host_now();
        clock.set(t0);
        let mut p = BlendPacer {
            t0,
            fps,
            step: 1.0 / (fps.max(1) * looks.max(1)) as f64,
            looks: looks.max(1) as i64,
            phase: 0.0,
            timed: false,
            shutter,
            next: 0,
            closed: -1,
            held: None,
            last_taken: None,
            different: 0,
            mix,
            frames,
            encoder,
            skipped: 0,
            stats: BlendStats::new(),
        };
        let mut seen = latest.seq();
        if let Some(b) = latest.get() {
            p.mix.hold(b);
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
                    p.arrive(a.at, a.buffer, a.seq, true);
                }
            }
            // Put up some other way (the away screen): take it from the capture.
            let now = latest.seq();
            if now > seen {
                seen = now;
                if let Some(b) = latest.get() {
                    p.arrive(host_now(), b, now, false);
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
        let mut b = Blender::new(64, 32, 1).unwrap();
        b.hold(filled(64, 32, 120, 100, 150));
        b.add(0, 1.0);
        b.add(0, 1.0);
        let out = b.finish(0).unwrap();
        let (y, cb, cr) = sample(&out.0);
        assert!(y.abs_diff(120) <= 1 && cb.abs_diff(100) <= 1 && cr.abs_diff(150) <= 1, "{y} {cb} {cr}");
        assert!(b.finish(1).is_none(), "the next frame starts empty");
    }

    /// White seen one look in four over black: a quarter of white's light,
    /// which is far brighter than a quarter of its value.
    #[test]
    fn averages_light() {
        let mut b = Blender::new(64, 32, 1).unwrap();
        b.hold(filled(64, 32, 235, 128, 128));
        b.add(0, 1.0);
        b.hold(filled(64, 32, 16, 128, 128));
        b.add(0, 1.0);
        b.add(0, 1.0);
        b.add(0, 1.0);
        let (y, cb, cr) = sample(&b.finish(0).unwrap().0);
        // sRGB of 0.25 light is 0.537: 16 + 219 * 0.537 = 134.
        assert!(y.abs_diff(134) <= 1, "{y}");
        assert_eq!((cb, cr), (128, 128));
    }

    /// Two frames made at once (blended softly) each get their own looks,
    /// by their own weights.
    #[test]
    fn two_frames_at_once() {
        let mut b = Blender::new(64, 32, 2).unwrap();
        b.hold(filled(64, 32, 235, 128, 128));
        b.add(0, 0.25);
        b.add(1, 0.75);
        b.hold(filled(64, 32, 16, 128, 128));
        b.add(0, 0.75);
        b.add(1, 0.25);
        let (y0, _, _) = sample(&b.finish(0).unwrap().0);
        let (y1, _, _) = sample(&b.finish(1).unwrap().0);
        // sRGB of 0.75 light is 0.881: 16 + 219 * 0.881 = 209.
        assert!(y0.abs_diff(134) <= 1, "{y0}");
        assert!(y1.abs_diff(209) <= 1, "{y1}");
    }
}
