//! Decoded frames drawn straight from the GPU: nothing is copied back to the
//! CPU or uploaded again.
//!
//! - **Windows**: the decoder renders each frame, at the video's own
//!   resolution, into one of a pool of shareable D3D11 RGBA textures
//!   (`capture::decode::Surfaces`); each is opened once in wgpu's D3D12
//!   device (`dx12`).
//! - **macOS**: the decoder's own NV12 frames, in IOSurfaces; each plane is
//!   opened as a Metal texture over the same memory (`metal`), and the shader
//!   converts to RGB as it filters.
//!
//! Frames are drawn by our own shader ([`Draw`]) with a Lanczos-3 filter
//! whose kernel widens with the reduction: a picture shown smaller than the
//! video is resampled from every source pixel it covers, as sharp as ffmpeg's
//! default scaler, with no shimmer; at 1:1 every pixel is copied untouched;
//! shown larger, it's Lanczos-upscaled. (egui's own drawing samples
//! bilinearly, which turns a 2560-wide frame drawn 1686 wide soft.) From
//! NV12 the filter runs on each plane at its own resolution, chroma at its
//! own sample positions (H.264's: left-aligned, between rows), then converts:
//! the same as converting first, since the filter and the conversion are
//! both linear, at a quarter of the cost for the colour.
//!
//! Needs wgpu on the platform's backend (DX12 is asked for at startup on
//! Windows; macOS only has Metal) and, on Windows, the decoder on the same
//! graphics card; otherwise frames come back as RGBA as before.

use std::collections::HashMap;
use std::sync::OnceLock;

use eframe::egui_wgpu::{self, CallbackResources, CallbackTrait, RenderState, ScreenDescriptor};
use eframe::wgpu;

#[cfg(windows)]
mod dx12;
#[cfg(windows)]
pub use dx12::{Frames, share_luid};
#[cfg(target_os = "macos")]
mod metal;
#[cfg(target_os = "macos")]
pub use metal::{Frames, available};

static RENDER: OnceLock<RenderState> = OnceLock::new();

/// Remember the app's renderer, at startup.
pub fn init(state: Option<&RenderState>) {
    if let Some(s) = state {
        let _ = RENDER.set(s.clone());
    }
}

/// A decoded frame ready to draw: its texture(s), opened once per decoder
/// buffer (`key`).
#[derive(Clone)]
pub struct Shown {
    key: (usize, usize),
    planes: Planes,
}

#[derive(Clone)]
enum Planes {
    #[cfg_attr(not(windows), allow(dead_code))]
    Rgba(wgpu::TextureView),
    /// Studio-range Y and CbCr, and the matrix (BT.601, else BT.709).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Nv12 { y: wgpu::TextureView, cbcr: wgpu::TextureView, bt601: bool },
}

impl Shown {
    /// Draw it into `rect` (with the high-quality filter).
    pub fn paint(&self, ui: &egui::Ui, rect: egui::Rect) {
        ui.painter().add(egui_wgpu::Callback::new_paint_callback(rect, Draw(self.clone())));
    }
}

/// Forget a player's bind groups (its frames' pools) so their textures are
/// freed now, not when the next clip draws. Nothing is destroyed outright: a
/// frame drawn this pass makes its own again if it still needs one.
fn forget_pools(pools: &[usize]) {
    if let Some(rs) = RENDER.get() {
        if let Some(scaler) = rs.renderer.write().callback_resources.get_mut::<Scaler>() {
            scaler.groups.retain(|(p, _), _| !pools.contains(p));
        }
    }
}

/// The filter's pipelines and a bind group per frame texture, kept in egui's
/// callback resources. Bind groups not drawn for a while are dropped (they
/// hold their textures: a closed clip's frames are freed with them).
struct Scaler {
    rgba: Pipeline,
    /// NV12 with BT.709 colours, and with BT.601.
    nv12: [Pipeline; 2],
    groups: HashMap<(usize, usize), (wgpu::BindGroup, std::time::Instant)>,
}

