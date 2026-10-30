//! The recording's frame made on the graphics card (`super::gpu`): the
//! picture put into its box in the frame (`crate::ScreenPlacement`), the
//! webcam over it or under it, and NV12 for the encoder, in compute shaders;
//! only the NV12 frame comes down to the CPU (an eighth of the screen's
//! pixels at 1440p → 1080p).
//! The same as `super::image` does on the CPU, which this replaces when the
//! screen's pictures are on the card.
//!
//! A picture that arrives in our memory (the away screen, X11) is uploaded
//! first. The webcam is drawn by the CPU's code (`Canvas::draw_camera`, only
//! its box, only when it changes) into a layer laid over the frame here.

use std::sync::Arc;

use super::gpu::Gpu;
use super::image::{CameraPicture, Canvas};
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

/// Where an encoder takes frames from on the card ([`GpuPicture::nv12_target`]).
pub(crate) struct Nv12Target {
    buffer: wgpu::Buffer,
    bind: wgpu::BindGroup,
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

/// The webcam over the frame: drawn into a frame-size canvas of ours (clear
/// where it isn't), uploaded where it changed.
struct Layer {
    canvas: Canvas,
    texture: wgpu::Texture,
    /// The rows last drawn on, to clear and upload.
    rows: Option<(usize, usize)>,
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
        let overlay = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("webcam layer"),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Bgra8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
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
        let layer = Layer { canvas: Canvas::new(width, height), texture: overlay, rows: None, on: false };
        Self { gpu, width, height, screen, scale, nv12, frame, uniforms, nv12_buf, readback, layer, uploaded: None, nv12_bind, blend: None, preview: None }
    }

    /// Where the picture goes from now on.
    pub(crate) fn set_screen(&mut self, screen: crate::ScreenPlacement) {
        self.screen = screen;
    }

    /// Draw the webcam's layer: `cam` placed at `place`, or none.
    pub(crate) fn set_camera(&mut self, cam: Option<(&CameraPicture, &Placement)>) {
        let l = &mut self.layer;
        let w = l.canvas.width as usize;
        let row = w * 4;
        // Clear what was drawn before.
        if let Some((top, bottom)) = l.rows {
            l.canvas.bgra[top * row..bottom * row].fill(0);
        }
        let drawn = cam.filter(|(_, p)| !p.is_hidden()).and_then(|(c, p)| {
            l.canvas.draw_camera(c, p);
            let (fh, ph) = (l.canvas.height as f32, p.h * l.canvas.height as f32);
            let top = (p.y * fh).max(0.0).floor() as usize;
            let bottom = ((p.y * fh + ph).min(fh).ceil() as usize).min(l.canvas.height as usize);
            (top < bottom).then_some((top, bottom))
        });
        // Upload the rows drawn now and before (cleared).
        let span = match (l.rows, drawn) {
            (Some(a), Some(b)) => Some((a.0.min(b.0), a.1.max(b.1))),
            (a, b) => a.or(b),
        };
        if let Some((top, bottom)) = span {
            self.gpu.queue.write_texture(
                wgpu::TexelCopyTextureInfo { texture: &l.texture, mip_level: 0, origin: wgpu::Origin3d { x: 0, y: top as u32, z: 0 }, aspect: wgpu::TextureAspect::All },
                &l.canvas.bgra[top * row..bottom * row],
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row as u32), rows_per_image: Some((bottom - top) as u32) },
                wgpu::Extent3d { width: w as u32, height: (bottom - top) as u32, depth_or_array_layers: 1 },
            );
        }
        l.rows = drawn;
        l.on = drawn.is_some();
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
    pub(crate) fn start_blending(&mut self, open: usize) {
        let device = &self.gpu.device;
        let pipeline = |source: &str, entry: &str| {
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some(entry), source: wgpu::ShaderSource::Wgsl(source.into()) });
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor { label: Some(entry), layout: None, module: &module, entry_point: Some(entry), compilation_options: Default::default(), cache: None })
        };
        let uniform = |label| device.create_buffer(&wgpu::BufferDescriptor { label: Some(label), size: 64, usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
        let bytes = self.width as u64 * self.height as u64 * 16;
        let slots = (0..open.max(1))
            .map(|_| Slot {
                sum: device.create_buffer(&wgpu::BufferDescriptor { label: Some("blend sum"), size: bytes, usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false }),
                uniforms: uniform("look"),
                frame: None,
                added: 0.0,
                pending: 0.0,
            })
            .collect();
        self.blend = Some(Blend { accumulate: pipeline(BLEND_SHADER, "accumulate"), resolve: pipeline(RESOLVE_SHADER, "resolve"), slots, resolve_uniforms: uniform("resolve"), held: None, dst: [0; 4] });
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

    /// A place an encoder takes frames from on the card (`super::nvenc`):
    /// `buffer`, NV12 with rows `pitch` bytes apart, made by [`Self::nv12_to`].
    pub(crate) fn nv12_target(&self, buffer: wgpu::Buffer, pitch: u32) -> Nv12Target {
        let device = &self.gpu.device;
        let sizes = device.create_buffer(&wgpu::BufferDescriptor { label: Some("size"), size: 16, usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
        let bytes: Vec<u8> = [self.width, self.height, pitch, 0].iter().flat_map(|v| v.to_le_bytes()).collect();
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
        Nv12Target { buffer, bind }
    }

    /// The frame as NV12 into `target`, on the card: done when this returns.
    pub(crate) fn nv12_to(&self, target: &Nv12Target) {
        let device = &self.gpu.device;
        let groups = ((target.buffer.size() / 4) as u32).div_ceil(64);
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("nv12 out") });
        {
            let mut pass = enc.begin_compute_pass(&Default::default());
            pass.set_pipeline(&self.nv12);
            pass.set_bind_group(0, &target.bind, &[]);
            pass.dispatch_workgroups(groups.min(65535), groups.div_ceil(65535), 1);
        }
        let index = self.gpu.queue.submit([enc.finish()]);
        let _ = device.poll(wgpu::PollType::Wait { submission_index: Some(index), timeout: None });
    }

    /// The card it's made on.
    pub(crate) fn gpu(&self) -> &Arc<Gpu> {
        &self.gpu
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

/// NV12's size: a full-size luma plane, then a half-size plane of U, V pairs.
pub(crate) fn nv12_len(width: u32, height: u32) -> usize {
    width as usize * height as usize * 3 / 2
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The screen in a box of its own in front of the webcam covers it there
    /// and nowhere else; behind it, the webcam covers it. The same for a
    /// blended frame.
    #[test]
    #[ignore = "uses the graphics card: cargo test -p capture in_front_or_behind -- --ignored"]
    fn in_front_or_behind() {
        let gpu = super::super::gpu::gpu().expect("a graphics card");
        let white = Frame { width: 64, height: 36, stride: 256, order: Order::Bgr, data: [255u8, 255, 255, 255].repeat(64 * 36), texture: None };
        let cam = CameraPicture { width: 16, height: 9, bgra: [40u8, 40, 40, 255].repeat(16 * 9) };
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
