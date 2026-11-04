//! The recording's frame made on the graphics card (`super::gpu`): the
//! picture put into its box in the frame (`crate::ScreenPlacement`), the
//! webcam over it or under it, and NV12 for the encoder, in compute shaders;
//! only the NV12 frame comes down to the CPU (an eighth of the screen's
//! pixels at 1440p → 1080p).
//! The same as `super::image` does on the CPU, which this replaces when the
//! screen's pictures are on the card.
//!
//! A picture that arrives in our memory (the away screen, X11) is uploaded
//! first. The webcam's picture is uploaded as it comes and drawn into its
//! box here too, into a layer laid over the frame, when it or its box
//! changes (as `Canvas::draw_camera` on the CPU, which took 12-46% of a
//! core at 30 fps for a quarter- to half-wide box).

use std::sync::Arc;

use super::gpu::Gpu;
use super::image::CameraPicture;
use super::screen::{Frame, Order};
use crate::webcam::Placement;

/// How the screen's flipped, for the shaders' `unturn`: 1 mirrored, 2 upside down.
fn flips(p: &crate::ScreenPlacement) -> u32 {
    u32::from(p.flip_h) | u32::from(p.flip_v) << 1
}

const SCALE_SHADER: &str = r#"
struct Scale {
    // The part of the source shown (x, y, w, h, in its own pixels), and
    // where in the frame (x, y, w, h).
    src: vec4<f32>,
    dst: vec4<f32>,
    // The frame's size, whether a webcam layer is laid over it, and whether
    // the picture is in front of it instead.
    size: vec2<u32>,
    layer: u32,
    front: u32,
    // How the picture's turned and flipped (see `unturn`).
    turns: u32,
    flips: u32,
    _pad: vec2<u32>,
}

@group(0) @binding(0) var source: texture_2d<f32>;
@group(0) @binding(1) var frame: texture_storage_2d<rgba8unorm, write>;
@group(0) @binding(2) var<uniform> s: Scale;
@group(0) @binding(3) var overlay: texture_2d<f32>;

// A point of the picture's box (0 to 1 across and down it) in the part of
// the source used (0 to 1): flipped back, then turned back, as it's turned
// and then flipped (`ScreenPlacement::unturn`). `flips`: 1 mirrored, 2 upside down.
fn unturn(q0: vec2<f32>, turns: u32, flips: u32) -> vec2<f32> {
    var q = q0;
    if ((flips & 1u) != 0u) {
        q.x = 1.0 - q.x;
    }
    if ((flips & 2u) != 0u) {
        q.y = 1.0 - q.y;
    }
    switch turns {
        case 1u: { q = vec2<f32>(q.y, 1.0 - q.x); }
        case 2u: { q = vec2<f32>(1.0 - q.x, 1.0 - q.y); }
        case 3u: { q = vec2<f32>(1.0 - q.y, q.x); }
        default: {}
    }
    return q;
}

// An interval of the source at least one sample wide (enlarging then blends
// the nearest two), within [lo, hi], as `Taps` does on the CPU.
fn widen(a0: f32, b0: f32, lo: f32, hi: f32) -> vec2<f32> {
    var a = a0;
    var b = b0;
    if (b - a < 1.0) {
        let c = (a + b) / 2.0;
        a = max(c - 0.5, lo);
        b = min(a + 1.0, hi);
        a = b - 1.0;
    }
    return vec2<f32>(a, b);
}

// The source area the output pixel at `p` covers (x0, x1, y0, y1): its
// corners through the turn and flip, into `src` (x, y, w, h) from `dst`.
fn area(p: vec2<f32>, src: vec4<f32>, dst: vec4<f32>, turns: u32, flips: u32) -> vec4<f32> {
    let a = src.xy + unturn((p - dst.xy) / dst.zw, turns, flips) * src.zw;
    let b = src.xy + unturn((p + 1.0 - dst.xy) / dst.zw, turns, flips) * src.zw;
    let lo = min(a, b);
    let hi = max(a, b);
    return vec4<f32>(widen(lo.x, hi.x, src.x, src.x + src.z), widen(lo.y, hi.y, src.y, src.y + src.w));
}

// Each frame pixel: the average of the source area it covers; black outside
// the picture's place; the webcam layer over it where it's drawn (or only
// outside the picture, when the picture's in front).
@compute @workgroup_size(8, 8)
fn scale(@builtin(global_invocation_id) id: vec3<u32>) {
    if (id.x >= s.size.x || id.y >= s.size.y) {
        return;
    }
    var c = vec3<f32>(0.0);
    let p = vec2<f32>(id.xy);
    let inside = p.x >= s.dst.x && p.y >= s.dst.y && p.x < s.dst.x + s.dst.z && p.y < s.dst.y + s.dst.w;
    if (inside) {
        let a = area(p, s.src, s.dst, s.turns, s.flips);
        let sx = a.xy;
        let sy = a.zw;
        var sum = vec3<f32>(0.0);
        var total = 0.0;
        for (var y = floor(sy.x); y < sy.y; y += 1.0) {
            let wy = min(sy.y, y + 1.0) - max(sy.x, y);
            for (var x = floor(sx.x); x < sx.y; x += 1.0) {
                let w = (min(sx.y, x + 1.0) - max(sx.x, x)) * wy;
                sum += textureLoad(source, vec2<i32>(i32(x), i32(y)), 0).rgb * w;
                total += w;
            }
        }
        c = sum / max(total, 1e-6);
    }
    if (s.layer != 0u && !(inside && s.front != 0u)) {
        let l = textureLoad(overlay, vec2<i32>(id.xy), 0);
        if (l.a > 0.5) {
            c = l.rgb;
        }
    }
    textureStore(frame, vec2<i32>(id.xy), vec4<f32>(c, 1.0));
}
"#;

const BLEND_SHADER: &str = r#"
struct Look {
    src: vec4<f32>,
    dst: vec4<f32>,
    size: vec2<u32>,
    weight: f32,
    _pad: f32,
    turns: u32,
    flips: u32,
    _pad2: vec2<u32>,
}

@group(0) @binding(0) var source: texture_2d<f32>;
@group(0) @binding(1) var<storage, read_write> sum: array<vec4<f32>>;
@group(0) @binding(2) var<uniform> l: Look;

// A point of the picture's box (0 to 1 across and down it) in the part of
// the source used (0 to 1): flipped back, then turned back, as it's turned
// and then flipped (`ScreenPlacement::unturn`). `flips`: 1 mirrored, 2 upside down.
fn unturn(q0: vec2<f32>, turns: u32, flips: u32) -> vec2<f32> {
    var q = q0;
    if ((flips & 1u) != 0u) {
        q.x = 1.0 - q.x;
    }
    if ((flips & 2u) != 0u) {
        q.y = 1.0 - q.y;
    }
    switch turns {
        case 1u: { q = vec2<f32>(q.y, 1.0 - q.x); }
        case 2u: { q = vec2<f32>(1.0 - q.x, 1.0 - q.y); }
        case 3u: { q = vec2<f32>(1.0 - q.y, q.x); }
        default: {}
    }
    return q;
}

// An interval of the source at least one sample wide (enlarging then blends
// the nearest two), within [lo, hi], as `Taps` does on the CPU.
fn widen(a0: f32, b0: f32, lo: f32, hi: f32) -> vec2<f32> {
    var a = a0;
    var b = b0;
    if (b - a < 1.0) {
        let c = (a + b) / 2.0;
        a = max(c - 0.5, lo);
        b = min(a + 1.0, hi);
        a = b - 1.0;
    }
    return vec2<f32>(a, b);
}

// The source area the output pixel at `p` covers (x0, x1, y0, y1): its
// corners through the turn and flip, into `src` (x, y, w, h) from `dst`.
fn area(p: vec2<f32>, src: vec4<f32>, dst: vec4<f32>, turns: u32, flips: u32) -> vec4<f32> {
    let a = src.xy + unturn((p - dst.xy) / dst.zw, turns, flips) * src.zw;
    let b = src.xy + unturn((p + 1.0 - dst.xy) / dst.zw, turns, flips) * src.zw;
    let lo = min(a, b);
    let hi = max(a, b);
    return vec4<f32>(widen(lo.x, hi.x, src.x, src.x + src.z), widen(lo.y, hi.y, src.y, src.y + src.w));
}

