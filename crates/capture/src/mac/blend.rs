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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_core_foundation::CFRetained;
use objc2_metal::{
    MTLCommandBuffer, MTLCommandBufferStatus, MTLCommandEncoder, MTLCommandQueue, MTLComputeCommandEncoder, MTLComputePipelineState, MTLCreateSystemDefaultDevice, MTLDevice,
    MTLPixelFormat, MTLSize, MTLStorageMode, MTLTexture, MTLTextureDescriptor, MTLTextureUsage,
};

use super::export::Scaler;
use super::gpu::{FramePool, kernels, plane_texture};
use super::video::{Buffer, Frames, Latest};
use crate::mixer::Clock;
use crate::sck::{Encoder, host_now};
use crate::shutter::Shutter;

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
    /// When it waits for the GPU.
    waits: Waits,
    /// Its GPU work given and not done yet, oldest first (live).
    behind: std::collections::VecDeque<Retained<ProtocolObject<dyn MTLCommandBuffer>>>,
    /// What the GPU was last given, not waiting for it.
    last: Option<Retained<ProtocolObject<dyn MTLCommandBuffer>>>,
}

/// When a blender waits for the graphics card.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Waits {
    /// Never (exports): each picture is kept until the GPU is done with it,
    /// and a frame finished is made once [`Blender::last_work`] is done.
    Never,
    /// As exports do, but once this much of its work is waiting for the GPU,
    /// for the oldest: each piece holds a picture of the stream's, which has
    /// 8 for everything that holds them (live).
    Behind(usize),
}

/// The most of a live blender's work that may wait for the GPU before it
/// waits for some of it. Waiting at each step instead (a picture added, a
/// frame made), the pacer was held up as long as the GPU was busy with a
/// game: kept busy in 30 ms pieces, up to 23 ms a picture and 39 ms a frame,
/// so pictures were late for their looks (33 in 5 s at 60 fps, 2 looks) and
/// 40% of frames got both their pictures.
const MOST_BEHIND: usize = 4;

/// Whether the GPU is done with `work` (or gave up on it).
pub(crate) fn gpu_done(work: &ProtocolObject<dyn MTLCommandBuffer>) -> bool {
    matches!(work.status(), MTLCommandBufferStatus::Completed | MTLCommandBufferStatus::Error)
}

// Metal devices, queues, pipelines and textures are thread-safe; the buffers
// are CoreVideo's (see `Buffer`).
unsafe impl Send for Blender {}

impl Blender {
    /// Live: `open` frames made at once. A frame finished is made once
    /// [`Self::last_work`] is done; the GPU is waited for only when it's
    /// [`MOST_BEHIND`] behind.
    pub(crate) fn new(width: usize, height: usize, open: usize) -> Result<Self> {
        let device = MTLCreateSystemDefaultDevice().context("no Metal device")?;
        let queue = device.newCommandQueue().context("no Metal command queue")?;
        Self::on(device, queue, width, height, open, Waits::Behind(MOST_BEHIND))
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
        Self::on(device, queue, width, height, open, Waits::Never)
    }

