//! Decoded frames drawn straight from the GPU: nothing is copied back to the
//! CPU or uploaded again.
//!
//! - **Windows**: the decoder copies each frame, at the video's own
//!   resolution and still NV12, into one of a pool of shareable D3D11
//!   textures (`capture::decode::Surfaces`; converted to RGBA instead where
//!   the graphics card can't share NV12); each is opened once in wgpu's
//!   D3D12 device (`dx12`), an NV12 one as a texture per plane.
//! - **macOS**: the decoder's own NV12 frames, in IOSurfaces; each plane is
//!   opened as a Metal texture over the same memory (`metal`), and the shader
//!   converts to RGB as it filters.
//!
//! Frames are drawn by our own shaders ([`Draw`]) with a Lanczos-3 filter
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
//! The filter is separable, and runs as two passes: each source row filtered
//! across into a picture as wide as the screen's ([`Rows`], 16-bit float),
//! then its columns filtered down onto the screen. A pass reads the n pixels
//! of its window, not n² as one pass does: 2560 frames drawn 1000 wide read
//! ~16 source pixels per screen pixel instead of ~110 (plus the colour's).
//!
//! Needs wgpu on the platform's backend (DX12 is asked for at startup on
//! Windows; macOS only has Metal) and, on Windows, the decoder on the same
//! graphics card; otherwise frames come back as RGBA as before.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Instant;

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

/// Remember the app's renderer, at startup, and get the frame filter ready
/// in the background: DirectX takes a while to compile its shaders, and done
/// on the first draw that kept a clip's first picture black that long (the
/// sound playing already).
pub fn init(state: Option<&RenderState>, ctx: &egui::Context) {
    let Some(s) = state else { return };
    let _ = RENDER.set(s.clone());
    let (s, ctx) = (s.clone(), ctx.clone());
    std::thread::Builder::new()
        .name("frame filter".into())
        .spawn(move || {
            let started = std::time::Instant::now();
            let scaler = Scaler::new(&s.device, s.target_format);
            s.renderer.write().callback_resources.insert(scaler);
            // A picture waiting for it is drawn now.
            ctx.request_repaint();
            if std::env::var_os("HESTECLIPS_DEBUG_VIDEO").is_some() {
                eprintln!("frame filter ready in {} ms", started.elapsed().as_millis());
            }
        })
        .expect("spawn frame filter thread");
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
    Nv12 { y: wgpu::TextureView, cbcr: wgpu::TextureView, bt601: bool },
}

/// What kind of picture: which shaders draw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Kind {
    Rgba,
    Nv12 { bt601: bool },
}

impl Planes {
    fn kind(&self) -> Kind {
        match self {
            Planes::Rgba(_) => Kind::Rgba,
            Planes::Nv12 { bt601, .. } => Kind::Nv12 { bt601: *bt601 },
        }
    }

    fn views(&self) -> Vec<&wgpu::TextureView> {
        match self {
            Planes::Rgba(v) => vec![v],
            Planes::Nv12 { y, cbcr, .. } => vec![y, cbcr],
        }
    }
}

impl Shown {
    /// Draw it into `rect` (with the high-quality filter).
    pub fn paint(&self, ui: &egui::Ui, rect: egui::Rect) {
        ui.painter().add(egui_wgpu::Callback::new_paint_callback(rect, Draw(self.clone(), rect)));
    }