// sRGB's values to light.
fn linear(c: vec3<f32>) -> vec3<f32> {
    return select(pow((c + 0.055) / 1.055, vec3<f32>(2.4)), c / 12.92, c <= vec3<f32>(0.04045));
}

// A look: the picture put into the frame's size (as `scale`), its light
// added to the frame's sum with the look's weight.
@compute @workgroup_size(8, 8)
fn accumulate(@builtin(global_invocation_id) id: vec3<u32>) {
    if (id.x >= l.size.x || id.y >= l.size.y) {
        return;
    }
    var c = vec3<f32>(0.0);
    let p = vec2<f32>(id.xy);
    if (p.x >= l.dst.x && p.y >= l.dst.y && p.x < l.dst.x + l.dst.z && p.y < l.dst.y + l.dst.w) {
        let a = area(p, l.src, l.dst, l.turns, l.flips);
        let sx = a.xy;
        let sy = a.zw;
        var total = 0.0;
        for (var y = floor(sy.x); y < sy.y; y += 1.0) {
            let wy = min(sy.y, y + 1.0) - max(sy.x, y);
            for (var x = floor(sx.x); x < sx.y; x += 1.0) {
                let w = (min(sx.y, x + 1.0) - max(sx.x, x)) * wy;
                c += textureLoad(source, vec2<i32>(i32(x), i32(y)), 0).rgb * w;
                total += w;
            }
        }
        c = linear(c / max(total, 1e-6));
    }
    let i = id.y * l.size.x + id.x;
    sum[i] += vec4<f32>(c * l.weight, 0.0);
}
"#;

const RESOLVE_SHADER: &str = r#"
struct Resolve {
    size: vec2<u32>,
    total: f32,
    layer: u32,
    // Where the picture is, and whether it's in front of the webcam layer.
    dst: vec4<f32>,
    front: u32,
    _pad: vec3<u32>,
}

@group(0) @binding(0) var<storage, read> sum: array<vec4<f32>>;
@group(0) @binding(1) var frame: texture_storage_2d<rgba8unorm, write>;
@group(0) @binding(2) var<uniform> r: Resolve;
@group(0) @binding(3) var overlay: texture_2d<f32>;

fn srgb(c: vec3<f32>) -> vec3<f32> {
    return select(1.055 * pow(c, vec3<f32>(1.0 / 2.4)) - 0.055, c * 12.92, c <= vec3<f32>(0.0031308));
}

// The frame: its looks' average light, back to sRGB; the webcam over it
// (or only outside the picture, when the picture's in front).
@compute @workgroup_size(8, 8)
fn resolve(@builtin(global_invocation_id) id: vec3<u32>) {
    if (id.x >= r.size.x || id.y >= r.size.y) {
        return;
    }
    var c = srgb(clamp(sum[id.y * r.size.x + id.x].rgb / r.total, vec3<f32>(0.0), vec3<f32>(1.0)));
    let p = vec2<f32>(id.xy);
    let inside = p.x >= r.dst.x && p.y >= r.dst.y && p.x < r.dst.x + r.dst.z && p.y < r.dst.y + r.dst.w;
    if (r.layer != 0u && !(inside && r.front != 0u)) {
        let l = textureLoad(overlay, vec2<i32>(id.xy), 0);
        if (l.a > 0.5) {
            c = l.rgb;
        }
    }
    textureStore(frame, vec2<i32>(id.xy), vec4<f32>(c, 1.0));
}
"#;

const CAMERA_SHADER: &str = r#"
struct Cam {
    // The frame's size, and the camera picture's (as it comes, unturned).
    size: vec2<u32>,
    camera: vec2<u32>,
    // The box (x, y, w, h, in the frame's pixels), and the part of the
    // turned picture shown (u0, u1, v0, v1, in its pixels).
    box_: vec4<f32>,
    part: vec4<f32>,
    // Turned a quarter this many times; flipped (1 across, 2 upside down);
    // whether it's shown.
    turns: u32,
    flips: u32,
    on: u32,
    _pad: u32,
}

@group(0) @binding(0) var camera: texture_2d<f32>;
@group(0) @binding(1) var layer: texture_storage_2d<rgba8unorm, write>;
@group(0) @binding(2) var<uniform> c: Cam;

// A point of the turned picture in the camera's own (`Placement::unturn`).
fn unturn(p: vec2<f32>, size: vec2<f32>, turns: u32) -> vec2<f32> {
    switch turns {
        case 1u: { return vec2<f32>(p.y, size.y - p.x); }
        case 2u: { return vec2<f32>(size.x - p.x, size.y - p.y); }
        case 3u: { return vec2<f32>(size.x - p.y, p.x); }
        default: { return p; }
    }
}

// The camera's colour at a point (pixel centres at whole numbers), blended
// between the four nearest pixels in 256ths (`CameraPicture::sample`).
fn sample(p0: vec2<f32>) -> vec3<f32> {
    let last = vec2<f32>(c.camera) - 1.0;
    let p = clamp(p0, vec2<f32>(0.0), last);
    let a = vec2<i32>(p);
    let b = min(a + 1, vec2<i32>(c.camera) - 1);
    let f = floor((p - vec2<f32>(a)) * 256.0) / 256.0;
    let top = mix(textureLoad(camera, a, 0).rgb, textureLoad(camera, vec2<i32>(b.x, a.y), 0).rgb, f.x);
    let bottom = mix(textureLoad(camera, vec2<i32>(a.x, b.y), 0).rgb, textureLoad(camera, b, 0).rgb, f.x);
    return mix(top, bottom, f.y);
}

// Each pixel of the layer: the camera's picture in its box, clear
// elsewhere (`Canvas::draw_camera`).
@compute @workgroup_size(8, 8)
fn draw_camera(@builtin(global_invocation_id) id: vec3<u32>) {
    if (id.x >= c.size.x || id.y >= c.size.y) {
        return;
    }
    var out = vec4<f32>(0.0);
    var f = (vec2<f32>(id.xy) + 0.5 - c.box_.xy) / c.box_.zw;
    if (c.on != 0u && all(f >= vec2<f32>(0.0)) && all(f < vec2<f32>(1.0))) {
        if ((c.flips & 1u) != 0u) {
            f.x = 1.0 - f.x;
        }
        if ((c.flips & 2u) != 0u) {
            f.y = 1.0 - f.y;
        }
        let uv = vec2<f32>(c.part.x + f.x * (c.part.y - c.part.x), c.part.z + f.y * (c.part.w - c.part.z));
        out = vec4<f32>(sample(unturn(uv, vec2<f32>(c.camera), c.turns) - 0.5), 1.0);
    }
    textureStore(layer, vec2<i32>(id.xy), out);
}
"#;

const NV12_SHADER: &str = r#"
struct Size {
    width: u32,
    height: u32,
    // Bytes from one row to the next: the width, or more (an encoder's
    // aligned rows).
    pitch: u32,
    _pad: u32,
}

@group(0) @binding(0) var image: texture_2d<f32>;
@group(0) @binding(1) var<storage, read_write> nv12: array<u32>;
@group(0) @binding(2) var<uniform> n: Size;

fn rgb8(x: u32, y: u32) -> vec3<i32> {
    return vec3<i32>(round(textureLoad(image, vec2<i32>(i32(x), i32(y)), 0).rgb * 255.0));
}

// One byte of NV12 (BT.709, studio range): as `nv12_pair` on the CPU. A
// row's padding past the width is 0.
fn byte_at(b: u32) -> u32 {
    let w = n.width;
    let h = n.height;
    let p = n.pitch;
    if (b < p * h) {
        let x = b % p;
        if (x >= w) {
            return 0u;
        }
        let c = rgb8(x, b / p);
        return u32(((47 * c.r + 157 * c.g + 16 * c.b + 128) >> 8u) + 16);
    }
    let k = b - p * h;
    let x = k % p;
    if (k >= p * h / 2u || x >= w) {
        return 0u;
    }
    let cy = k / p;
    let cx = x / 2u;
    let y0 = cy * 2u;
    let y1 = min(y0 + 1u, h - 1u);
    let sum = rgb8(cx * 2u, y0) + rgb8(cx * 2u + 1u, y0) + rgb8(cx * 2u, y1) + rgb8(cx * 2u + 1u, y1);
    var v: i32;
    if (x % 2u == 0u) {
        v = 128 + ((-26 * sum.r - 86 * sum.g + 112 * sum.b + 512) >> 10u);
    } else {
        v = 128 + ((112 * sum.r - 102 * sum.g - 10 * sum.b + 512) >> 10u);
    }
    return u32(clamp(v, 16, 240));
}

