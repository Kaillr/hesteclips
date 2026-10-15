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

    /// Its colours, blurred very wide, over `cover` (clipped to `painter`'s
    /// clip): the stage's ambient light. Made from the frame on the GPU each
    /// time it changes (see [`Glow`]); `id` keeps one stage's light apart
    /// from another's.
    pub fn paint_glow(&self, painter: &egui::Painter, cover: egui::Rect, id: egui::Id) {
        painter.add(egui_wgpu::Callback::new_paint_callback(cover, Ambient(self.clone(), id)));
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
    /// The ambient light: shrinking a frame to a few dozen pixels (RGBA,
    /// NV12 with each matrix), easing it into the last, blurring it across
    /// and down, and drawing it big.
    glow_shrink: [wgpu::RenderPipeline; 3],
    /// Blurring the shrunk frame and easing the light towards it, in one;
    /// and starting the light afresh from it.
    glow_blur: [wgpu::RenderPipeline; 2],
    glow_show: wgpu::RenderPipeline,
    /// Two textures and a sampler (the ease and the show read with one).
    glow_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    glows: HashMap<egui::Id, (Glow, Instant)>,
}

/// One stage's ambient light: the frame shrunk (`small`), and the light,
/// blurred and eased towards each new frame, in turns (`light`); each
/// `GLOW_W`×`GLOW_H`.
struct Glow {
    small: wgpu::TextureView,
    light: [wgpu::TextureView; 2],
    /// Read `light[0]` or `light[1]`, for drawing it.
    show: [wgpu::BindGroup; 2],
    /// Which of `light` holds the light now.
    flip: usize,
    /// The frame it last took in (the same one drawn again isn't eased in twice).
    last: Option<(usize, usize)>,
    /// When it last took one in: a long gap (a seek, a new clip) starts over
    /// instead of fading from a picture that's gone.
    at: Instant,
}

/// The ambient light's size: small enough that drawing it big is a wide blur.
const GLOW_W: u32 = 64;
const GLOW_H: u32 = 36;

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

/// Draws a stage's ambient light from a frame, as an egui paint callback.
struct Ambient(Shown, egui::Id);

impl CallbackTrait for Ambient {
    fn prepare(
        &self,
        device: &wgpu::Device,
        _queue: &wgpu::Queue,
        _screen: &ScreenDescriptor,
        encoder: &mut wgpu::CommandEncoder,
        resources: &mut CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        if let Some(scaler) = resources.get_mut::<Scaler>() {
            scaler.glow(device, encoder, &self.0, self.1);
        }
        Vec::new()
    }

    fn paint(&self, _info: egui::PaintCallbackInfo, pass: &mut wgpu::RenderPass<'static>, resources: &CallbackResources) {
        if let Some(scaler) = resources.get::<Scaler>() {
            scaler.show_glow(pass, self.1);
        }
    }
}

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
        scaler.across(device, encoder, &self.0, whole_picture(&self.1, screen.pixels_per_point));
        Vec::new()
    }

    fn paint(&self, info: egui::PaintCallbackInfo, pass: &mut wgpu::RenderPass<'static>, resources: &CallbackResources) {
        let Some(scaler) = resources.get::<Scaler>() else { return };
        let place = whole_picture(&self.1, info.pixels_per_point);
        // egui's own viewport is cut to the window: scrolled partly off it,
        // the whole picture would squeeze into what's left. The scissor
        // (the clip) still hides what's outside.
        pass.set_viewport(place[0] as f32, place[1] as f32, place[2] as f32, place[3] as f32, 0.0, 1.0);
        scaler.down(pass, &self.0, place);
    }
}