    /// Where pass 1's output goes for drawing it at `place` (screen pixels:
    /// left, top, width, height).
    fn rows_key(&self, place: [i32; 4]) -> RowsKey {
        let mut planes = [(0, 0); 2];
        for (p, v) in planes.iter_mut().zip(self.planes.views()) {
            let s = v.texture().size();
            *p = (s.width, s.height);
        }
        RowsKey { place, planes, kind: self.planes.kind() }
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

/// The filter's pipelines, kept in egui's callback resources, with what
/// they draw from and into. Bind groups and pass-1 pictures not used for a
/// while are dropped (bind groups hold their textures: a closed clip's
/// frames are freed with them).
struct Scaler {
    /// One texture to read; two (NV12's planes).
    one: wgpu::BindGroupLayout,
    two: wgpu::BindGroupLayout,
    /// Pass 1 (across), into a [`Rows`] picture: from RGBA, from luma (both
    /// sampled from a pixel's centre) and from chroma (sampled from its left
    /// edge, H.264's siting).
    across_rgba: wgpu::RenderPipeline,
    across_luma: wgpu::RenderPipeline,
    across_chroma: wgpu::RenderPipeline,
    /// Pass 2 (down, onto the screen): RGBA, and NV12 with BT.709 colours
    /// and with BT.601, converted as they're drawn.
    down_rgba: wgpu::RenderPipeline,
    down_nv12: [wgpu::RenderPipeline; 2],
    /// Pass 1's input: per frame texture, a bind group per plane.
    groups: HashMap<(usize, usize), (Vec<wgpu::BindGroup>, Instant)>,
    /// Pass 1's output, per place on screen it's drawn to.
    rows: HashMap<RowsKey, (Rows, Instant)>,
}

/// Which [`Rows`] picture: where on the screen (so two pictures drawn at
/// once don't share one), the planes' sizes and the kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct RowsKey {
    place: [i32; 4],
    planes: [(u32, u32); 2],
    kind: Kind,
}

/// A picture's rows filtered across to the screen's width: a texture per
/// plane, as wide as it's drawn and as tall as the plane, and pass 2's bind
/// group reading them.
struct Rows {
    views: Vec<wgpu::TextureView>,
    group: wgpu::BindGroup,
}

/// Draws a frame with the filter, as an egui paint callback.
struct Draw(Shown, egui::Rect);

impl CallbackTrait for Draw {
    fn prepare(
        &self,
        device: &wgpu::Device,
        _queue: &wgpu::Queue,
        screen: &ScreenDescriptor,
        encoder: &mut wgpu::CommandEncoder,
        resources: &mut CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        // Still being made (right after launch): no picture yet, rather than
        // the window stuck compiling it here.
        let Some(scaler) = resources.get_mut::<Scaler>() else { return Vec::new() };
        // Where egui puts it (rounded to pixels, kept on the screen).
        let place = egui::epaint::ViewportInPixels::from_points(&self.1, screen.pixels_per_point, screen.size_in_pixels);
        scaler.across(device, encoder, &self.0, [place.left_px, place.top_px, place.width_px, place.height_px]);
        Vec::new()
    }