// Four bytes of NV12 a thread: luma rows, then interleaved U, V rows.
@compute @workgroup_size(64)
fn to_nv12(@builtin(global_invocation_id) id: vec3<u32>) {
    let i = id.x + id.y * 65535u * 64u;
    if (i >= arrayLength(&nv12)) {
        return;
    }
    let b = i * 4u;
    nv12[i] = byte_at(b) | (byte_at(b + 1u) << 8u) | (byte_at(b + 2u) << 16u) | (byte_at(b + 3u) << 24u);
}
"#;

/// The frame being made on the card, and what it shows.
pub(crate) struct GpuPicture {
    gpu: Arc<Gpu>,
    pub width: u32,
    pub height: u32,
    /// Where the picture goes (and the part of it used), as last set
    /// (`set_screen`).
    screen: crate::ScreenPlacement,
    scale: wgpu::ComputePipeline,
    nv12: wgpu::ComputePipeline,
    /// The frame, RGBA.
    frame: wgpu::Texture,
    uniforms: wgpu::Buffer,
    /// NV12 on the card, and where it's read back.
    nv12_buf: wgpu::Buffer,
    readback: wgpu::Buffer,
    /// The webcam's layer: drawn on the CPU (only its box), laid over the frame.
    layer: Layer,
    /// A picture from our memory, uploaded: its texture and its number.
    uploaded: Option<(wgpu::Texture, u64)>,
    nv12_bind: wgpu::BindGroup,
    /// Frame blending's sums, once it's started.
    blend: Option<Blend>,
    /// The preview, shrunk on the card: its texture, where it's read back,
    /// and its uniforms.
    preview: Option<PreviewTarget>,
}

/// What an encoder takes its frames from on the card, as NV12: the
/// recording's picture ([`GpuPicture`]), or an export's decoded frames
/// scaled as they are (`export`).
pub(crate) trait Nv12Source {
    /// The card it's on.
    fn gpu(&self) -> &Arc<Gpu>;

    /// A place an encoder takes frames from (`super::nvenc`,
    /// `super::vkenc`): `buffer`, NV12 with rows `pitch` bytes apart, made by
    /// [`Self::nv12_to`] at `size` (the frame's, or more: black past it),
    /// and then copied into `picture` (NV12 of that size) if there is one.
    fn nv12_target(&self, buffer: wgpu::Buffer, pitch: u32, size: (u32, u32), picture: Option<wgpu::Texture>) -> Nv12Target;

    /// The frame as NV12 into `target`, on the card: done when this returns.
    fn nv12_to(&self, target: &Nv12Target);
}

/// Where an encoder takes frames from on the card ([`Nv12Source::nv12_target`]).
pub(crate) struct Nv12Target {
    buffer: wgpu::Buffer,
    bind: wgpu::BindGroup,
    pitch: u32,
    size: (u32, u32),
    /// The encoder's picture, copied into.
    picture: Option<wgpu::Texture>,
}

impl Nv12Target {
    /// `buffer` and the rest as given to `nv12_target`, `bind` the source's
    /// bindings for writing into it.
    pub(crate) fn new(buffer: wgpu::Buffer, bind: wgpu::BindGroup, pitch: u32, size: (u32, u32), picture: Option<wgpu::Texture>) -> Self {
        Self { buffer, bind, pitch, size, picture }
    }

    /// Fill it by `pipeline` (four bytes a thread, as `to_nv12`) and copy
    /// that into the encoder's picture: done when this returns.
    pub(crate) fn fill(&self, gpu: &Gpu, pipeline: &wgpu::ComputePipeline) {
        let device = &gpu.device;
        let groups = ((self.buffer.size() / 4) as u32).div_ceil(64);
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("nv12 out") });
        {
            let mut pass = enc.begin_compute_pass(&Default::default());
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, &self.bind, &[]);
            pass.dispatch_workgroups(groups.min(65535), groups.div_ceil(65535), 1);
        }
        if let Some(picture) = &self.picture {
            // Luma, then the U, V pairs at half the size each way.
            let (w, h) = self.size;
            for (plane, offset, size) in [(wgpu::TextureAspect::Plane0, 0, (w, h)), (wgpu::TextureAspect::Plane1, self.pitch as u64 * h as u64, (w / 2, h / 2))] {
                enc.copy_buffer_to_texture(
                    wgpu::TexelCopyBufferInfo { buffer: &self.buffer, layout: wgpu::TexelCopyBufferLayout { offset, bytes_per_row: Some(self.pitch), rows_per_image: Some(size.1) } },
                    wgpu::TexelCopyTextureInfo { texture: picture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: plane },
                    wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
                );
            }
        }
        let index = gpu.queue.submit([enc.finish()]);
        let _ = device.poll(wgpu::PollType::Wait { submission_index: Some(index), timeout: None });
    }
}

/// The frame made another size on the card, for a stream's pictures
/// ([`GpuPicture::scaled`]): the picture that size, the scale's uniforms,
/// NV12 made from it, and where that's read back.
pub(crate) struct Scaled {
    width: u32,
    height: u32,
    texture: wgpu::Texture,
    uniforms: wgpu::Buffer,
    nv12_buf: wgpu::Buffer,
    readback: wgpu::Buffer,
    nv12_bind: wgpu::BindGroup,
}

/// A stream frame made of every frame in its own time (frame blending on,
/// the stream at a fraction of the frame rate: 48 fps from 144): their
/// light averaged, as a frame's looks are, so its motion is blurred over
/// all of its time as the recording's is over each frame's (as Windows'
/// `StreamBlend`; one frame in three instead blurred a third of it).
/// [`GpuPicture::frame_sum`].
pub(crate) struct FrameSum {
    accumulate: wgpu::ComputePipeline,
    resolve: wgpu::ComputePipeline,
    /// The frames' light added up, a pixel each.
    sum: wgpu::Buffer,
    /// The accumulate's uniforms (the same for every frame), and the
    /// resolve's.
    look: wgpu::Buffer,
    resolve_uniforms: wgpu::Buffer,
    /// The average, as the frame is: RGBA, its size.
    texture: wgpu::Texture,
    /// Frames added since it was last made or let go of.
    count: u32,
}

/// A small copy of the frame for the preview, kept for its size.
struct PreviewTarget {
    texture: wgpu::Texture,
    readback: wgpu::Buffer,
    uniforms: wgpu::Buffer,
    /// Bytes from one row to the next in `readback` (as wgpu pads them).
    row: u32,
}

/// Frame blending on the card: each open frame's sum of light, and the
/// picture the looks now see.
struct Blend {
    accumulate: wgpu::ComputePipeline,
    resolve: wgpu::ComputePipeline,
    slots: Vec<Slot>,
    resolve_uniforms: wgpu::Buffer,
    held: Option<wgpu::Texture>,
    /// Where the looks put the picture, last: where the frame has it.
    dst: [u32; 4],
}

/// One open frame: its sum, which frame it is, its looks' weight so far
/// added and still to add (a picture seen by several looks in a row is
/// added once, with all their weight).
struct Slot {
    sum: wgpu::Buffer,
    uniforms: wgpu::Buffer,
    frame: Option<i64>,
    added: f32,
    pending: f32,
}

/// The webcam over the frame: its picture drawn into its box on the card,
/// clear where it isn't.
struct Layer {
    texture: wgpu::Texture,
    draw: wgpu::ComputePipeline,
    uniforms: wgpu::Buffer,
    /// The camera's picture on the card, and which picture it is
    /// (`CameraPicture::seq`).
    picture: Option<(wgpu::Texture, u64)>,
    /// Whether anything is drawn.
    on: bool,
}