/// Where the picture goes, in screen pixels (left, top, width, height),
/// rounded but not cut to the window: it may reach past its edge.
fn whole_picture(rect: &egui::Rect, ppp: f32) -> [i32; 4] {
    let (left, top) = ((rect.min.x * ppp).round(), (rect.min.y * ppp).round());
    let (right, bottom) = ((rect.max.x * ppp).round(), (rect.max.y * ppp).round());
    [left as i32, top as i32, (right - left) as i32, (bottom - top) as i32]
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
        let glow_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ambient light"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture { sample_type: wgpu::TextureSampleType::Float { filterable: true }, view_dimension: wgpu::TextureViewDimension::D2, multisampled: false },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture { sample_type: wgpu::TextureSampleType::Float { filterable: true }, view_dimension: wgpu::TextureViewDimension::D2, multisampled: false },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry { binding: 2, visibility: wgpu::ShaderStages::FRAGMENT, ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering), count: None },
            ],
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("ambient light"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });
        let light = wgpu::TextureFormat::Rgba16Float;
        let shrink_nv12 = |bt601: &str| format!("{COMMON}{GLOW_AVERAGE}{GLOW_SHRINK_NV12}").replace("BT601", bt601);
        let show = format!("{COMMON}{GLOW_SHOW}").replace("SRGB_TARGET", srgb);
        let gl = &glow_layout;
        let [s_rgba, s_709, s_601, blur, fresh, show] = std::thread::scope(|scope| {
            let jobs = [
                (format!("{COMMON}{GLOW_AVERAGE}{GLOW_SHRINK_RGBA}"), one_ref, light),
                (shrink_nv12("false"), two_ref, light),
                (shrink_nv12("true"), two_ref, light),
                (format!("{COMMON}{GLOW_BLUR}").replace("EASE", "0.2"), gl, light),
                (format!("{COMMON}{GLOW_BLUR}").replace("EASE", "1.0"), gl, light),
                (show, gl, target),
            ]
            .map(|(source, layout, format)| scope.spawn(move || pipeline(device, layout, format, &source)));
            jobs.map(|j| j.join().expect("ambient light compile"))
        });
        Self {
            one,
            two,
            across_rgba,
            across_luma,
            across_chroma,
            down_rgba,
            down_nv12: [bt709, bt601],
            groups: HashMap::new(),
            rows: HashMap::new(),
            glow_shrink: [s_rgba, s_709, s_601],
            glow_blur: [blur, fresh],
            glow_show: show,
            glow_layout,
            sampler,
            glows: HashMap::new(),
        }
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
        self.frame_groups(device, shown);
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

    /// Bind groups reading the frame's planes: one per plane, then (for
    /// NV12) both together for the ambient light's shrink.
    fn frame_groups(&mut self, device: &wgpu::Device, shown: &Shown) {
        let now = Instant::now();
        let views = shown.planes.views();
        let groups = &mut self.groups.entry(shown.key).or_insert_with(|| {
            let mut groups: Vec<wgpu::BindGroup> = views
                .iter()
                .map(|v| {
                    device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("decoded frame"),
                        layout: &self.one,
                        entries: &[wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(v) }],
                    })
                })
                .collect();
            if views.len() == 2 {
                let entries: Vec<_> = views.iter().enumerate().map(|(i, v)| wgpu::BindGroupEntry { binding: i as u32, resource: wgpu::BindingResource::TextureView(v) }).collect();
                groups.push(device.create_bind_group(&wgpu::BindGroupDescriptor { label: Some("decoded frame planes"), layout: &self.two, entries: &entries }));
            }
            (groups, now)
        });
        groups.1 = now;
    }

    /// The ambient light for stage `id`: `shown` shrunk to a few dozen
    /// pixels, eased into the light so far (a cut fades in over a moment
    /// instead of flashing), then blurred across and down.
    fn glow(&mut self, device: &wgpu::Device, encoder: &mut wgpu::CommandEncoder, shown: &Shown, id: egui::Id) {
        let now = Instant::now();
        self.glows.retain(|_, (_, used)| now.duration_since(*used).as_secs() < 2);
        self.frame_groups(device, shown);
        let tex = |label| {
            device
                .create_texture(&wgpu::TextureDescriptor {
                    label: Some(label),
                    size: wgpu::Extent3d { width: GLOW_W, height: GLOW_H, depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rgba16Float,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                })
                .create_view(&wgpu::TextureViewDescriptor::default())
        };
        let (layout, sampler) = (&self.glow_layout, &self.sampler);
        let (glow, used) = self.glows.entry(id).or_insert_with(|| {
            let light = [tex("ambient: light"), tex("ambient: light")];
            let show = |v: &wgpu::TextureView| {
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("ambient light"),
                    layout,
                    entries: &[
                        wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(v) },
                        wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(v) },
                        wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(sampler) },
                    ],
                })
            };
            let show = [show(&light[0]), show(&light[1])];
            (Glow { small: tex("ambient: small"), light, show, flip: 0, last: None, at: now }, now)
        });
        *used = now;
        if glow.last == Some(shown.key) {
            return;
        }
        // Not drawn for a moment (paused, a new clip, a jump): this frame
        // alone, no fade from what was.
        let fresh = glow.last.is_none() || now.duration_since(glow.at).as_millis() > 250;
        glow.last = Some(shown.key);
        glow.at = now;
        let pair = |a: &wgpu::TextureView, b: &wgpu::TextureView| {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("ambient light"),
                layout: &self.glow_layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(a) },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(b) },
                    wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                ],
            })
        };
        let step = |encoder: &mut wgpu::CommandEncoder, target: &wgpu::TextureView, pipeline: &wgpu::RenderPipeline, group: &wgpu::BindGroup| {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("ambient light"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
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
        };
        // Shrink: from the frame's planes.
        let frame = &self.groups[&shown.key].0;
        let (shrink, group) = match shown.planes.kind() {
            Kind::Rgba => (&self.glow_shrink[0], &frame[0]),
            Kind::Nv12 { bt601 } => (&self.glow_shrink[1 + usize::from(bt601)], &frame[2]),
        };
        step(encoder, &glow.small, shrink, group);
        // Blurred, and the light so far eased towards it (blurring and
        // easing in either order come to the same). Fresh: it alone.
        let (from, to) = (glow.flip, 1 - glow.flip);
        step(encoder, &glow.light[to], &self.glow_blur[usize::from(fresh)], &pair(&glow.small, &glow.light[from]));
        glow.flip = to;
    }

    /// Draw stage `id`'s ambient light over `pass`'s viewport.
    fn show_glow(&self, pass: &mut wgpu::RenderPass<'_>, id: egui::Id) {
        let Some((glow, _)) = self.glows.get(&id) else { return };
        pass.set_pipeline(&self.glow_show);
        pass.set_bind_group(0, &glow.show[glow.flip], &[]);
        pass.draw(0..3, 0..1);
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

/// The ambient light, step 1: each pixel the average of the part of the
/// frame it covers, in linear light, so a bright spot lights the surround as
/// much as it would a wall. 3×3 looks across it: the wide blur after hides
/// how few, and easing frame to frame keeps a small bright thing from
/// flickering (6×6 cost twice as much for no visible difference).
const GLOW_AVERAGE: &str = r#"
fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    let v = clamp(c, vec3<f32>(0.0), vec3<f32>(1.0));
    return select(pow((v + 0.055) / 1.055, vec3<f32>(2.4)), v / 12.92, v <= vec3<f32>(0.04045));
}