struct Pipeline {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
}

/// Draws a frame with the filter, as an egui paint callback.
struct Draw(Shown);

impl CallbackTrait for Draw {
    fn prepare(
        &self,
        device: &wgpu::Device,
        _queue: &wgpu::Queue,
        _screen: &ScreenDescriptor,
        _encoder: &mut wgpu::CommandEncoder,
        resources: &mut CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        if resources.get::<Scaler>().is_none() {
            let Some(rs) = RENDER.get() else { return Vec::new() };
            resources.insert(Scaler::new(device, rs.target_format));
        }
        let scaler = resources.get_mut::<Scaler>().expect("just made");
        let now = std::time::Instant::now();
        scaler.groups.retain(|_, (_, used)| now.duration_since(*used).as_secs() < 2);
        if let Some((_, used)) = scaler.groups.get_mut(&self.0.key) {
            *used = now;
        } else {
            let group = match &self.0.planes {
                Planes::Rgba(view) => device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("decoded frame"),
                    layout: &scaler.rgba.layout,
                    entries: &[wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(view) }],
                }),
                Planes::Nv12 { y, cbcr, bt601 } => device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("decoded frame"),
                    layout: &scaler.nv12[usize::from(*bt601)].layout,
                    entries: &[
                        wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(y) },
                        wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(cbcr) },
                    ],
                }),
            };
            scaler.groups.insert(self.0.key, (group, now));
        }
        Vec::new()
    }

    fn paint(&self, _info: egui::PaintCallbackInfo, pass: &mut wgpu::RenderPass<'static>, resources: &CallbackResources) {
        let Some(scaler) = resources.get::<Scaler>() else { return };
        let Some((group, _)) = scaler.groups.get(&self.0.key) else { return };
        match &self.0.planes {
            Planes::Rgba(_) => pass.set_pipeline(&scaler.rgba.pipeline),
            Planes::Nv12 { bt601, .. } => pass.set_pipeline(&scaler.nv12[usize::from(*bt601)].pipeline),
        }
        pass.set_bind_group(0, group, &[]);
        pass.draw(0..3, 0..1);
    }
}

impl Scaler {
    fn new(device: &wgpu::Device, target: wgpu::TextureFormat) -> Self {
        let srgb = if target.is_srgb() { "true" } else { "false" };
        let rgba = Pipeline::new(device, target, &format!("{COMMON}{RGBA}").replace("SRGB_TARGET", srgb), 1);
        let nv12 = ["false", "true"].map(|bt601| Pipeline::new(device, target, &format!("{COMMON}{NV12}").replace("SRGB_TARGET", srgb).replace("BT601", bt601), 2));
        Self { rgba, nv12, groups: HashMap::new() }
    }
}

impl Pipeline {
    /// A pipeline drawing with `source`, which reads `textures` unfiltered textures.
    fn new(device: &wgpu::Device, target: wgpu::TextureFormat, source: &str, textures: u32) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("frame filter"), source: wgpu::ShaderSource::Wgsl(source.into()) });
        let entries: Vec<_> = (0..textures)
            .map(|binding| wgpu::BindGroupLayoutEntry {
                binding,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            })
            .collect();
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: Some("frame filter"), entries: &entries });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("frame filter"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("frame filter"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState { module: &shader, entry_point: Some("vs"), compilation_options: Default::default(), buffers: &[] },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState { format: target, blend: None, write_mask: wgpu::ColorWrites::ALL })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        Self { pipeline, layout }
    }
}

/// Lanczos-3 resampling. One full-viewport triangle (egui sets the viewport
/// to the callback's rect); each output pixel weighs every source pixel within
/// 3 lobes, the lobes stretched by the reduction so a smaller picture is
/// filtered rather than skipped through.
const COMMON: &str = r#"
struct Out {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) i: u32) -> Out {
    let x = f32((i << 1u) & 2u);
    let y = f32(i & 2u);
    var o: Out;
    o.pos = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    o.uv = vec2<f32>(x, y);
    return o;
}