impl GpuPicture {
    pub(crate) fn new(gpu: Arc<Gpu>, width: u32, height: u32, screen: crate::ScreenPlacement) -> Self {
        let device = &gpu.device;
        let pipeline = |source: &str, entry: &str| {
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some(entry), source: wgpu::ShaderSource::Wgsl(source.into()) });
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor { label: Some(entry), layout: None, module: &module, entry_point: Some(entry), compilation_options: Default::default(), cache: None })
        };
        let (scale, nv12) = (pipeline(SCALE_SHADER, "scale"), pipeline(NV12_SHADER, "to_nv12"));
        let texture = |label, usage| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage,
                view_formats: &[],
            })
        };
        let frame = texture("frame", wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC);
        let overlay = texture("webcam layer", wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC);
        let uniform = |label| device.create_buffer(&wgpu::BufferDescriptor { label: Some(label), size: 64, usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
        let (uniforms, sizes) = (uniform("scale"), uniform("size"));
        let bytes = nv12_len(width, height).div_ceil(4) as u64 * 4;
        let nv12_buf = device.create_buffer(&wgpu::BufferDescriptor { label: Some("nv12"), size: bytes, usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC, mapped_at_creation: false });
        let readback = device.create_buffer(&wgpu::BufferDescriptor { label: Some("nv12 read"), size: bytes, usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false });
        let frame_view = frame.create_view(&Default::default());
        let nv12_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("nv12"),
            layout: &nv12.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&frame_view) },
                wgpu::BindGroupEntry { binding: 1, resource: nv12_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: sizes.as_entire_binding() },
            ],
        });
        let size_bytes: Vec<u8> = [width, height, width, 0].iter().flat_map(|v| v.to_le_bytes()).collect();
        gpu.queue.write_buffer(&sizes, 0, &size_bytes);
        let layer = Layer { texture: overlay, draw: pipeline(CAMERA_SHADER, "draw_camera"), uniforms: uniform("camera"), picture: None, on: false };
        Self { gpu, width, height, screen, scale, nv12, frame, uniforms, nv12_buf, readback, layer, uploaded: None, nv12_bind, blend: None, preview: None }
    }

    /// Where the picture goes from now on.
    pub(crate) fn set_screen(&mut self, screen: crate::ScreenPlacement) {
        self.screen = screen;
    }

    /// Draw the webcam's layer: `cam` placed at `place`, or none. A new
    /// picture is uploaded first (one copy, its own size).
    pub(crate) fn set_camera(&mut self, cam: Option<(&CameraPicture, &Placement)>) {
        let shown = cam.filter(|(c, p)| !p.is_hidden() && c.width > 0 && c.height > 0);
        let (device, queue) = (&self.gpu.device, &self.gpu.queue);
        let l = &mut self.layer;
        if let Some((c, _)) = shown {
            let id = c.seq;
            let size = wgpu::Extent3d { width: c.width, height: c.height, depth_or_array_layers: 1 };
            if l.picture.as_ref().is_none_or(|(t, _)| t.size() != size) {
                let t = device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("webcam picture"),
                    size,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Bgra8Unorm,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                });
                l.picture = Some((t, 0));
            }
            if let Some((t, uploaded)) = &mut l.picture
                && *uploaded != id
            {
                queue.write_texture(t.as_image_copy(), &c.bgra, wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(c.width * 4), rows_per_image: Some(c.height) }, size);
                *uploaded = id;
            }
        }
        l.on = shown.is_some();
        // Nothing to draw and nothing drawn: no work.
        let Some((picture, _)) = &l.picture else { return };
        let (fw, fh) = (self.width as f32, self.height as f32);
        let mut floats = [0f32; 8];
        let mut flags = [0u32; 4];
        let mut camera = [0u32; 2];
        if let Some((c, p)) = shown {
            // As `Canvas::draw_camera` places it.
            let (tw, th) = p.turned((c.width, c.height));
            let crop = p.fill_crop((tw, th), (self.width, self.height));
            let (cw, ch) = (tw as f32, th as f32);
            floats = [p.x * fw, p.y * fh, p.w * fw, p.h * fh, crop[0] * cw, (1.0 - crop[2]) * cw, crop[1] * ch, (1.0 - crop[3]) * ch];
            flags = [u32::from(p.turns % 4), u32::from(p.flip_h) | u32::from(p.flip_v) << 1, 1, 0];
            camera = [c.width, c.height];
        }
        let bytes: Vec<u8> = [self.width, self.height]
            .iter()
            .chain(&camera)
            .flat_map(|v| v.to_le_bytes())
            .chain(floats.iter().flat_map(|v| v.to_le_bytes()))
            .chain(flags.iter().flat_map(|v| v.to_le_bytes()))
            .collect();
        queue.write_buffer(&l.uniforms, 0, &bytes);
        let picture_view = picture.create_view(&Default::default());
        let layer_view = l.texture.create_view(&Default::default());
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("webcam"),
            layout: &l.draw.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&picture_view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&layer_view) },
                wgpu::BindGroupEntry { binding: 2, resource: l.uniforms.as_entire_binding() },
            ],
        });
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("webcam") });
        {
            let mut pass = enc.begin_compute_pass(&Default::default());
            pass.set_pipeline(&l.draw);
            pass.set_bind_group(0, &bind, &[]);
            pass.dispatch_workgroups(self.width.div_ceil(8), self.height.div_ceil(8), 1);
        }
        queue.submit([enc.finish()]);
    }

    /// Make the frame from `src` (picture number `seq`) on the card. Returns
    /// whether it could.
    pub(crate) fn draw(&mut self, src: &Frame, seq: u64) -> bool {
        let texture = match &src.texture {
            Some(t) => t.clone(),
            None => match self.upload(src, seq) {
                Some(t) => t,
                None => return false,
            },
        };
        let (sw, sh) = (texture.width(), texture.height());
        let ([sx, sy, sw, sh], [dx, dy, dw, dh]) = self.screen.place((sw, sh), (self.width, self.height));
        let flags = [self.width, self.height, u32::from(self.layer.on), u32::from(self.screen.in_front), u32::from(self.screen.turns % 4), flips(&self.screen), 0, 0];
        let floats: Vec<u8> = [sx, sy, sw, sh, dx, dy, dw, dh].iter().flat_map(|v| (*v as f32).to_le_bytes()).chain(flags.iter().flat_map(|v| v.to_le_bytes())).collect();
        self.gpu.queue.write_buffer(&self.uniforms, 0, &floats);
        let device = &self.gpu.device;
        let source_view = texture.create_view(&Default::default());
        let frame_view = self.frame.create_view(&Default::default());
        let overlay_view = self.layer.texture.create_view(&Default::default());
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("scale"),
            layout: &self.scale.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&source_view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&frame_view) },
                wgpu::BindGroupEntry { binding: 2, resource: self.uniforms.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(&overlay_view) },
            ],
        });
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
        {
            let mut pass = enc.begin_compute_pass(&Default::default());
            pass.set_pipeline(&self.scale);
            pass.set_bind_group(0, &bind, &[]);
            pass.dispatch_workgroups(self.width.div_ceil(8), self.height.div_ceil(8), 1);
        }
        self.gpu.queue.submit([enc.finish()]);
        true
    }

    /// Blend frames from now on, `open` of them at once (`Shutter::frames_open`).
    /// Blending already, its sums are made anew (the frames under way let
    /// go of) and nothing's held: its kernels stay as they are.
    pub(crate) fn start_blending(&mut self, open: usize) {
        let device = &self.gpu.device;
        let pipeline = |source: &str, entry: &str| {
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some(entry), source: wgpu::ShaderSource::Wgsl(source.into()) });
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor { label: Some(entry), layout: None, module: &module, entry_point: Some(entry), compilation_options: Default::default(), cache: None })
        };
        let uniform = |label| device.create_buffer(&wgpu::BufferDescriptor { label: Some(label), size: 64, usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
        let bytes = self.width as u64 * self.height as u64 * 16;
        let slots: Vec<Slot> = (0..open.max(1))
            .map(|_| Slot {
                sum: device.create_buffer(&wgpu::BufferDescriptor { label: Some("blend sum"), size: bytes, usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false }),
                uniforms: uniform("look"),
                frame: None,
                added: 0.0,
                pending: 0.0,
            })
            .collect();
        if let Some(b) = &mut self.blend {
            b.slots = slots;
            b.held = None;
            return;
        }
        self.blend = Some(Blend { accumulate: pipeline(BLEND_SHADER, "accumulate"), resolve: pipeline(RESOLVE_SHADER, "resolve"), slots, resolve_uniforms: uniform("resolve"), held: None, dst: [0; 4] });
    }

    /// Blend no more: its sums let go of (the frames are drawn again).
    pub(crate) fn stop_blending(&mut self) {
        self.blend = None;
    }

    /// Where the frame is made another size ([`Self::scaled_nv12`]):
    /// `width`×`height`, for a stream's pictures.
    pub(crate) fn scaled(&self, width: u32, height: u32) -> Scaled {
        let device = &self.gpu.device;
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("scaled frame"),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let uniform = |label| device.create_buffer(&wgpu::BufferDescriptor { label: Some(label), size: 64, usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
        let (uniforms, sizes) = (uniform("scaled frame"), uniform("scaled size"));
        // The whole frame into all of it, the webcam's layer not laid over
        // again (it's in the frame), as the preview's.
        let scale: Vec<u8> = [0.0f32, 0.0, self.width as f32, self.height as f32, 0.0, 0.0, width as f32, height as f32]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .chain([width, height, 0, 0, 0, 0, 0, 0].iter().flat_map(|v| v.to_le_bytes()))
            .collect();
        self.gpu.queue.write_buffer(&uniforms, 0, &scale);
        // Its rows packed, as ffmpeg takes them.
        let size: Vec<u8> = [width, height, width, 0].iter().flat_map(|v| v.to_le_bytes()).collect();
        self.gpu.queue.write_buffer(&sizes, 0, &size);
        let bytes = nv12_len(width, height).div_ceil(4) as u64 * 4;
        let nv12_buf = device.create_buffer(&wgpu::BufferDescriptor { label: Some("scaled nv12"), size: bytes, usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC, mapped_at_creation: false });
        let readback = device.create_buffer(&wgpu::BufferDescriptor { label: Some("scaled nv12 read"), size: bytes, usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false });
        let view = texture.create_view(&Default::default());
        let nv12_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("scaled nv12"),
            layout: &self.nv12.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                wgpu::BindGroupEntry { binding: 1, resource: nv12_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: sizes.as_entire_binding() },
            ],
        });
        Scaled { width, height, texture, uniforms, nv12_buf, readback, nv12_bind }
    }

    /// A sum of frames, for a stream's ([`Self::add_to_sum`]): the
    /// accumulate and resolve kernels frame blending's looks use, the whole
    /// frame taken as it is, each frame weighing the same.
    pub(crate) fn frame_sum(&self) -> FrameSum {
        let device = &self.gpu.device;
        let pipeline = |source: &str, entry: &str| {
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some(entry), source: wgpu::ShaderSource::Wgsl(source.into()) });
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor { label: Some(entry), layout: None, module: &module, entry_point: Some(entry), compilation_options: Default::default(), cache: None })
        };
        let uniform = |label| device.create_buffer(&wgpu::BufferDescriptor { label: Some(label), size: 64, usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
        let (w, h) = (self.width, self.height);
        let sum = device.create_buffer(&wgpu::BufferDescriptor { label: Some("stream sum"), size: w as u64 * h as u64 * 16, usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
        let look = uniform("stream look");
        // The whole frame into all of the sum, as it is, by one.
        let bytes: Vec<u8> = [0.0f32, 0.0, w as f32, h as f32, 0.0, 0.0, w as f32, h as f32]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .chain(w.to_le_bytes())
            .chain(h.to_le_bytes())
            .chain(1f32.to_le_bytes())
            .chain(0f32.to_le_bytes())
            .chain([0u32, 0, 0, 0].iter().flat_map(|v| v.to_le_bytes()))
            .collect();
        self.gpu.queue.write_buffer(&look, 0, &bytes);
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("stream average"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        FrameSum { accumulate: pipeline(BLEND_SHADER, "accumulate"), resolve: pipeline(RESOLVE_SHADER, "resolve"), sum, look, resolve_uniforms: uniform("stream resolve"), texture, count: 0 }
    }

    /// The frame as it is now, its light added to `to`'s.
    pub(crate) fn add_to_sum(&self, to: &mut FrameSum) {
        let device = &self.gpu.device;
        let frame_view = self.frame.create_view(&Default::default());
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("stream look"),
            layout: &to.accumulate.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&frame_view) },
                wgpu::BindGroupEntry { binding: 1, resource: to.sum.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: to.look.as_entire_binding() },
            ],
        });
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("stream look") });
        {
            let mut pass = enc.begin_compute_pass(&Default::default());
            pass.set_pipeline(&to.accumulate);
            pass.set_bind_group(0, &bind, &[]);
            pass.dispatch_workgroups(self.width.div_ceil(8), self.height.div_ceil(8), 1);
        }
        self.gpu.queue.submit([enc.finish()]);
        to.count += 1;
    }

    /// `to`'s frames' average light made, back to sRGB, into its picture
    /// (for [`Self::scaled_nv12`]), and its sum emptied. False if no frame
    /// was added.
    pub(crate) fn resolve_sum(&self, to: &mut FrameSum) -> bool {
        if to.count == 0 {
            return false;
        }
        let (w, h) = (self.width, self.height);
        // No webcam layer laid over (it's in each frame).
        let bytes: Vec<u8> = [w.to_le_bytes(), h.to_le_bytes(), (to.count as f32).to_le_bytes(), 0u32.to_le_bytes()]
            .into_iter()
            .chain([0.0f32, 0.0, w as f32, h as f32].map(f32::to_le_bytes))
            .chain([0u32, 0, 0, 0].map(u32::to_le_bytes))
            .collect::<Vec<_>>()
            .concat();
        self.gpu.queue.write_buffer(&to.resolve_uniforms, 0, &bytes);
        let device = &self.gpu.device;
        let out_view = to.texture.create_view(&Default::default());
        let overlay_view = self.layer.texture.create_view(&Default::default());
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("stream resolve"),
            layout: &to.resolve.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: to.sum.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&out_view) },
                wgpu::BindGroupEntry { binding: 2, resource: to.resolve_uniforms.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(&overlay_view) },
            ],
        });
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("stream resolve") });
        {
            let mut pass = enc.begin_compute_pass(&Default::default());
            pass.set_pipeline(&to.resolve);
            pass.set_bind_group(0, &bind, &[]);
            pass.dispatch_workgroups(w.div_ceil(8), h.div_ceil(8), 1);
        }
        enc.clear_buffer(&to.sum, 0, None);
        self.gpu.queue.submit([enc.finish()]);
        to.count = 0;
        true
    }

    /// `to`'s sum let go of (a frame of it left unfinished).
    pub(crate) fn clear_sum(&self, to: &mut FrameSum) {
        if to.count == 0 {
            return;
        }
        let mut enc = self.gpu.device.create_command_encoder(&Default::default());
        enc.clear_buffer(&to.sum, 0, None);
        self.gpu.queue.submit([enc.finish()]);
        to.count = 0;
    }

    /// The frame as it is now, or `from`'s average of frames, made `to`'s
    /// size and NV12 on the card (each pixel the average of what it
    /// covers), read back into `out`.
    pub(crate) fn scaled_nv12(&self, to: &Scaled, from: Option<&FrameSum>, out: &mut Vec<u8>) {
        let device = &self.gpu.device;
        let frame_view = from.map_or(&self.frame, |s| &s.texture).create_view(&Default::default());
        let scaled_view = to.texture.create_view(&Default::default());
        let overlay_view = self.layer.texture.create_view(&Default::default());
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("scaled frame"),
            layout: &self.scale.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&frame_view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&scaled_view) },
                wgpu::BindGroupEntry { binding: 2, resource: to.uniforms.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(&overlay_view) },
            ],
        });
        let groups = ((to.nv12_buf.size() / 4) as u32).div_ceil(64);
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("scaled nv12") });
        {
            let mut pass = enc.begin_compute_pass(&Default::default());
            pass.set_pipeline(&self.scale);
            pass.set_bind_group(0, &bind, &[]);
            pass.dispatch_workgroups(to.width.div_ceil(8), to.height.div_ceil(8), 1);
        }
        {
            let mut pass = enc.begin_compute_pass(&Default::default());
            pass.set_pipeline(&self.nv12);
            pass.set_bind_group(0, &to.nv12_bind, &[]);
            // Past 65535 groups a dimension, rows of them.
            pass.dispatch_workgroups(groups.min(65535), groups.div_ceil(65535), 1);
        }
        enc.copy_buffer_to_buffer(&to.nv12_buf, 0, &to.readback, 0, to.nv12_buf.size());
        let done = self.gpu.queue.submit([enc.finish()]);
        let slice = to.readback.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        let _ = device.poll(wgpu::PollType::Wait { submission_index: Some(done), timeout: None });
        out.clear();
        if let Ok(data) = slice.get_mapped_range() {
            out.extend_from_slice(&data[..nv12_len(to.width, to.height)]);
        }
        to.readback.unmap();
    }

    /// The looks from now on see this picture (number `seq`).
    pub(crate) fn hold(&mut self, src: &Frame, seq: u64) {
        self.flush();
        let texture = match &src.texture {
            Some(t) => Some(t.clone()),
            None => self.upload(src, seq),
        };
        if let Some(b) = &mut self.blend {
            b.held = texture;
        }
    }

    /// A look at the held picture counts toward frame `k` with `weight`.
    pub(crate) fn add(&mut self, k: i64, weight: f32) {
        let Some(b) = &self.blend else { return };
        if b.held.is_none() {
            return;
        }
        let at = k.rem_euclid(b.slots.len() as i64) as usize;
        if b.slots[at].frame != Some(k) {
            // A frame left open whose slot is wanted again: it's dropped.
            if b.slots[at].added > 0.0 {
                self.clear_slot(at);
            }
            let Some(b) = &mut self.blend else { return };
            let slot = &mut b.slots[at];
            slot.frame = Some(k);
            slot.added = 0.0;
            slot.pending = 0.0;
        }
        if let Some(b) = &mut self.blend {
            b.slots[at].pending += weight;
        }
    }

    /// Frame `k`'s looks are all in: make it (into the frame, for `nv12`).
    /// False if it saw no picture.
    pub(crate) fn finish(&mut self, k: i64) -> bool {
        self.flush();
        let (width, height, layer, front) = (self.width, self.height, self.layer.on, self.screen.in_front);
        let Some(b) = &mut self.blend else { return false };
        let n = b.slots.len() as i64;
        let at = k.rem_euclid(n) as usize;
        let slot = &mut b.slots[at];
        if slot.frame != Some(k) || slot.added <= 0.0 {
            slot.frame = None;
            slot.added = 0.0;
            return false;
        }
        let bytes: Vec<u8> = [width.to_le_bytes(), height.to_le_bytes(), slot.added.to_le_bytes(), u32::from(layer).to_le_bytes()]
            .into_iter()
            .chain(b.dst.map(|v| (v as f32).to_le_bytes()))
            .chain([u32::from(front), 0, 0, 0].map(u32::to_le_bytes))
            .collect::<Vec<_>>()
            .concat();
        self.gpu.queue.write_buffer(&b.resolve_uniforms, 0, &bytes);
        let device = &self.gpu.device;
        let frame_view = self.frame.create_view(&Default::default());
        let overlay_view = self.layer.texture.create_view(&Default::default());
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("resolve"),
            layout: &b.resolve.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: slot.sum.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&frame_view) },
                wgpu::BindGroupEntry { binding: 2, resource: b.resolve_uniforms.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(&overlay_view) },
            ],
        });
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("resolve") });
        {
            let mut pass = enc.begin_compute_pass(&Default::default());
            pass.set_pipeline(&b.resolve);
            pass.set_bind_group(0, &bind, &[]);
            pass.dispatch_workgroups(width.div_ceil(8), height.div_ceil(8), 1);
        }
        enc.clear_buffer(&slot.sum, 0, None);
        self.gpu.queue.submit([enc.finish()]);
        slot.frame = None;
        slot.added = 0.0;
        true
    }

    /// Add each open frame's waiting weight of the held picture to its sum.
    fn flush(&mut self) {
        let (width, height, screen) = (self.width, self.height, self.screen);
        let Some(b) = &mut self.blend else { return };
        let Some(held) = &b.held else {
            b.slots.iter_mut().for_each(|s| s.pending = 0.0);
            return;
        };
        if b.slots.iter().all(|s| s.pending <= 0.0) {
            return;
        }
        let ([sx, sy, sw, sh], [dx, dy, dw, dh]) = screen.place((held.width(), held.height()), (width, height));
        b.dst = [dx, dy, dw, dh];
        let device = &self.gpu.device;
        let source_view = held.create_view(&Default::default());
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("looks") });
        for slot in b.slots.iter_mut().filter(|s| s.pending > 0.0) {
            let bytes: Vec<u8> = [sx, sy, sw, sh, dx, dy, dw, dh]
                .iter()
                .flat_map(|v| (*v as f32).to_le_bytes())
                .chain(width.to_le_bytes())
                .chain(height.to_le_bytes())
                .chain(slot.pending.to_le_bytes())
                .chain(0f32.to_le_bytes())
                .chain([u32::from(screen.turns % 4), flips(&screen), 0, 0].iter().flat_map(|v| v.to_le_bytes()))
                .collect();
            self.gpu.queue.write_buffer(&slot.uniforms, 0, &bytes);
            let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("look"),
                layout: &b.accumulate.get_bind_group_layout(0),
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&source_view) },
                    wgpu::BindGroupEntry { binding: 1, resource: slot.sum.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 2, resource: slot.uniforms.as_entire_binding() },
                ],
            });
            {
                let mut pass = enc.begin_compute_pass(&Default::default());
                pass.set_pipeline(&b.accumulate);
                pass.set_bind_group(0, &bind, &[]);
                pass.dispatch_workgroups(width.div_ceil(8), height.div_ceil(8), 1);
            }
            slot.added += slot.pending;
            slot.pending = 0.0;
        }
        self.gpu.queue.submit([enc.finish()]);
    }

    /// Empty a slot's sum.
    fn clear_slot(&mut self, at: usize) {
        let Some(b) = &self.blend else { return };
        let mut enc = self.gpu.device.create_command_encoder(&Default::default());
        enc.clear_buffer(&b.slots[at].sum, 0, None);
        self.gpu.queue.submit([enc.finish()]);
    }

    /// The frame as NV12, into `out`: converted on the card, read back.
    pub(crate) fn nv12(&self, out: &mut Vec<u8>) {
        let device = &self.gpu.device;
        let words = (self.nv12_buf.size() / 4) as u32;
        let groups = words.div_ceil(64);
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("nv12") });
        {
            let mut pass = enc.begin_compute_pass(&Default::default());
            pass.set_pipeline(&self.nv12);
            pass.set_bind_group(0, &self.nv12_bind, &[]);
            // Past 65535 groups a dimension, rows of them.
            pass.dispatch_workgroups(groups.min(65535), groups.div_ceil(65535), 1);
        }
        enc.copy_buffer_to_buffer(&self.nv12_buf, 0, &self.readback, 0, self.nv12_buf.size());
        self.gpu.queue.submit([enc.finish()]);
        let slice = self.readback.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        let _ = device.poll(wgpu::PollType::wait_indefinitely());
        out.clear();
        if let Ok(data) = slice.get_mapped_range() {
            out.extend_from_slice(&data[..nv12_len(self.width, self.height)]);
        }
        self.readback.unmap();
    }

    /// The frame shrunk to fit `max`, as R, G, B, A, for the preview. Its
    /// size. Shrunk on the card (as the frame is made) and only that read
    /// back: reading the whole frame back and shrinking it on the CPU took
    /// 20 ms, a 60 fps frame's time and more.
    pub(crate) fn preview(&mut self, max: (u32, u32), out: &mut Vec<u8>) -> (u32, u32) {
        let scale = (max.0 as f64 / self.width as f64).min(max.1 as f64 / self.height as f64).min(1.0);
        let w = ((self.width as f64 * scale).round() as u32).max(1);
        let h = ((self.height as f64 * scale).round() as u32).max(1);
        let device = &self.gpu.device;
        if self.preview.as_ref().is_none_or(|p| (p.texture.width(), p.texture.height()) != (w, h)) {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("preview"),
                size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let row = (w * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
            let readback = device.create_buffer(&wgpu::BufferDescriptor { label: Some("preview read"), size: row as u64 * h as u64, usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false });
            let uniforms = device.create_buffer(&wgpu::BufferDescriptor { label: Some("preview scale"), size: 64, usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
            self.preview = Some(PreviewTarget { texture, readback, uniforms, row });
        }
        let p = self.preview.as_ref().expect("made above");
        // The whole frame into the whole preview, no webcam layer (it's in the frame).
        let bytes: Vec<u8> = [0.0f32, 0.0, self.width as f32, self.height as f32, 0.0, 0.0, w as f32, h as f32]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .chain([w, h, 0, 0].iter().flat_map(|v| v.to_le_bytes()))
            .collect();
        self.gpu.queue.write_buffer(&p.uniforms, 0, &bytes);
        let frame_view = self.frame.create_view(&Default::default());
        let preview_view = p.texture.create_view(&Default::default());
        let overlay_view = self.layer.texture.create_view(&Default::default());
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("preview"),
            layout: &self.scale.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&frame_view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&preview_view) },
                wgpu::BindGroupEntry { binding: 2, resource: p.uniforms.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(&overlay_view) },
            ],
        });
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("preview") });
        {
            let mut pass = enc.begin_compute_pass(&Default::default());
            pass.set_pipeline(&self.scale);
            pass.set_bind_group(0, &bind, &[]);
            pass.dispatch_workgroups(w.div_ceil(8), h.div_ceil(8), 1);
        }
        enc.copy_texture_to_buffer(
            p.texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo { buffer: &p.readback, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(p.row), rows_per_image: Some(h) } },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        let done = self.gpu.queue.submit([enc.finish()]);
        let slice = p.readback.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        let _ = device.poll(wgpu::PollType::Wait { submission_index: Some(done), timeout: Some(std::time::Duration::from_secs(1)) });
        out.clear();
        if let Ok(data) = slice.get_mapped_range() {
            for r in data.chunks(p.row as usize).take(h as usize) {
                out.extend_from_slice(&r[..w as usize * 4]);
            }
        }
        p.readback.unmap();
        (w, h)
    }

    /// A picture from our memory on the card: uploaded once per picture.
    fn upload(&mut self, src: &Frame, seq: u64) -> Option<wgpu::Texture> {
        if let Some((t, s)) = &self.uploaded
            && *s == seq
            && t.width() == src.width
            && t.height() == src.height
        {
            return Some(t.clone());
        }
        if src.width == 0 || src.height == 0 || src.data.len() < src.stride * (src.height as usize - 1) + src.width as usize * 4 {
            return None;
        }
        let format = match src.order {
            Order::Bgr => wgpu::TextureFormat::Bgra8Unorm,
            Order::Rgb => wgpu::TextureFormat::Rgba8Unorm,
        };
        let reuse = self.uploaded.take().map(|(t, _)| t).filter(|t| t.width() == src.width && t.height() == src.height && t.format() == format);
        let t = reuse.unwrap_or_else(|| {
            self.gpu.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("uploaded picture"),
                size: wgpu::Extent3d { width: src.width, height: src.height, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            })
        });
        self.gpu.queue.write_texture(
            t.as_image_copy(),
            &src.data,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(src.stride as u32), rows_per_image: Some(src.height) },
            wgpu::Extent3d { width: src.width, height: src.height, depth_or_array_layers: 1 },
        );
        self.uploaded = Some((t.clone(), seq));
        Some(t)
    }
}