const LOOKS: i32 = 3;
"#;

const GLOW_SHRINK_RGBA: &str = r#"
@group(0) @binding(0) var frame: texture_2d<f32>;

@fragment
fn fs(in: Out) -> @location(0) vec4<f32> {
    let dims = vec2<f32>(textureDimensions(frame));
    let cell = vec2<f32>(1.0 / 64.0, 1.0 / 36.0);
    let corner = floor(in.uv / cell) * cell;
    var sum = vec3<f32>(0.0);
    for (var j = 0; j < LOOKS; j++) {
        for (var i = 0; i < LOOKS; i++) {
            let uv = corner + (vec2<f32>(f32(i), f32(j)) + 0.5) / f32(LOOKS) * cell;
            let p = vec2<i32>(clamp(uv * dims, vec2<f32>(0.0), dims - 1.0));
            sum += srgb_to_linear(textureLoad(frame, p, 0).rgb);
        }
    }
    return vec4<f32>(sum / f32(LOOKS * LOOKS), 1.0);
}
"#;

const GLOW_SHRINK_NV12: &str = r#"
@group(0) @binding(0) var luma: texture_2d<f32>;
@group(0) @binding(1) var chroma: texture_2d<f32>;

@fragment
fn fs(in: Out) -> @location(0) vec4<f32> {
    let ly = vec2<f32>(textureDimensions(luma));
    let lc = vec2<f32>(textureDimensions(chroma));
    let cell = vec2<f32>(1.0 / 64.0, 1.0 / 36.0);
    let corner = floor(in.uv / cell) * cell;
    var sum = vec3<f32>(0.0);
    for (var j = 0; j < LOOKS; j++) {
        for (var i = 0; i < LOOKS; i++) {
            let uv = corner + (vec2<f32>(f32(i), f32(j)) + 0.5) / f32(LOOKS) * cell;
            let y = textureLoad(luma, vec2<i32>(clamp(uv * ly, vec2<f32>(0.0), ly - 1.0)), 0).r;
            let c = textureLoad(chroma, vec2<i32>(clamp(uv * lc, vec2<f32>(0.0), lc - 1.0)), 0).rg;
            let yy = (y * 255.0 - 16.0) / 219.0;
            let cb = (c.x * 255.0 - 128.0) / 224.0;
            let cr = (c.y * 255.0 - 128.0) / 224.0;
            var rgb: vec3<f32>;
            if BT601 {
                rgb = vec3<f32>(yy + 1.402 * cr, yy - 0.344136 * cb - 0.714136 * cr, yy + 1.772 * cb);
            } else {
                rgb = vec3<f32>(yy + 1.5748 * cr, yy - 0.187324 * cb - 0.468124 * cr, yy + 1.8556 * cb);
            }
            sum += srgb_to_linear(rgb);
        }
    }
    return vec4<f32>(sum / f32(LOOKS * LOOKS), 1.0);
}
"#;