    fn paint(&self, info: egui::PaintCallbackInfo, pass: &mut wgpu::RenderPass<'static>, resources: &CallbackResources) {
        let Some(scaler) = resources.get::<Scaler>() else { return };
        let place = info.viewport_in_pixels();
        scaler.down(pass, &self.0, [place.left_px, place.top_px, place.width_px, place.height_px]);
    }
}

impl Scaler {
    /// Compiles its shaders side by side: DirectX's compiler is slow.
    fn new(device: &wgpu::Device, target: wgpu::TextureFormat) -> Self {
        let layout = |textures: u32| {
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
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: Some("frame filter"), entries: &entries })
        };
        let (one, two) = (layout(1), layout(2));
        let srgb = if target.is_srgb() { "true" } else { "false" };
        let across = |offset: &str| format!("{COMMON}{ACROSS}").replace("OFFSET", offset);
        let down_nv12 = |bt601: &str| format!("{COMMON}{DOWN}{DOWN_NV12}").replace("SRGB_TARGET", srgb).replace("BT601", bt601);
        let down_rgba = format!("{COMMON}{DOWN}{DOWN_RGBA}").replace("SRGB_TARGET", srgb);
        let f16 = |c: u32| match c {
            1 => wgpu::TextureFormat::R16Float,
            2 => wgpu::TextureFormat::Rg16Float,
            _ => wgpu::TextureFormat::Rgba16Float,
        };
        let (one_ref, two_ref) = (&one, &two);
        let [across_rgba, across_luma, across_chroma, down_rgba, bt709, bt601] = std::thread::scope(|scope| {
            let jobs = [
                (across("0.5"), one_ref, f16(4)),
                (across("0.5"), one_ref, f16(1)),
                (across("0.25"), one_ref, f16(2)),
                (down_rgba, one_ref, target),
                (down_nv12("false"), two_ref, target),
                (down_nv12("true"), two_ref, target),
            ]
            .map(|(source, layout, format)| scope.spawn(move || pipeline(device, layout, format, &source)));
            jobs.map(|j| j.join().expect("frame filter compile"))
        });
        Self { one, two, across_rgba, across_luma, across_chroma, down_rgba, down_nv12: [bt709, bt601], groups: HashMap::new(), rows: HashMap::new() }
    }

    /// Pass 1 for drawing `shown` at `place`: its rows filtered across, into
    /// the [`Rows`] for that place.
    fn across(&mut self, device: &wgpu::Device, encoder: &mut wgpu::CommandEncoder, shown: &Shown, place: [i32; 4]) {
        if place[2] <= 0 || place[3] <= 0 {
            return;
        }
        let now = Instant::now();
        self.groups.retain(|_, (_, used)| now.duration_since(*used).as_secs() < 2);
        self.rows.retain(|_, (_, used)| now.duration_since(*used).as_secs() < 2);
        let views = shown.planes.views();
        let groups = &mut self.groups.entry(shown.key).or_insert_with(|| {
            let groups = views
                .iter()
                .map(|v| {
                    device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("decoded frame"),
                        layout: &self.one,
                        entries: &[wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(v) }],
                    })
                })
                .collect();
            (groups, now)
        });
        groups.1 = now;
        let key = shown.rows_key(place);
        let rows = &mut self.rows.entry(key).or_insert_with(|| {
            let formats: &[wgpu::TextureFormat] = match key.kind {
                Kind::Rgba => &[wgpu::TextureFormat::Rgba16Float],
                Kind::Nv12 { .. } => &[wgpu::TextureFormat::R16Float, wgpu::TextureFormat::Rg16Float],
            };
            let views: Vec<wgpu::TextureView> = formats
                .iter()
                .zip(key.planes)
                .map(|(&format, (_, height))| {
                    device
                        .create_texture(&wgpu::TextureDescriptor {
                            label: Some("frame filter rows"),
                            size: wgpu::Extent3d { width: place[2] as u32, height: height.max(1), depth_or_array_layers: 1 },
                            mip_level_count: 1,
                            sample_count: 1,
                            dimension: wgpu::TextureDimension::D2,
                            format,
                            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
                            view_formats: &[],
                        })
                        .create_view(&wgpu::TextureViewDescriptor::default())
                })
                .collect();
            let entries: Vec<_> =
                views.iter().enumerate().map(|(i, v)| wgpu::BindGroupEntry { binding: i as u32, resource: wgpu::BindingResource::TextureView(v) }).collect();
            let layout = if views.len() == 1 { &self.one } else { &self.two };
            let group = device.create_bind_group(&wgpu::BindGroupDescriptor { label: Some("frame filter rows"), layout, entries: &entries });
            (Rows { views, group }, now)
        });
        rows.1 = now;
        let pipelines = match key.kind {
            Kind::Rgba => vec![&self.across_rgba],
            Kind::Nv12 { .. } => vec![&self.across_luma, &self.across_chroma],
        };
        let groups = &self.groups[&shown.key].0;
        for ((view, pipeline), group) in rows.0.views.iter().zip(pipelines).zip(groups) {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("frame filter: across"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, group, &[]);
            pass.draw(0..3, 0..1);
        }
    }

    /// Pass 2: the rows [`Self::across`] made for `place` filtered down,
    /// into `pass` (its viewport at `place`).
    fn down(&self, pass: &mut wgpu::RenderPass<'_>, shown: &Shown, place: [i32; 4]) {
        let Some((rows, _)) = self.rows.get(&shown.rows_key(place)) else { return };
        match shown.planes.kind() {
            Kind::Rgba => pass.set_pipeline(&self.down_rgba),
            Kind::Nv12 { bt601 } => pass.set_pipeline(&self.down_nv12[usize::from(bt601)]),
        }
        pass.set_bind_group(0, &rows.group, &[]);
        pass.draw(0..3, 0..1);
    }
}