impl Nv12Source for GpuPicture {
    fn gpu(&self) -> &Arc<Gpu> {
        &self.gpu
    }

    fn nv12_target(&self, buffer: wgpu::Buffer, pitch: u32, size: (u32, u32), picture: Option<wgpu::Texture>) -> Nv12Target {
        let device = &self.gpu.device;
        let sizes = device.create_buffer(&wgpu::BufferDescriptor { label: Some("size"), size: 16, usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
        let bytes: Vec<u8> = [size.0, size.1, pitch, 0].iter().flat_map(|v| v.to_le_bytes()).collect();
        self.gpu.queue.write_buffer(&sizes, 0, &bytes);
        let frame_view = self.frame.create_view(&Default::default());
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("nv12 out"),
            layout: &self.nv12.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&frame_view) },
                wgpu::BindGroupEntry { binding: 1, resource: buffer.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: sizes.as_entire_binding() },
            ],
        });
        Nv12Target::new(buffer, bind, pitch, size, picture)
    }

    fn nv12_to(&self, target: &Nv12Target) {
        target.fill(&self.gpu, &self.nv12);
    }
}

/// NV12's size: a full-size luma plane, then a half-size plane of U, V pairs.
pub(crate) fn nv12_len(width: u32, height: u32) -> usize {
    width as usize * height as usize * 3 / 2
}