    fn on(
        device: Retained<ProtocolObject<dyn MTLDevice>>,
        queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
        width: usize,
        height: usize,
        open: usize,
        waits: Waits,
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
            behind: Default::default(),
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

    /// Mix the looks at the picture that's up into the averages, at once:
    /// the picture is kept until the GPU is done with it.
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

    /// Encode one compute pass with `f` and give it to the GPU, keeping
    /// `reads` until the GPU is done with it; live, once [`MOST_BEHIND`] of
    /// its work is waiting for the GPU, wait for the oldest.
    fn run(&mut self, reads: Option<Arc<Buffer>>, f: impl FnOnce(&ProtocolObject<dyn MTLComputeCommandEncoder>, &Self) -> Result<()>) -> Result<()> {
        let cmd = self.queue.commandBuffer().context("no command buffer")?;
        let enc = cmd.computeCommandEncoder().context("no compute encoder")?;
        let result = f(&enc, self);
        enc.endEncoding();
        if let Some(reads) = reads {
            // Kept until the GPU, done, lets go of the block.
            let keep = RcBlock::new(move |_: NonNull<ProtocolObject<dyn MTLCommandBuffer>>| {
                let _ = &reads;
            });
            unsafe { cmd.addCompletedHandler(RcBlock::as_ptr(&keep)) };
        }
        cmd.commit();
        if let Waits::Behind(most) = self.waits {
            // One queue: the GPU does its work in order.
            while self.behind.front().is_some_and(|w| gpu_done(w)) {
                self.behind.pop_front();
            }
            self.behind.push_back(cmd.clone());
            while self.behind.len() > most {
                if let Some(oldest) = self.behind.pop_front() {
                    oldest.waitUntilCompleted();
                }
            }
        }
        self.last = Some(cmd);
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

    /// The GPU work that makes the frame last finished, if it has some: the
    /// frame is made once that's done.
    fn work(&self) -> Option<Retained<ProtocolObject<dyn MTLCommandBuffer>>> {
        match self {
            Self::Blend(b) => b.last_work(),
            Self::One { .. } => None,
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
    shutter: Shutter,
    /// The next look to take.
    next: i64,
    /// The last frame done (made or empty): later looks don't count toward it.
    closed: i64,
    /// The picture that's up (its number), and the one the last look took.
    held: Option<u64>,
    last_taken: Option<u64>,
    /// The picture that's up, for a mix that takes over (blending changed).
    up: Option<Arc<Buffer>>,
    /// Different pictures in the frame's own time so far.
    different: u32,
    mix: Mix,
    frames: Frames,
    encoder: SendEncoder,
    /// The stream's own pictures, while it streams (one per rendition).
    stream: Vec<StreamOut>,
    skipped: u64,
    stats: BlendStats,
    report: EncodeReport,
    /// Blended frames the GPU is still making, oldest first.
    making: std::collections::VecDeque<Making>,
}

/// What the session changes in a running pacer: frame blending, and the
/// stream's pictures (see `Session::set_blend` and `Session::stream_video`
/// in `crate::sck`). Each is taken up at the pacer's next turn.
#[derive(Clone, Default)]
pub(crate) struct PacerCtl {
    pub(crate) blend: Arc<Mutex<Option<BlendChange>>>,
    /// The stream started (one per rendition, `stream::renditions`), or
    /// stopped (none).
    pub(crate) stream: Arc<Mutex<Option<Vec<StreamOut>>>>,
}

/// Frame blending changed: `looks` a frame from the next frame on, where
/// `shutter` says, made by `mix` (made beforehand: a blender's kernels take
/// a moment to build, which the pacer mustn't wait for).
pub(crate) struct BlendChange {
    pub(crate) looks: u32,
    pub(crate) shutter: Shutter,
    pub(crate) mix: Mix,
}

/// One of the stream's pictures (a rendition, `stream::renditions`): a
/// frame for every `every` the pacer makes, made the rendition's size by
/// Metal (`super::export::Scaler`, as an export's) and encoded by a
/// VideoToolbox encoder of its own, all on a thread of its own so the pacer
/// never waits for any of it. A frame that comes while the thread is still
/// busy with those before is left out.
pub(crate) struct StreamOut {
    every: i64,
    /// The pacer's frames, their numbers, and whether it blends.
    tx: Option<SyncSender<(Arc<Buffer>, i64, bool)>>,
    thread: Option<JoinHandle<()>>,
    /// Frames left out, its thread busy.
    dropped: u64,
}

/// A buffer to hand to the GPU, from one that's shared.
fn retain(buf: &objc2_core_video::CVPixelBuffer) -> CFRetained<objc2_core_video::CVPixelBuffer> {
    unsafe { CFRetained::retain(NonNull::from(buf)) }
}

impl StreamOut {
    /// A frame for every `every` of the pacer's, `from` big, made `to` big
    /// and encoded by `encoder` (at the rendition's rate). With frame
    /// blending on, and `every` over one (48 fps from 144), each frame is
    /// the average of all the pacer's in its time, as light, as a frame is
    /// of its looks, so its motion is blurred over all of its time as the
    /// recording's is over each frame's (as Windows' `StreamBlend`); taking
    /// one in three instead blurred a third of it. The average's made on
    /// the scaler's queue, so it's made before it's scaled.
    pub(crate) fn start(every: i64, from: (usize, usize), to: (usize, usize), fps: u32, encoder: Encoder) -> Result<Self> {
        let device = MTLCreateSystemDefaultDevice().context("no Metal device")?;
        let queue = device.newCommandQueue().context("no Metal command queue")?;
        let size = |(w, h): (usize, usize)| (w as u32, h as u32);
        let scaler = Scaler::new(&device, &queue, size(from), [0, 0, from.0 as u32, from.1 as u32], size(to), false)?;
        let blender = if every > 1 { Some(Blender::queued(device.clone(), queue.clone(), from.0, from.1, 1)?) } else { None };
        // Room for a stream frame's worth of the pacer's frames, and more.
        let (tx, rx) = mpsc::sync_channel::<(Arc<Buffer>, i64, bool)>((2 * every as usize).clamp(2, 16));
        let encoder = SendEncoder(encoder);
        let thread = thread::Builder::new().name("stream video".into()).spawn(move || {
            // All of it moves here (not just the encoder inside).
            let encoder = encoder;
            let mut blender = blender;
            // The stream frame whose average is being made.
            let mut summing: Option<i64> = None;
            let mut skipped = 0u64;
            for (frame, k, blending) in rx {
                let n = k.div_euclid(every);
                let made = match blender.as_mut().filter(|_| blending) {
                    Some(b) => {
                        // A frame of a later one: the one being made never
                        // got its last (left out), and is let go of.
                        if summing.is_some_and(|s| s != n) {
                            let _ = b.finish(0);
                        }
                        summing = Some(n);
                        b.hold(frame);
                        b.add(0, 1.0);
                        if k.rem_euclid(every) != every - 1 {
                            continue;
                        }
                        summing = None;
                        match b.finish(0) {
                            Some(average) => average,
                            None => continue,
                        }
                    }
                    None => {
                        // Blending stopped partway through one: let go of.
                        if summing.take().is_some()
                            && let Some(b) = blender.as_mut()
                        {
                            let _ = b.finish(0);
                        }
                        if k.rem_euclid(every) != 0 {
                            continue;
                        }
                        frame
                    }
                };
                // Made the rendition's size in a buffer of its own (the
                // screen's own go back to it at once), then encoded.
                let scaled = scaler.scale(retain(&made.0));
                drop(made);
                match scaled.and_then(|m| Ok(encoder.0.encode(m.ready()?, n, fps))) {
                    Ok(true) => {}
                    Ok(false) => skipped += 1,
                    Err(e) => crate::eprintln_later!("stream: {e:#}"),
                }
            }
            if skipped > 0 {
                eprintln!("stream: its encoder couldn't take {skipped} frames");
            }
            encoder.0.finish();
        })?;
        Ok(Self { every, tx: Some(tx), thread: Some(thread), dropped: 0 })
    }

    /// The pacer's frame `k`, `frame` (`blending`: frame blending's on):
    /// handed over if it's wanted, every one while it averages its own,
    /// else those at its own frames' times.
    fn offer(&mut self, k: i64, frame: &Arc<Buffer>, blending: bool) {
        if !(blending && self.every > 1) && k.rem_euclid(self.every) != 0 {
            return;
        }
        if let Some(tx) = &self.tx
            && tx.try_send((frame.clone(), k, blending)).is_err()
        {
            self.dropped += 1;
        }
    }

    /// The frames handed in encoded and sent on (waited for).
    pub(crate) fn finish(mut self) {
        drop(self.tx.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        if self.dropped > 0 {
            eprintln!("stream: {} frames left out, it was busy", self.dropped);
        }
    }
}

/// A frame waiting to be encoded: a blended one the GPU is still making
/// (`work`), or one made already that waits behind such frames, as frames
/// are encoded in order.
struct Making {
    k: i64,
    made: Arc<Buffer>,
    work: Option<Retained<ProtocolObject<dyn MTLCommandBuffer>>>,
    /// How late it was closed, how long closing it took, and when.
    late: f64,
    mixed: Duration,
    closed: Instant,
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

    /// Frame `k` is done: encoded (with the webcam over it) once the GPU has
    /// made it, frames in order. The pacer doesn't wait for the GPU (see
    /// `MOST_BEHIND`).
    fn close(&mut self, k: i64) {
        self.closed = k;
        let started = Instant::now();
        // After its last look's time and the room a picture has to come in.
        let late = host_now() - self.look_time(self.shutter.last_look(k)) - BLEND_LATE;
        let Some(made) = self.mix.finish(k) else { return };
        let mixed = started.elapsed();
        let work = self.mix.work();
        if work.is_none() && self.making.is_empty() {
            self.encode(k, made, late, mixed, None);
        } else {
            // After the frames before it: blending turned off, the last
            // blended ones can still be on the GPU, and a frame encoded
            // ahead of them broke the clip or recording it was in
            // (AVAssetWriter's -16364 at its end).
            self.making.push_back(Making { k, made, work, late, mixed, closed: started });
        }
        self.encode_made(false);
    }

    /// Encode the frames that are made, in order; `all`, those the GPU
    /// hasn't made yet too, once it has (stopping).
    fn encode_made(&mut self, all: bool) {
        while let Some(m) = self.making.front() {
            if let Some(work) = m.work.as_ref().filter(|w| !gpu_done(w)) {
                if !all {
                    break;
                }
                work.waitUntilCompleted();
            }
            let Some(m) = self.making.pop_front() else { break };
            let gpu = m.work.is_some().then(|| m.closed.elapsed());
            self.encode(m.k, m.made, m.late, m.mixed, gpu);
        }
    }

    /// Encode frame `k`, `made` (closed `late`, in `mixed`; made by the GPU
    /// `gpu` after it was closed, when it's blended).
    fn encode(&mut self, k: i64, made: Arc<Buffer>, late: f64, mixed: Duration, gpu: Option<Duration>) {
        let started = Instant::now();
        let frame = self.frames.make(made);
        // The stream's pictures', where they want it (whether or not the
        // recording's encoder takes it).
        let blending = self.looks > 1;
        for s in &mut self.stream {
            s.offer(k, &frame, blending);
        }
        let composed = started.elapsed();
        let free = self.encoder.0.free();
        let encoded = self.encoder.0.encode(&frame.0, k, self.fps);
        let steps = Steps { mix: mixed, make: composed, encode: started.elapsed() - composed };
        if encoded {
            crate::count_encoded();
        } else {
            crate::count_skipped(1);
            self.skipped += 1;
            if self.skipped.is_power_of_two() {
                crate::eprintln_later!("video encoder is behind: {} frames skipped so far", self.skipped);
            }
        }
        self.report.frame(late, steps, gpu, free, !encoded, &self.encoder.0);
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
        let adding = Instant::now();
        self.up = Some(picture.clone());
        self.mix.hold(picture);
        self.report.added(adding.elapsed());
        self.held = Some(id);
        self.stats.pictures += 1;
    }

    /// Blend as `c` says from the next frame on: the frame under way is made
    /// from the looks it gets from now (at the new rate, by the new mix),
    /// and the looks lock onto the pictures again. The old mix's frames
    /// still being made are encoded as they're done.
    fn change_blend(&mut self, c: BlendChange) {
        let looks = c.looks.max(1);
        let k = self.closed + 1;
        self.looks = looks as i64;
        self.step = 1.0 / (self.fps.max(1) * looks) as f64;
        self.shutter = c.shutter;
        self.mix = c.mix;
        if let Some(up) = &self.up {
            self.mix.hold(up.clone());
        }
        // Frame `k`'s own time is where it was: looks `k * looks` on, at the
        // new step. The first picture the stream times puts them halfway
        // between pictures again.
        self.phase = 0.0;
        self.timed = false;
        self.next = self.shutter.first_look(k);
        self.different = 0;
        self.stats = BlendStats::new();
        crate::blending(self.fps, looks);
        crate::eprintln_later!("{}", if looks > 1 { format!("frame blending: {looks} looks a frame from now on") } else { "frame blending: off from now on".to_owned() });
    }

    /// The stream started or stopped, if the session says so. A stopped
    /// stream's pictures are finished on a thread of their own (that waits
    /// for their encoders).
    fn take_stream(&mut self, ctl: &PacerCtl) {
        let Some(change) = ctl.stream.lock().unwrap_or_else(PoisonError::into_inner).take() else { return };
        let old = std::mem::replace(&mut self.stream, change);
        if old.is_empty() {
            return;
        }
        let _ = thread::Builder::new().name("stream-end".into()).spawn(move || old.into_iter().for_each(StreamOut::finish));
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

/// How long each step of making a frame took (see `BlendPacer::close`).
#[derive(Clone, Copy, Default)]
struct Steps {
    /// The looks' mixing given to the graphics card, or not blended, the
    /// picture taken.
    mix: Duration,
    /// The webcam put over it, or the screen into its box (the graphics card
    /// waited for), and the preview offered it.
    make: Duration,
    /// Handed to the encoder.
    encode: Duration,
}

/// Where frames wait, logged for every 5 s in which frames were skipped
/// (every 5 s with `HESTECLIPS_ENCODE_LOG=1`), as on Windows, so a log from a
/// Mac that can't keep up says what it's short of: how late frames were made
/// and how long making one took, and which of its steps the slowest one's
/// time went to; how long after a blended frame was closed the graphics card
/// had made it, and the longest a picture took to be added to a blend (the
/// graphics card is waited for when it's behind); how long the encoder held
/// frames; the fewest free places in its queue; and how many frames were
/// skipped.
struct EncodeReport {
    verbose: bool,
    since: Instant,
    frames: u32,
    late_max: f64,
    close_max: Duration,
    slowest: Steps,
    gpu_max: Duration,
    add_max: Duration,
    free_min: usize,
    skipped: u32,
}

impl EncodeReport {
    fn new() -> Self {
        Self {
            verbose: std::env::var_os("HESTECLIPS_ENCODE_LOG").is_some(),
            since: Instant::now(),
            frames: 0,
            late_max: 0.0,
            close_max: Duration::ZERO,
            slowest: Steps::default(),
            gpu_max: Duration::ZERO,
            add_max: Duration::ZERO,
            free_min: usize::MAX,
            skipped: 0,
        }
    }

    /// A picture took `took` to be added to the frames it counts toward.
    fn added(&mut self, took: Duration) {
        self.add_max = self.add_max.max(took);
    }

    /// A frame closed `late` seconds after its time, made by the GPU `gpu`
    /// after (blended), in `steps`, with `free` places in the encoder's
    /// queue; `skipped` if the encoder couldn't take it.
    fn frame(&mut self, late: f64, steps: Steps, gpu: Option<Duration>, free: usize, skipped: bool, encoder: &Encoder) {
        self.frames += 1;
        self.gpu_max = self.gpu_max.max(gpu.unwrap_or_default());
        self.late_max = self.late_max.max(late);
        let took = steps.mix + steps.make + steps.encode;
        if took > self.close_max {
            self.close_max = took;
            self.slowest = steps;
        }
        self.free_min = self.free_min.min(free);
        self.skipped += skipped as u32;
        if self.since.elapsed() < Duration::from_secs(5) {
            return;
        }
        let (held, held_most, held_count) = encoder.held();
        if self.verbose || self.skipped > 0 {
            let ms = |d: Duration| d.as_secs_f64() * 1000.0;
            crate::eprintln_later!(
                "encode: {} frames; closed up to {:.1} ms late, making one took up to {:.1} ms (mixing {:.1}, compositor {:.1}, encoder {:.1}); blended frames made by the GPU up to {:.1} ms after closing, a picture took up to {:.1} ms to add; encoder held frames {:.1} ms avg, {:.1} max; fewest free places {}; skipped {}",
                self.frames,
                self.late_max * 1000.0,
                ms(self.close_max),
                ms(self.slowest.mix),
                ms(self.slowest.make),
                ms(self.slowest.encode),
                ms(self.gpu_max),
                ms(self.add_max),
                ms(held) / held_count.max(1) as f64,
                ms(held_most),
                self.free_min,
                self.skipped
            );
        }
        *self = Self { verbose: self.verbose, ..Self::new() };
    }
}

/// The pacer: `looks` looks at the screen per frame, each taking the picture
/// that was up at its time (by when the screen showed it), kept halfway
/// between the screen's pictures; the frame is made of them as `mix` says
/// (blended, or one look's picture as it is). Frame `k` is stamped `k / fps`
/// on the host clock, the audio's: a frame is made every 1/fps whether the
/// screen changed or not (constant frame rate), and when the encoder can't
/// take one it's skipped, not queued, so video never falls behind. What
/// `ctl` says changed (frame blending, the stream) is taken up at its next
/// turn.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_blend_pacer(
    fps: u32,
    looks: u32,
    shutter: Shutter,
    latest: Arc<Latest>,
    mix: Mix,
    frames: Frames,
    encoder: Encoder,
    clock: Arc<Clock>,
    ctl: PacerCtl,
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
            up: None,
            different: 0,
            mix,
            frames,
            encoder,
            stream: Vec::new(),
            skipped: 0,
            stats: BlendStats::new(),
            report: EncodeReport::new(),
            making: Default::default(),
        };
        let mut seen = latest.seq();
        if let Some(b) = latest.get() {
            p.up = Some(b.clone());
            p.mix.hold(b);
            p.held = Some(seen);
        }
        while !stop2.load(Ordering::Relaxed) {
            // Frame blending changed, the stream started or stopped.
            let change = ctl.blend.lock().unwrap_or_else(PoisonError::into_inner).take();
            if let Some(c) = change {
                p.change_blend(c);
            }
            p.take_stream(&ctl);
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
            p.encode_made(false);
        }
        p.encode_made(true);
        for s in std::mem::take(&mut p.stream) {
            s.finish();
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

    /// Frame `k` finished, once the GPU has made it (the blender doesn't
    /// wait for it).
    fn made(b: &mut Blender, k: i64) -> Option<Arc<Buffer>> {
        let out = b.finish(k)?;
        if let Some(work) = b.last_work() {
            work.waitUntilCompleted();
        }
        Some(out)
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
        let out = made(&mut b, 0).unwrap();
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
        let (y, cb, cr) = sample(&made(&mut b, 0).unwrap().0);
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
        let (y0, _, _) = sample(&made(&mut b, 0).unwrap().0);
        let (y1, _, _) = sample(&made(&mut b, 1).unwrap().0);
        // sRGB of 0.75 light is 0.881: 16 + 219 * 0.881 = 209.
        assert!(y0.abs_diff(134) <= 1, "{y0}");
        assert!(y1.abs_diff(209) <= 1, "{y1}");
    }
}