/// A pipeline drawing one triangle over its target with `source`.
fn pipeline(device: &wgpu::Device, layout: &wgpu::BindGroupLayout, target: wgpu::TextureFormat, source: &str) -> wgpu::RenderPipeline {
    let t0 = std::time::Instant::now();
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("frame filter"), source: wgpu::ShaderSource::Wgsl(source.into()) });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: Some("frame filter"), bind_group_layouts: &[Some(layout)], immediate_size: 0 });
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
    if std::env::var_os("HESTECLIPS_DEBUG_VIDEO").is_some() {
        eprintln!("frame filter pipeline ({target:?}) in {} ms", t0.elapsed().as_millis());
    }
    pipeline
}

/// Lanczos-3: one full-viewport triangle (egui sets the viewport to the
/// callback's rect); the lobes stretched by the reduction so a smaller
/// picture is filtered rather than skipped through.
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
"#;

/// Pass 1: each pixel of the output is one source row (its `pos.y`)
/// filtered across, centred under the output column. `OFFSET`: a source
/// pixel's centre is 0.5 from its left edge; chroma's samples sit 0.25 from
/// it (left-aligned with luma's first column, at half the resolution).
const ACROSS: &str = r#"
@group(0) @binding(0) var plane: texture_2d<f32>;

@fragment
fn fs(in: Out) -> @location(0) vec4<f32> {
    let dims = vec2<i32>(textureDimensions(plane));
    let width = f32(dims.x);
    // Source pixels per output pixel: above 1 when shown smaller.
    let s = max(abs(dpdx(in.uv.x)) * width, 1.0);
    let x = in.uv.x * width - OFFSET;
    let y = clamp(i32(in.pos.y), 0, dims.y - 1);
    let r = min(3.0 * s, 12.0);
    let lo = i32(floor(x - r)) + 1;
    let n = min(i32(floor(x + r)) - lo + 1, 25);
    var sum = vec4<f32>(0.0);
    var total = 0.0;
    for (var i = 0; i < n; i++) {
        let w = lanczos3((f32(lo + i) - x) / s);
        sum += textureLoad(plane, vec2<i32>(clamp(lo + i, 0, dims.x - 1), y), 0) * w;
        total += w;
    }
    return sum / total;
}
"#;

/// Pass 2's filter: the output pixel's column of pass 1's rows, filtered
/// down (`s`: rows per output pixel, at least 1).
const DOWN: &str = r#"
fn down(rows: texture_2d<f32>, uv: vec2<f32>, s: f32) -> vec4<f32> {
    let dims = vec2<i32>(textureDimensions(rows));
    let col = clamp(i32(uv.x * f32(dims.x)), 0, dims.x - 1);
    let y = uv.y * f32(dims.y) - 0.5;
    let r = min(3.0 * s, 12.0);
    let lo = i32(floor(y - r)) + 1;
    let n = min(i32(floor(y + r)) - lo + 1, 25);
    var sum = vec4<f32>(0.0);
    var total = 0.0;
    for (var j = 0; j < n; j++) {
        let w = lanczos3((f32(lo + j) - y) / s);
        sum += textureLoad(rows, vec2<i32>(col, clamp(lo + j, 0, dims.y - 1)), 0) * w;
        total += w;
    }
    return sum / total;
}

fn to_linear(c: vec3<f32>) -> vec3<f32> {
    return select(pow((c + 0.055) / 1.055, vec3<f32>(2.4)), c / 12.92, c <= vec3<f32>(0.04045));
}

fn finish(rgb: vec3<f32>) -> vec4<f32> {
    var c = clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0));
    if SRGB_TARGET { c = to_linear(c); }
    return vec4<f32>(c, 1.0);
}
"#;

const DOWN_RGBA: &str = r#"
@group(0) @binding(0) var rows: texture_2d<f32>;