#[cfg(test)]
mod tests {
    use super::super::image::Canvas;
    use super::*;

    /// The screen in a box of its own in front of the webcam covers it there
    /// and nowhere else; behind it, the webcam covers it. The same for a
    /// blended frame.
    #[test]
    #[ignore = "uses the graphics card: cargo test -p capture in_front_or_behind -- --ignored"]
    fn in_front_or_behind() {
        let gpu = super::super::gpu::gpu().expect("a graphics card");
        let white = Frame { width: 64, height: 36, stride: 256, order: Order::Bgr, data: [255u8, 255, 255, 255].repeat(64 * 36), texture: None };
        let cam = CameraPicture { width: 16, height: 9, bgra: [40u8, 40, 40, 255].repeat(16 * 9), seq: 1 };
        let all = Placement { x: 0.0, y: 0.0, w: 1.0, h: 1.0, crop: [0.0; 4], flip_h: false, flip_v: false, turns: 0, stretch: 1.0 };
        let left = crate::ScreenPlacement { rect: [0.0, 0.0, 0.5, 1.0], ..crate::ScreenPlacement::whole(crate::Framing::Fill) };
        // Luma of white, and of the camera's grey.
        let (screen, camera) = (235u8, 50u8);
        for blended in [false, true] {
            for (in_front, want_left) in [(true, screen), (false, camera)] {
                let mut pic = GpuPicture::new(gpu.clone(), 64, 36, crate::ScreenPlacement { in_front, ..left });
                pic.set_camera(Some((&cam, &all)));
                if blended {
                    pic.start_blending(1);
                    pic.hold(&white, 1);
                    pic.add(0, 1.0);
                    assert!(pic.finish(0));
                } else {
                    assert!(pic.draw(&white, 1));
                }
                let mut out = Vec::new();
                pic.nv12(&mut out);
                let (l, r) = (out[18 * 64 + 10], out[18 * 64 + 50]);
                assert_eq!((l, r), (want_left, camera), "in front {in_front}, blended {blended}");
            }
        }
    }