/// Step 2: the shrunk frame blurred wide (a 2D Gaussian, about a third of
/// the picture across, so the light is a soft wash, not shapes), and the
/// light so far (`previous`) a step towards it (`EASE`; 1 starts afresh): a
/// cut fades over about a tenth of a second instead of flashing.
const GLOW_BLUR: &str = r#"
@group(0) @binding(0) var small: texture_2d<f32>;
@group(0) @binding(1) var previous: texture_2d<f32>;
@group(0) @binding(2) var smp: sampler;

@fragment
fn fs(in: Out) -> @location(0) vec4<f32> {
    let texel = 1.0 / vec2<f32>(textureDimensions(small));
    var sum = vec4<f32>(0.0);
    var total = 0.0;
    // Every other texel (the sampler blends the one between), out to 12.
    for (var j = -6; j <= 6; j++) {
        for (var i = -6; i <= 6; i++) {
            let d = vec2<f32>(f32(i), f32(j)) * 2.0;
            let w = exp(-dot(d, d) / (2.0 * 6.0 * 6.0));
            sum += textureSample(small, smp, in.uv + texel * (d + 0.5)) * w;
            total += w;
        }
    }
    let blurred = sum / total;
    return mix(textureSample(previous, smp, in.uv), blurred, EASE);
}
"#;

/// Step 3: the light drawn big, sampled smoothly (bicubic B-spline from four
/// bilinear looks: plain bilinear shows a grid of diamonds this enlarged),
/// back to sRGB for the screen.
const GLOW_SHOW: &str = r#"
@group(0) @binding(0) var light: texture_2d<f32>;
@group(0) @binding(1) var unused: texture_2d<f32>;
@group(0) @binding(2) var smp: sampler;

fn to_srgb(c: vec3<f32>) -> vec3<f32> {
    let v = clamp(c, vec3<f32>(0.0), vec3<f32>(1.0));
    return select(1.055 * pow(v, vec3<f32>(1.0 / 2.4)) - 0.055, v * 12.92, v <= vec3<f32>(0.0031308));
}

fn smooth_sample(uv: vec2<f32>) -> vec4<f32> {
    let size = vec2<f32>(textureDimensions(light));
    let p = uv * size - 0.5;
    let f = fract(p);
    let base = floor(p);
    // B-spline weights, folded into two bilinear looks per axis.
    let w0 = (1.0 - f) * (1.0 - f) * (1.0 - f) / 6.0;
    let w1 = (4.0 - 6.0 * f * f + 3.0 * f * f * f) / 6.0;
    let w2 = (1.0 + 3.0 * f + 3.0 * f * f - 3.0 * f * f * f) / 6.0;
    let w3 = f * f * f / 6.0;
    let g0 = w0 + w1;
    let g1 = w2 + w3;
    let h0 = (base - 0.5 + w1 / g0) / size;
    let h1 = (base + 1.5 + w3 / g1) / size;
    let a = textureSample(light, smp, vec2<f32>(h0.x, h0.y));
    let b = textureSample(light, smp, vec2<f32>(h1.x, h0.y));
    let c = textureSample(light, smp, vec2<f32>(h0.x, h1.y));
    let d = textureSample(light, smp, vec2<f32>(h1.x, h1.y));
    return mix(mix(a, b, g1.x / (g0.x + g1.x)), mix(c, d, g1.x / (g0.x + g1.x)), g1.y / (g0.y + g1.y));
}