@fragment
fn fs(in: Out) -> @location(0) vec4<f32> {
    let s = max(abs(dpdy(in.uv.y)) * f32(textureDimensions(rows).y), 1.0);
    return finish(down(rows, in.uv, s).rgb);
}
"#;

const DOWN_NV12: &str = r#"
@group(0) @binding(0) var luma: texture_2d<f32>;
@group(0) @binding(1) var chroma: texture_2d<f32>;

@fragment
fn fs(in: Out) -> @location(0) vec4<f32> {
    // Each plane at its own resolution; chroma's rows sit between luma's
    // (H.264's siting), which centring on its own rows gives.
    let dy = abs(dpdy(in.uv.y));
    let y = down(luma, in.uv, max(dy * f32(textureDimensions(luma).y), 1.0)).r;
    let c = down(chroma, in.uv, max(dy * f32(textureDimensions(chroma).y), 1.0)).rg;

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

    /// Every filter shader compiles for every backend (naga validates them
    /// here; the app would only find out when a clip opens).
    #[test]
    fn shaders_validate() {
        let sources = [
            format!("{COMMON}{ACROSS}").replace("OFFSET", "0.5"),
            format!("{COMMON}{ACROSS}").replace("OFFSET", "0.25"),
            format!("{COMMON}{DOWN}{DOWN_RGBA}").replace("SRGB_TARGET", "false"),
            format!("{COMMON}{DOWN}{DOWN_NV12}").replace("SRGB_TARGET", "true").replace("BT601", "false"),
            format!("{COMMON}{DOWN}{DOWN_NV12}").replace("SRGB_TARGET", "false").replace("BT601", "true"),
        ];
        for code in sources {
            let module = naga::front::wgsl::parse_str(&code).unwrap_or_else(|e| panic!("{}", e.emit_to_string(&code)));
            naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::all())
                .validate(&module)
                .unwrap_or_else(|e| panic!("{e:?}"));
        }
    }

    /// Draw `shown` at `w`×`h` into a texture of `format` with both passes,
    /// as the app does; its pixels as tight RGBA rows.
    fn render(device: &wgpu::Device, queue: &wgpu::Queue, shown: &Shown, (w, h): (u32, u32), format: wgpu::TextureFormat) -> Vec<u8> {
        let mut scaler = Scaler::new(device, format);
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let row = (w * 4).div_ceil(256) * 256;
        let buf = device.create_buffer(&wgpu::BufferDescriptor { label: None, size: (row * h) as u64, usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false });
        let mut enc = device.create_command_encoder(&Default::default());
        let place = [0, 0, w as i32, h as i32];
        scaler.across(device, &mut enc, shown, place);
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
            scaler.down(&mut pass, shown, place);
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
        let out: Vec<u8> = (0..h as usize).flat_map(|r| mapped[r * row as usize..r * row as usize + w as usize * 4].to_vec()).collect();
        drop(mapped);
        buf.unmap();
        out
    }

    /// A texture holding `data` (`channels` bytes a pixel).
    fn upload(device: &wgpu::Device, queue: &wgpu::Queue, (w, h): (u32, u32), format: wgpu::TextureFormat, channels: u32, data: &[u8]) -> wgpu::TextureView {
        let t = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(t.as_image_copy(), data, wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * channels), rows_per_image: None }, t.size());
        t.create_view(&Default::default())
    }

    fn lanczos3(x: f64) -> f64 {
        let ax = x.abs();
        if ax < 1e-5 {
            return 1.0;
        }
        if ax >= 3.0 {
            return 0.0;
        }
        let px = std::f64::consts::PI * x;
        3.0 * px.sin() * (px / 3.0).sin() / (px * px)
    }

    /// The window the old one-pass shader used along one axis: around
    /// `center` (in a plane `size` pixels long), `s` of its pixels per output
    /// pixel. (Index, weight) pairs.
    fn window(center: f64, s: f64, size: usize) -> Vec<(usize, f64)> {
        let r = (3.0 * s).min(12.0);
        let lo = (center - r).floor() as i64 + 1;
        let n = ((center + r).floor() as i64 - lo + 1).min(25);
        (0..n).map(|i| ((lo + i).clamp(0, size as i64 - 1) as usize, lanczos3(((lo + i) as f64 - center) / s))).collect()
    }

    /// Filtered value of `plane` (`w`×`h`, `channels` a pixel) for the old
    /// shader's windows `wx` × `wy`.
    fn filter(plane: &[u8], w: usize, channels: usize, wx: &[(usize, f64)], wy: &[(usize, f64)], c: usize) -> f64 {
        let (tx, ty): (f64, f64) = (wx.iter().map(|p| p.1).sum(), wy.iter().map(|p| p.1).sum());
        let mut sum = 0.0;
        for &(y, a) in wy {
            for &(x, b) in wx {
                sum += plane[(y * w + x) * channels + c] as f64 / 255.0 * a * b;
            }
        }
        sum / (tx * ty)
    }

    /// A picture with sharp edges, fine detail and smooth ramps.
    fn pattern(w: usize, h: usize, channels: usize, seed: u32) -> Vec<u8> {
        (0..w * h * channels)
            .map(|i| {
                let (p, c) = (i / channels, i % channels);
                let (x, y) = (p % w, p / w);
                let hash = ((x as u32).wrapping_mul(73_856_093) ^ (y as u32).wrapping_mul(19_349_663) ^ seed.wrapping_add(c as u32 * 83_492_791)) % 256;
                if (x / 37 + y / 23) % 2 == 0 { hash as u8 } else { ((x * 255) / w) as u8 }
            })
            .collect()
    }

    /// The two passes draw what the one pass did (the shader before them,
    /// worked through on the CPU), shrunk, at 1:1 and enlarged, RGBA and
    /// NV12: within a level, the 16-bit rows' rounding.
    /// `cargo test -p hesteclips two_passes -- --ignored --nocapture`
    #[test]
    #[ignore = "needs a graphics card"]
    fn two_passes_match_one() {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = pollster::block_on(instance.request_adapter(&Default::default())).expect("adapter");
        let (device, queue) = pollster::block_on(adapter.request_device(&Default::default())).expect("device");
        let (w, h) = (640usize, 360usize);
        let rgba = pattern(w, h, 4, 1);
        let luma = pattern(w, h, 1, 2);
        let chroma = pattern(w / 2, h / 2, 2, 3);
        let rgba_shown = Shown { key: (0, 0), planes: Planes::Rgba(upload(&device, &queue, (w as u32, h as u32), wgpu::TextureFormat::Rgba8Unorm, 4, &rgba)) };
        let nv12_shown = Shown {
            key: (0, 1),
            planes: Planes::Nv12 {
                y: upload(&device, &queue, (w as u32, h as u32), wgpu::TextureFormat::R8Unorm, 1, &luma),
                cbcr: upload(&device, &queue, (w as u32 / 2, h as u32 / 2), wgpu::TextureFormat::Rg8Unorm, 2, &chroma),
                bt601: false,
            },
        };
        for (ow, oh) in [(427usize, 240usize), (640, 360), (213, 120), (900, 506)] {
            let (sx, sy) = ((w as f64 / ow as f64).max(1.0), (h as f64 / oh as f64).max(1.0));
            // The old shader's sample positions: luma (and RGBA) at pixel
            // centres; chroma half that, left-sited across, between rows down.
            let lx = |o: usize| (o as f64 + 0.5) / ow as f64 * w as f64 - 0.5;
            let ly = |o: usize| (o as f64 + 0.5) / oh as f64 * h as f64 - 0.5;
            let check = |name: &str, got: &[u8], want: &dyn Fn(usize, usize) -> [f64; 3]| {
                let (mut sum, mut worst, mut n) = (0.0, 0.0f64, 0);
                for y in 0..oh {
                    for x in 0..ow {
                        let e = want(x, y);
                        for c in 0..3 {
                            let d = (got[(y * ow + x) * 4 + c] as f64 - (e[c].clamp(0.0, 1.0) * 255.0)).abs();
                            sum += d;
                            worst = worst.max(d);
                            n += 1;
                        }
                    }
                }
                println!("{name} {ow}x{oh}: mean difference {:.3} levels, largest {worst:.2}", sum / n as f64);
                // Rounding to 8 bits alone is 0.25 levels on average.
                assert!(sum / (n as f64) < 0.35 && worst <= 1.5, "{name} {ow}x{oh}");
            };
            let got = render(&device, &queue, &rgba_shown, (ow as u32, oh as u32), wgpu::TextureFormat::Rgba8Unorm);
            check("RGBA", &got, &|x, y| {
                let (wx, wy) = (window(lx(x), sx, w), window(ly(y), sy, h));
                [0, 1, 2].map(|c| filter(&rgba, w, 4, &wx, &wy, c))
            });
            let got = render(&device, &queue, &nv12_shown, (ow as u32, oh as u32), wgpu::TextureFormat::Rgba8Unorm);
            check("NV12", &got, &|x, y| {
                let yy = filter(&luma, w, 1, &window(lx(x), sx, w), &window(ly(y), sy, h), 0);
                let (cwx, cwy) = (window(lx(x) * 0.5, (sx * 0.5).max(1.0), w / 2), window(ly(y) * 0.5 - 0.25, (sy * 0.5).max(1.0), h / 2));
                let (cb, cr) = (filter(&chroma, w / 2, 2, &cwx, &cwy, 0), filter(&chroma, w / 2, 2, &cwx, &cwy, 1));
                let (yy, cb, cr) = ((yy * 255.0 - 16.0) / 219.0, (cb * 255.0 - 128.0) / 224.0, (cr * 255.0 - 128.0) / 224.0);
                [yy + 1.5748 * cr, yy - 0.187324 * cb - 0.468124 * cr, yy + 1.8556 * cb]
            });
        }
    }

    /// A live preview frame (a ScreenCaptureKit buffer) opens as textures.
    /// `cargo test -p hesteclips sck_preview -- --ignored --nocapture`
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore]
    fn sck_preview_frame_opens() {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor { backends: wgpu::Backends::METAL, ..wgpu::InstanceDescriptor::new_without_display_handle() });
        let adapter = pollster::block_on(instance.request_adapter(&Default::default())).expect("adapter");
        let (device, _queue) = pollster::block_on(adapter.request_device(&Default::default())).expect("device");
        let preview = capture::preview::VideoPreview::start(&capture::VideoSource::Screen { id: String::new(), hook: Default::default() }, Some(720), 30, None, None);
        let frame = (0..100)
            .find_map(|_| {
                capture::preview::request();
                std::thread::sleep(std::time::Duration::from_millis(50));
                capture::preview::latest()
            })
            .unwrap_or_else(|| panic!("no preview frame: {:?}", preview.error()));
        let s = frame.surface.as_ref().expect("a surface");
        let surface = s.io_surface().unwrap();
        println!("frame {}x{}, surface {}x{}, planes {:?}", frame.width, frame.height, surface.width(), surface.height(),
            (0..2).map(|p| (surface.width_of_plane(p), surface.height_of_plane(p))).collect::<Vec<_>>());
        let y = unsafe { metal::open(&device, &surface, 0, wgpu::TextureFormat::R8Unorm, frame.width, frame.height) };
        let c = unsafe { metal::open(&device, &surface, 1, wgpu::TextureFormat::Rg8Unorm, frame.width.div_ceil(2), frame.height.div_ceil(2)) };
        println!("y: {:?}, cbcr: {:?}", y.as_ref().map(|_| ()), c.as_ref().map(|_| ()));
        y.unwrap();
        c.unwrap();
    }

    /// A decoded NV12 frame drawn by the real filter matches ffmpeg's
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
        let shown = Shown { key: (0, 0), planes: Planes::Nv12 { y: y.create_view(&Default::default()), cbcr: c.create_view(&Default::default()), bt601: false } };
        for out_w in [w, 1280] {
            let out_h = ((out_w as f64 * h as f64 / w as f64 / 2.0).round() as u32) * 2;
            let ours = render(&device, &queue, &shown, (out_w, out_h), wgpu::TextureFormat::Rgba8Unorm);
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