    /// The webcam drawn on the card is the CPU's (`Canvas::draw_camera`), give
    /// or take a step of rounding: boxed, turned, mirrored, cropped, partly
    /// off the frame.
    #[test]
    #[ignore = "uses the graphics card: cargo test -p capture webcam_layer -- --ignored"]
    fn webcam_layer_matches_the_cpu() {
        let gpu = super::super::gpu::gpu().expect("a graphics card");
        let (w, h) = (320u32, 180u32);
        let cam = CameraPicture { width: 160, height: 120, bgra: (0..160 * 120 * 4).map(|i| if i % 4 == 3 { 255 } else { ((i * 7919) % 251) as u8 }).collect(), seq: 1 };
        let base = Placement { x: 0.55, y: 0.5, w: 0.4, h: 0.45, crop: [0.0; 4], flip_h: false, flip_v: false, turns: 0, stretch: 1.0 };
        let cases = [
            ("boxed", base),
            ("turned", Placement { turns: 1, ..base }),
            ("turned twice, mirrored", Placement { turns: 2, flip_h: true, ..base }),
            ("upside down, cropped", Placement { flip_v: true, crop: [0.1, 0.05, 0.2, 0.0], ..base }),
            ("off the edge", Placement { x: 0.8, y: -0.2, ..base }),
            ("the whole frame", Placement { x: 0.0, y: 0.0, w: 1.0, h: 1.0, ..base }),
        ];
        for (name, place) in cases {
            let mut canvas = Canvas::new(w, h);
            canvas.draw_camera(&cam, &place);
            let mut pic = GpuPicture::new(gpu.clone(), w, h, crate::ScreenPlacement::whole(crate::Framing::Fit));
            pic.set_camera(Some((&cam, &place)));
            let got = gpu.read(&pic.layer.texture);
            // The CPU's layer is B, G, R, A; the card's R, G, B, A.
            let mut worst = 0;
            for (c, g) in canvas.bgra.chunks_exact(4).zip(got.chunks_exact(4)) {
                assert_eq!(c[3] > 127, g[3] > 127, "{name}: drawn in different places");
                if c[3] > 127 {
                    worst = worst.max(c[0].abs_diff(g[2])).max(c[1].abs_diff(g[1])).max(c[2].abs_diff(g[0]));
                }
            }
            eprintln!("{name}: off by at most {worst}");
            assert!(worst <= 1, "{name}: off by {worst}");
        }
    }