@fragment
fn fs(in: Out) -> @location(0) vec4<f32> {
    var c = smooth_sample(in.uv).rgb;
    // A little more colour: spread this wide it greys out.
    let grey = dot(c, vec3<f32>(0.2126, 0.7152, 0.0722));
    c = max(mix(vec3<f32>(grey), c, 1.35), vec3<f32>(0.0)) * 0.8;
    if SRGB_TARGET { return vec4<f32>(c, 1.0); }
    return vec4<f32>(to_srgb(c), 1.0);
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
            format!("{COMMON}{GLOW_AVERAGE}{GLOW_SHRINK_RGBA}"),
            format!("{COMMON}{GLOW_AVERAGE}{GLOW_SHRINK_NV12}").replace("BT601", "false"),
            format!("{COMMON}{GLOW_BLUR}").replace("EASE", "0.2"),
            format!("{COMMON}{GLOW_SHOW}").replace("SRGB_TARGET", "true"),
            format!("{COMMON}{GLOW_SHOW}").replace("SRGB_TARGET", "false"),
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

    /// The stage's light, as the app makes it: `frames` taken in one after
    /// another (each a new frame, as playing gives), then drawn `w`×`h`.
    fn glow_after(device: &wgpu::Device, queue: &wgpu::Queue, frames: &[&Shown], (w, h): (u32, u32)) -> Vec<u8> {
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let mut scaler = Scaler::new(device, format);
        let id = egui::Id::new("test stage");
        for f in frames {
            let mut enc = device.create_command_encoder(&Default::default());
            scaler.glow(device, &mut enc, f, id);
            queue.submit([enc.finish()]);
        }
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
            scaler.show_glow(&mut pass, id);
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

    /// A frame of one colour (RGBA), as a new decoder buffer `key`.
    fn solid(device: &wgpu::Device, queue: &wgpu::Queue, key: usize, rgb: [u8; 3]) -> Shown {
        let (w, h) = (320u32, 180u32);
        let data: Vec<u8> = (0..w * h).flat_map(|_| [rgb[0], rgb[1], rgb[2], 255]).collect();
        Shown { key: (key, 0), planes: Planes::Rgba(upload(device, queue, (w, h), wgpu::TextureFormat::Rgba8Unorm, 4, &data)) }
    }

    /// A red frame lights the stage red; a cut to blue fades rather than
    /// flips; drawn big, the light is smooth (no step between neighbours).
    /// `cargo test -p hesteclips stage_light -- --ignored --nocapture`
    #[test]
    #[ignore = "needs a graphics card"]
    fn stage_light() {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = pollster::block_on(instance.request_adapter(&Default::default())).expect("adapter");
        let (device, queue) = pollster::block_on(adapter.request_device(&Default::default())).expect("device");
        let red = solid(&device, &queue, 1, [220, 30, 30]);
        let blue = solid(&device, &queue, 2, [30, 30, 220]);
        let px = |img: &[u8], w: usize, x: usize, y: usize| [img[(y * w + x) * 4], img[(y * w + x) * 4 + 1], img[(y * w + x) * 4 + 2]];
        let (w, h) = (400usize, 225usize);
        let lit = glow_after(&device, &queue, &[&red], (w as u32, h as u32));
        let c = px(&lit, w, w / 2, h / 2);
        assert!(c[0] > 150 && c[1] < 60 && c[2] < 60, "red lights red: {c:?}");
        // One frame after the cut: still mostly red, a little blue.
        let cut = glow_after(&device, &queue, &[&red, &blue], (w as u32, h as u32));
        let c = px(&cut, w, w / 2, h / 2);
        assert!(c[0] > c[2] && c[2] > 40, "a cut fades in: {c:?}");
        // Smooth: neighbouring pixels a step or two apart at most.
        let mut half = vec![0u8; 320 * 180 * 4];
        for (i, p) in half.chunks_mut(4).enumerate() {
            let v = if i % 320 < 160 { 0 } else { 255 };
            p.copy_from_slice(&[v, v, v, 255]);
        }
        let edge = Shown { key: (4, 0), planes: Planes::Rgba(upload(&device, &queue, (320, 180), wgpu::TextureFormat::Rgba8Unorm, 4, &half)) };
        let soft = glow_after(&device, &queue, &[&edge], (w as u32, h as u32));
        let worst = (1..w).map(|x| (px(&soft, w, x, h / 2)[0] as i32 - px(&soft, w, x - 1, h / 2)[0] as i32).abs()).max().unwrap();
        assert!(worst <= 3, "a hard edge spreads out smoothly (largest step {worst})");
    }

    /// What the light costs next to drawing the frame itself: a 2560×1440
    /// NV12 frame, each timed over many runs on the GPU.
    /// `cargo test -p hesteclips stage_light_cost -- --ignored --nocapture`
    #[test]
    #[ignore = "needs a graphics card"]
    fn stage_light_cost() {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = pollster::block_on(instance.request_adapter(&Default::default())).expect("adapter");
        let (device, queue) = pollster::block_on(adapter.request_device(&Default::default())).expect("device");
        let (w, h) = (2560usize, 1440usize);
        let luma = pattern(w, h, 1, 1);
        let chroma = pattern(w / 2, h / 2, 2, 2);
        let make = |key| Shown {
            key: (key, 0),
            planes: Planes::Nv12 {
                y: upload(&device, &queue, (w as u32, h as u32), wgpu::TextureFormat::R8Unorm, 1, &luma),
                cbcr: upload(&device, &queue, (w as u32 / 2, h as u32 / 2), wgpu::TextureFormat::Rg8Unorm, 2, &chroma),
                bt601: false,
            },
        };
        let frames: Vec<Shown> = (0..4).map(make).collect();
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let mut scaler = Scaler::new(&device, format);
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d { width: 1686, height: 948, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let view = target.create_view(&Default::default());
        let wait = || {
            device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        };
        let runs = 300;
        let mut time = |f: &mut dyn FnMut(&mut Scaler, &mut wgpu::CommandEncoder, &Shown)| {
            // Warm up.
            for s in &frames {
                let mut enc = device.create_command_encoder(&Default::default());
                f(&mut scaler, &mut enc, s);
                queue.submit([enc.finish()]);
            }
            wait();
            let t = std::time::Instant::now();
            for i in 0..runs {
                let mut enc = device.create_command_encoder(&Default::default());
                f(&mut scaler, &mut enc, &frames[i % frames.len()]);
                queue.submit([enc.finish()]);
            }
            wait();
            t.elapsed().as_secs_f64() * 1000.0 / runs as f64
        };
        let place = [0, 0, 1686, 948];
        let frame_ms = time(&mut |sc, enc, s| {
            sc.across(&device, enc, s, place);
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment { view: &view, depth_slice: None, resolve_target: None, ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store } })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            sc.down(&mut pass, s, place);
        });
        let id = egui::Id::new("bench");
        let glow_ms = time(&mut |sc, enc, s| {
            sc.glow(&device, enc, s, id);
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment { view: &view, depth_slice: None, resolve_target: None, ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store } })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            sc.show_glow(&mut pass, id);
        });
        // The light drawn big alone (the frame it's from already taken in).
        let show_ms = time(&mut |sc, enc, _| {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment { view: &view, depth_slice: None, resolve_target: None, ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store } })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            sc.show_glow(&mut pass, id);
        });
        // An empty pass: what any pass costs here, for scale.
        let empty_ms = time(&mut |_, enc, _| {
            let _pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment { view: &view, depth_slice: None, resolve_target: None, ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store } })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
        });
        println!("2560×1440 NV12 frame drawn 1686×948: {frame_ms:.3} ms; its stage light (shrink, ease, blur, drawn 1686×948): {glow_ms:.3} ms ({:.0}% of the frame); drawing the light alone {show_ms:.3} ms; an empty pass {empty_ms:.3} ms", glow_ms / frame_ms * 100.0);
    }
}