const PI: f32 = 3.14159265;

fn lanczos3(x: f32) -> f32 {
    let ax = abs(x);
    if ax < 1e-5 { return 1.0; }
    if ax >= 3.0 { return 0.0; }
    let px = PI * x;
    return 3.0 * sin(px) * sin(px / 3.0) / (px * px);
}

fn to_linear(c: vec3<f32>) -> vec3<f32> {
    return select(pow((c + 0.055) / 1.055, vec3<f32>(2.4)), c / 12.92, c <= vec3<f32>(0.04045));
}

fn finish(rgb: vec3<f32>) -> vec4<f32> {
    var c = clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0));
    if SRGB_TARGET { c = to_linear(c); }
    return vec4<f32>(c, 1.0);
}

/// The filter's window over one plane: `src` is the output pixel's centre
/// in the plane's pixels, `s` source pixels per screen pixel (at least 1).
struct Window {
    lo: vec2<i32>,
    n: vec2<i32>,
    wx: array<f32, 25>,
    wy: array<f32, 25>,
    total: f32,
};

fn window(src: vec2<f32>, s: vec2<f32>) -> Window {
    var w: Window;
    let r = min(3.0 * s, vec2<f32>(12.0));
    w.lo = vec2<i32>(floor(src - r)) + vec2<i32>(1);
    w.n = min(vec2<i32>(floor(src + r)) - w.lo + vec2<i32>(1), vec2<i32>(25));
    // Weights per column and per row once (the filter is separable), then
    // every source pixel in the window is just a load and a multiply.
    var tx = 0.0;
    var ty = 0.0;
    for (var i = 0; i < w.n.x; i++) {
        w.wx[i] = lanczos3((f32(w.lo.x + i) - src.x) / s.x);
        tx += w.wx[i];
    }
    for (var j = 0; j < w.n.y; j++) {
        w.wy[j] = lanczos3((f32(w.lo.y + j) - src.y) / s.y);
        ty += w.wy[j];
    }
    w.total = tx * ty;
    return w;
}
"#;

const RGBA: &str = r#"
@group(0) @binding(0) var frame: texture_2d<f32>;

@fragment
fn fs(in: Out) -> @location(0) vec4<f32> {
    let size = vec2<f32>(textureDimensions(frame));
    let last = vec2<i32>(textureDimensions(frame)) - vec2<i32>(1);
    // Source pixels per screen pixel: above 1 when shown smaller.
    let step = vec2<f32>(abs(dpdx(in.uv.x)), abs(dpdy(in.uv.y))) * size;
    let w = window(in.uv * size - 0.5, max(step, vec2<f32>(1.0)));
    var sum = vec3<f32>(0.0);
    for (var j = 0; j < w.n.y; j++) {
        let y = clamp(w.lo.y + j, 0, last.y);
        var row = vec3<f32>(0.0);
        for (var i = 0; i < w.n.x; i++) {
            row += textureLoad(frame, vec2<i32>(clamp(w.lo.x + i, 0, last.x), y), 0).rgb * w.wx[i];
        }
        sum += row * w.wy[j];
    }
    return finish(sum / w.total);
}
"#;

const NV12: &str = r#"
@group(0) @binding(0) var luma: texture_2d<f32>;
@group(0) @binding(1) var chroma: texture_2d<f32>;