    /// What a new webcam picture costs the CPU (uploading it, drawing its box
    /// on the card), against `Canvas::draw_camera`'s 4-60 ms.
    #[test]
    #[ignore = "uses the graphics card: cargo test --release -p capture webcam_layer_time -- --ignored --nocapture"]
    fn webcam_layer_time() {
        let gpu = super::super::gpu::gpu().expect("a graphics card");
        let cpu = || {
            // SAFETY: a valid rusage to fill.
            let mut u = unsafe { std::mem::zeroed::<libc::rusage>() };
            unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
            (u.ru_utime.tv_sec + u.ru_stime.tv_sec) as f64 + (u.ru_utime.tv_usec + u.ru_stime.tv_usec) as f64 / 1e6
        };
        let mut pic = GpuPicture::new(gpu.clone(), 1920, 1080, crate::ScreenPlacement::whole(crate::Framing::Fit));
        let pictures: Vec<Arc<CameraPicture>> = (0..2).map(|k| Arc::new(CameraPicture { width: 1920, height: 1080, bgra: vec![40 + k as u8; 1920 * 1080 * 4], seq: k as u64 + 1 })).collect();
        for w in [0.25f32, 0.5, 1.0] {
            let place = Placement { x: 1.0 - w, y: 1.0 - w, w, h: w, crop: [0.0; 4], flip_h: false, flip_v: false, turns: 0, stretch: 1.0 };
            let n = 60;
            let c0 = cpu();
            for i in 0..n {
                pic.set_camera(Some((&pictures[i % 2], &place)));
            }
            let _ = gpu.device.poll(wgpu::PollType::wait_indefinitely());
            let c = (cpu() - c0) * 1000.0 / n as f64;
            eprintln!("a {:.0}%-wide box: {c:.2} ms CPU a picture ({:.0}% of a core at 30 fps)", w * 100.0, c * 3.0);
        }
    }

    /// Blending black and white evenly averages their light (sRGB 188, luma
    /// 178), not their values (128); a frame no look saw isn't made; two
    /// frames open at once keep their sums apart.
    #[test]
    #[ignore = "uses the graphics card: cargo test -p capture blends_light -- --ignored"]
    fn blends_light() {
        let gpu = super::super::gpu::gpu().expect("a graphics card");
        let solid = |v: u8| Frame { width: 16, height: 16, stride: 64, order: Order::Bgr, data: [v, v, v, 255].repeat(256), texture: None };
        let mut pic = GpuPicture::new(gpu, 16, 16, crate::ScreenPlacement::whole(crate::Framing::Fit));
        pic.start_blending(2);
        assert!(!pic.finish(0), "no look, no frame");
        pic.hold(&solid(0), 1);
        pic.add(1, 1.0);
        pic.add(2, 3.0);
        pic.hold(&solid(255), 2);
        pic.add(1, 1.0);
        assert!(pic.finish(1));
        let mut out = Vec::new();
        pic.nv12(&mut out);
        eprintln!("frame 1 luma {}", out[0]);
        assert!((out[0] as i32 - 178).abs() <= 1, "luma {}", out[0]);
        // Frame 2: three parts black, one white: a quarter of the light.
        pic.add(2, 1.0);
        assert!(pic.finish(2));
        pic.nv12(&mut out);
        // A quarter of white's light is sRGB 137: luma ((220 * 137 + 128) >> 8) + 16 = 134.
        eprintln!("frame 2 luma {}", out[0]);
        assert!((out[0] as i32 - 134).abs() <= 1, "luma {}", out[0]);
    }

    /// How long a preview frame takes (drawing it, NV12, and the preview).
    #[test]
    #[ignore = "uses the graphics card: cargo test -p capture preview_time -- --ignored"]
    fn preview_time() {
        let gpu = super::super::gpu::gpu().expect("a graphics card");
        let (sw, sh) = (2560u32, 1440u32);
        let frame = Frame { width: sw, height: sh, stride: sw as usize * 4, order: Order::Bgr, data: vec![90; (sw * sh * 4) as usize], texture: None };
        let mut pic = GpuPicture::new(gpu, 1920, 1080, crate::ScreenPlacement::whole(crate::Framing::Fit));
        let mut out = Vec::new();
        let mut nv12 = Vec::new();
        for i in 0..3 {
            pic.draw(&frame, i);
            pic.preview((1280, 720), &mut out);
        }
        let n = 30;
        let t = std::time::Instant::now();
        for i in 0..n {
            pic.draw(&frame, 10 + i);
            pic.nv12(&mut nv12);
        }
        let frame_ms = t.elapsed().as_secs_f64() * 1000.0 / n as f64;
        let t = std::time::Instant::now();
        for _ in 0..n {
            pic.preview((1280, 720), &mut out);
        }
        eprintln!("frame + nv12: {frame_ms:.2} ms; preview: {:.2} ms", t.elapsed().as_secs_f64() * 1000.0 / n as f64);
    }

    /// The card makes the same NV12 as the CPU, give or take a step from
    /// rounding when scaling.
    #[test]
    #[ignore = "uses the graphics card: cargo test -p capture same_frame_as_the_cpu -- --ignored"]
    fn same_frame_as_the_cpu() {
        let gpu = super::super::gpu::gpu().expect("a graphics card");
        let whole = crate::ScreenPlacement::whole;
        let boxed = crate::ScreenPlacement { rect: [0.5, 0.25, 0.375, 0.5], ..whole(crate::Framing::Fill) };
        for (sw, sh, dw, dh, place) in [(64, 36, 64, 36, whole(crate::Framing::Fit)), (2560, 1440, 1920, 1080, whole(crate::Framing::Fit)), (300, 200, 128, 128, whole(crate::Framing::Fill)), (40, 30, 64, 48, whole(crate::Framing::Fit)), (2560, 1440, 1920, 1080, boxed)] {
            let data: Vec<u8> = (0..sw * sh * 4).map(|i| ((i * 7919) % 251) as u8).collect();
            let frame = Frame { width: sw, height: sh, stride: sw as usize * 4, order: Order::Bgr, data, texture: None };
            let mut cpu = Canvas::new(dw, dh);
            cpu.draw_screen(&frame, &place, false);
            let mut want = Vec::new();
            cpu.to_nv12(&mut want);
            let mut pic = GpuPicture::new(gpu.clone(), dw, dh, place);
            assert!(pic.draw(&frame, 1));
            let mut got = Vec::new();
            pic.nv12(&mut got);
            assert_eq!(got.len(), want.len());
            let worst = got.iter().zip(&want).map(|(a, b)| (*a as i32 - *b as i32).abs()).max().unwrap();
            let off = got.iter().zip(&want).filter(|(a, b)| a != b).count();
            eprintln!("{sw}x{sh} → {dw}x{dh}: {off} of {} bytes differ, by at most {worst}", got.len());
            assert!(worst <= 2, "off by {worst}");
        }
    }
}