@fragment
fn fs(in: Out) -> @location(0) vec4<f32> {
    let size = vec2<f32>(textureDimensions(luma));
    let step = vec2<f32>(abs(dpdx(in.uv.x)), abs(dpdy(in.uv.y))) * size;

    // Luma at full resolution.
    let last_y = vec2<i32>(textureDimensions(luma)) - vec2<i32>(1);
    let wy = window(in.uv * size - 0.5, max(step, vec2<f32>(1.0)));
    var y = 0.0;
    for (var j = 0; j < wy.n.y; j++) {
        let py = clamp(wy.lo.y + j, 0, last_y.y);
        var row = 0.0;
        for (var i = 0; i < wy.n.x; i++) {
            row += textureLoad(luma, vec2<i32>(clamp(wy.lo.x + i, 0, last_y.x), py), 0).r * wy.wx[i];
        }
        y += row * wy.wy[j];
    }
    y /= wy.total;

    // Chroma at half: its samples sit on even luma columns, between rows
    // (H.264's default siting), so luma position p is chroma (p.x / 2,
    // p.y / 2 - 0.25) in chroma pixels from the first sample.
    let csize = vec2<f32>(textureDimensions(chroma));
    let last_c = vec2<i32>(textureDimensions(chroma)) - vec2<i32>(1);
    let p = in.uv * size - 0.5;
    let csrc = vec2<f32>(p.x * 0.5, p.y * 0.5 - 0.25);
    let wc = window(csrc, max(step * 0.5, vec2<f32>(1.0)));
    var c = vec2<f32>(0.0);
    for (var j = 0; j < wc.n.y; j++) {
        let py = clamp(wc.lo.y + j, 0, last_c.y);
        var row = vec2<f32>(0.0);
        for (var i = 0; i < wc.n.x; i++) {
            row += textureLoad(chroma, vec2<i32>(clamp(wc.lo.x + i, 0, last_c.x), py), 0).rg * wc.wx[i];
        }
        c += row * wc.wy[j];
    }
    c /= wc.total;

    // Studio range (16-235 luma, 16-240 chroma) to full-range RGB.
    let yy = (y * 255.0 - 16.0) / 219.0;
    let cb = (c.x * 255.0 - 128.0) / 224.0;
    let cr = (c.y * 255.0 - 128.0) / 224.0;
    var rgb: vec3<f32>;
    if BT601 {
        rgb = vec3<f32>(yy + 1.402 * cr, yy - 0.344136 * cb - 0.714136 * cr, yy + 1.772 * cb);
    } else {
        rgb = vec3<f32>(yy + 1.5748 * cr, yy - 0.187324 * cb - 0.468124 * cr, yy + 1.8556 * cb);
    }
    return finish(rgb);
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// Both filters compile for every backend (naga validates them here; the
    /// app would only find out when a clip opens).
    #[test]
    fn shaders_validate() {
        for (src, bt601) in [(RGBA, "false"), (NV12, "false"), (NV12, "true")] {
            let code = format!("{COMMON}{src}").replace("SRGB_TARGET", "false").replace("BT601", bt601);
            let module = naga::front::wgsl::parse_str(&code).unwrap_or_else(|e| panic!("{}", e.emit_to_string(&code)));
            naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::all())
                .validate(&module)
                .unwrap_or_else(|e| panic!("{e:?}"));
        }
    }

    /// A decoded NV12 frame drawn by the real pipeline matches ffmpeg's
    /// conversion of the same frame: at 1:1 and shrunk (Lanczos both).
    /// `HESTECLIPS_TEST_CLIP=<mp4> cargo test -p hesteclips nv12 -- --ignored --nocapture`
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore]
    fn nv12_matches_ffmpeg() {
        use capture::decode::{Decoder, Output};
        let clip = std::path::PathBuf::from(std::env::var("HESTECLIPS_TEST_CLIP").expect("HESTECLIPS_TEST_CLIP"));
        let frame: u64 = 600;
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor { backends: wgpu::Backends::METAL, ..wgpu::InstanceDescriptor::new_without_display_handle() });
        let adapter = pollster::block_on(instance.request_adapter(&Default::default())).expect("adapter");
        let (device, queue) = pollster::block_on(adapter.request_device(&Default::default())).expect("device");
        let mut dec = Decoder::open(&clip, Output::Screen).expect("decoder");
        dec.set_fps(60.0);
        let pic = dec.frame(frame).unwrap().unwrap();
        let surface = pic.gpu.as_ref().unwrap().io_surface().unwrap();
        let (w, h) = (pic.width, pic.height);
        let y = unsafe { metal::open(&device, &surface, 0, wgpu::TextureFormat::R8Unorm, w, h) }.unwrap();
        let c = unsafe { metal::open(&device, &surface, 1, wgpu::TextureFormat::Rg8Unorm, w.div_ceil(2), h.div_ceil(2)) }.unwrap();
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let pipe = Pipeline::new(&device, format, &format!("{COMMON}{NV12}").replace("SRGB_TARGET", "false").replace("BT601", "false"), 2);
        let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &pipe.layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&y.create_view(&Default::default())) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&c.create_view(&Default::default())) },
            ],
        });
        for out_w in [w, 1280] {
            let out_h = ((out_w as f64 * h as f64 / w as f64 / 2.0).round() as u32) * 2;
            let target = device.create_texture(&wgpu::TextureDescriptor {
                label: None,
                size: wgpu::Extent3d { width: out_w, height: out_h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let row = (out_w * 4).div_ceil(256) * 256;
            let buf = device.create_buffer(&wgpu::BufferDescriptor { label: None, size: (row * out_h) as u64, usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false });
            let mut enc = device.create_command_encoder(&Default::default());
            {
                let view = target.create_view(&Default::default());
                let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: None,
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment { view: &view, depth_slice: None, resolve_target: None, ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store } })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_pipeline(&pipe.pipeline);
                pass.set_bind_group(0, &group, &[]);
                pass.draw(0..3, 0..1);
            }
            enc.copy_texture_to_buffer(
                target.as_image_copy(),
                wgpu::TexelCopyBufferInfo { buffer: &buf, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: None } },
                target.size(),
            );
            queue.submit([enc.finish()]);
            buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
            device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
            let mapped = buf.slice(..).get_mapped_range().unwrap();
            let ours: Vec<u8> = (0..out_h as usize).flat_map(|r| mapped[r * row as usize..r * row as usize + out_w as usize * 4].to_vec()).collect();
            drop(mapped);
            buf.unmap();
            let ff = media::ffmpeg()
                .args(["-v", "error", "-i"])
                .arg(&clip)
                .args(["-vf", &format!("select=eq(n\\,{frame}),scale={out_w}:{out_h}:flags=lanczos+accurate_rnd+full_chroma_int:in_color_matrix=bt709:out_range=full,format=rgba"), "-frames:v", "1", "-f", "rawvideo", "-"])
                .output()
                .unwrap()
                .stdout;
            assert_eq!(ff.len(), ours.len());
            let diffs: Vec<u32> = ours.iter().zip(&ff).enumerate().filter(|(i, _)| i % 4 != 3).map(|(_, (a, b))| (*a as i32 - *b as i32).unsigned_abs()).collect();
            let mean = diffs.iter().sum::<u32>() as f64 / diffs.len() as f64;
            let over = diffs.iter().filter(|&&d| d > 6).count() as f64 / diffs.len() as f64;
            let bias: Vec<f64> = (0..3).map(|c| ours.iter().skip(c).step_by(4).zip(ff.iter().skip(c).step_by(4)).map(|(a, b)| *a as f64 - *b as f64).sum::<f64>() / (ours.len() / 4) as f64).collect();
            println!("{out_w}x{out_h}: mean |diff| {mean:.2}, over 6 levels {:.3}%, bias r/g/b {:.2} {:.2} {:.2}", over * 100.0, bias[0], bias[1], bias[2]);
            assert!(mean < 1.5, "mean difference {mean}");
            assert!(bias.iter().all(|b| b.abs() < 0.75), "colour shifted: {bias:?}");
        }
    }
}
