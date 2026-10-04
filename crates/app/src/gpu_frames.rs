//! Decoded frames drawn straight from the GPU (Windows): the decoder renders
//! each frame, at the video's own resolution, into one of a pool of shareable
//! D3D11 textures (`capture::win::decode::Surfaces`); here each is opened once
//! in wgpu's D3D12 device. Nothing is copied back to the CPU or uploaded again.
//!
//! Frames are drawn by our own shader ([`Draw`]) with a Lanczos-3 filter
//! whose kernel widens with the reduction: a picture shown smaller than the
//! video is resampled from every source pixel it covers, as sharp as ffmpeg's
//! default scaler, with no shimmer; at 1:1 every pixel is copied untouched;
//! shown larger, it's Lanczos-upscaled. (egui's own drawing samples
//! bilinearly, which turns a 2560-wide frame drawn 1686 wide soft.)
//!
//! Needs wgpu on its DX12 backend (asked for at startup) and the decoder on
//! the same graphics card; otherwise frames come back as RGBA as before.

use std::collections::HashMap;
use std::sync::OnceLock;

use capture::win::decode::Surface;
use eframe::egui_wgpu::{self, CallbackResources, CallbackTrait, RenderState, ScreenDescriptor};
use eframe::wgpu;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D12::{ID3D12Fence, ID3D12Resource};

static RENDER: OnceLock<RenderState> = OnceLock::new();

/// Remember the app's renderer, at startup.
pub fn init(state: Option<&RenderState>) {
    if let Some(s) = state {
        let _ = RENDER.set(s.clone());
    }
}

/// The graphics card to decode on so frames can be shared with the renderer:
/// its LUID, if the renderer runs on D3D12.
pub fn share_luid() -> Option<u64> {
    let rs = RENDER.get()?;
    if rs.adapter.get_info().backend != wgpu::Backend::Dx12 {
        return None;
    }
    unsafe {
        let device = rs.device.as_hal::<wgpu::hal::api::Dx12>()?;
        let luid = device.raw_device().GetAdapterLuid();
        Some((luid.HighPart as u32 as u64) << 32 | luid.LowPart as u64)
    }
}

/// A decoded frame ready to draw.
#[derive(Clone)]
pub struct Shown {
    key: (usize, usize),
    view: wgpu::TextureView,
}

impl Shown {
    /// Draw it into `rect` (with the high-quality filter).
    pub fn paint(&self, ui: &egui::Ui, rect: egui::Rect) {
        ui.painter().add(egui_wgpu::Callback::new_paint_callback(rect, Draw(self.clone())));
    }
}

/// The pool textures opened so far. Freed when dropped.
#[derive(Default)]
pub struct Frames {
    opened: HashMap<(usize, usize), (wgpu::Texture, wgpu::TextureView)>,
    /// Each pool's shared fence, opened on D3D12.
    fences: HashMap<usize, ID3D12Fence>,
    /// Pools seen, oldest first (a decoder makes a new one if reopened).
    pools: Vec<usize>,
}

impl Frames {
    /// `s`, ready to draw (opened the first time it's seen). Call it each
    /// time a new frame goes on screen: the next draw waits until the decoder
    /// has finished writing it (else D3D12 could draw what the texture held
    /// before: another, older frame).
    pub fn show(&mut self, s: &Surface, width: u32, height: u32) -> Option<Shown> {
        let rs = RENDER.get()?;
        self.forget_old_pools(rs, s.key().0);
        self.wait_for(rs, s);
        if let Some((_, view)) = self.opened.get(&s.key()) {
            return Some(Shown { key: s.key(), view: view.clone() });
        }
        let texture = unsafe { open(rs, s.handle(), width, height) }.inspect_err(|e| eprintln!("sharing a frame failed: {e}")).ok()?;
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        self.opened.insert(s.key(), (texture, view.clone()));
        Some(Shown { key: s.key(), view })
    }

    /// Keep the current pool's textures and the one before it (its frames can
    /// still be on screen for a moment); free older ones.
    fn forget_old_pools(&mut self, rs: &RenderState, pool: usize) {
        if self.pools.contains(&pool) {
            return;
        }
        self.pools.push(pool);
        let _ = rs;
        while self.pools.len() > 2 {
            let old = self.pools.remove(0);
            self.fences.remove(&old);
            // Dropped, not destroyed: a frame drawn this pass may still use
            // one; wgpu frees each once nothing does.
            self.opened.retain(|(p, _), _| *p != old);
        }
    }

    /// Have the renderer's next submit wait on GPU for `s` to be written.
    fn wait_for(&mut self, rs: &RenderState, s: &Surface) {
        let Some((handle, value)) = s.fence() else { return };
        let pool = s.key().0;
        if !self.fences.contains_key(&pool) {
            let opened = unsafe {
                rs.device.as_hal::<wgpu::hal::api::Dx12>().and_then(|d| {
                    let mut fence: Option<ID3D12Fence> = None;
                    d.raw_device().OpenSharedHandle(HANDLE(handle as *mut _), &mut fence).inspect_err(|e| eprintln!("sharing the decoder's fence failed: {}", e.message())).ok()?;
                    fence
                })
            };
            let Some(f) = opened else { return };
            self.fences.insert(pool, f);
        }
        let fence = self.fences[&pool].clone();
        unsafe {
            if let Some(q) = rs.queue.as_hal::<wgpu::hal::api::Dx12>() {
                q.add_wait_fence(fence, value);
            }
        }
    }
}

impl Drop for Frames {
    /// Forget this player's bind groups so its textures are freed now, not
    /// when the next clip draws. Nothing is destroyed outright: a frame drawn
    /// this pass makes its own again if it still needs one.
    fn drop(&mut self) {
        if let Some(rs) = RENDER.get() {
            if let Some(scaler) = rs.renderer.write().callback_resources.get_mut::<Scaler>() {
                scaler.groups.retain(|(p, _), _| !self.pools.contains(p));
            }
        }
    }
}

/// Open a shared D3D11 texture (NT handle) as a wgpu texture on D3D12.
unsafe fn open(rs: &RenderState, handle: isize, width: u32, height: u32) -> Result<wgpu::Texture, String> {
    let size = wgpu::Extent3d { width, height, depth_or_array_layers: 1 };
    let hal_texture = unsafe {
        let device = rs.device.as_hal::<wgpu::hal::api::Dx12>().ok_or("the renderer isn't on D3D12")?;
        let mut resource: Option<ID3D12Resource> = None;
        device.raw_device().OpenSharedHandle(HANDLE(handle as *mut _), &mut resource).map_err(|e| e.message())?;
        let resource = resource.ok_or("no resource")?;
        wgpu::hal::dx12::Device::texture_from_raw(resource, wgpu::TextureFormat::Rgba8Unorm, wgpu::TextureDimension::D2, size, 1, 1)
    };
    let desc = wgpu::TextureDescriptor {
        label: Some("decoded frame"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    };
    // Opened shared textures start in the common state, which D3D12 promotes
    // to "shader resource" on first read: all the renderer ever does with it.
    Ok(unsafe { rs.device.create_texture_from_hal::<wgpu::hal::api::Dx12>(hal_texture, &desc, wgpu::wgt::TextureUses::RESOURCE) })
}

/// The filter's pipeline and a bind group per frame texture, kept in egui's
/// callback resources. Bind groups not drawn for a while are dropped (they
/// hold their textures: a closed clip's frames are freed with them).
struct Scaler {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    groups: HashMap<(usize, usize), (wgpu::BindGroup, std::time::Instant)>,
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
            let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("decoded frame"),
                layout: &scaler.layout,
                entries: &[wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&self.0.view) }],
            });
            scaler.groups.insert(self.0.key, (group, now));
        }
        Vec::new()
    }

    fn paint(&self, _info: egui::PaintCallbackInfo, pass: &mut wgpu::RenderPass<'static>, resources: &CallbackResources) {
        let Some(scaler) = resources.get::<Scaler>() else { return };
        let Some((group, _)) = scaler.groups.get(&self.0.key) else { return };
        pass.set_pipeline(&scaler.pipeline);
        pass.set_bind_group(0, group, &[]);
        pass.draw(0..3, 0..1);
    }
}

impl Scaler {
    fn new(device: &wgpu::Device, target: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("frame filter"),
            source: wgpu::ShaderSource::Wgsl(SHADER.replace("SRGB_TARGET", if target.is_srgb() { "true" } else { "false" }).into()),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("frame filter"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            }],
        });
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
        Self { pipeline, layout, groups: HashMap::new() }
    }
}

/// Lanczos-3 resampling. One full-viewport triangle (egui sets the viewport
/// to the callback's rect); each output pixel weighs every source pixel within
/// 3 lobes, the lobes stretched by the reduction so a smaller picture is
/// filtered rather than skipped through.
const SHADER: &str = r#"
@group(0) @binding(0) var frame: texture_2d<f32>;

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

@fragment
fn fs(in: Out) -> @location(0) vec4<f32> {
    let size = vec2<f32>(textureDimensions(frame));
    let last = vec2<i32>(textureDimensions(frame)) - vec2<i32>(1);
    // Source pixels per screen pixel: above 1 when shown smaller.
    let step = vec2<f32>(abs(dpdx(in.uv.x)), abs(dpdy(in.uv.y))) * size;
    let s = max(step, vec2<f32>(1.0));
    let src = in.uv * size - 0.5;
    let r = min(3.0 * s, vec2<f32>(12.0));
    let lo = vec2<i32>(floor(src - r)) + vec2<i32>(1);
    let n = min(vec2<i32>(floor(src + r)) - lo + vec2<i32>(1), vec2<i32>(25));
    // Weights per column and per row once (the filter is separable), then
    // every source pixel in the window is just a load and a multiply.
    var wx: array<f32, 25>;
    var wy: array<f32, 25>;
    var tx = 0.0;
    var ty = 0.0;
    for (var i = 0; i < n.x; i++) {
        wx[i] = lanczos3((f32(lo.x + i) - src.x) / s.x);
        tx += wx[i];
    }
    for (var j = 0; j < n.y; j++) {
        wy[j] = lanczos3((f32(lo.y + j) - src.y) / s.y);
        ty += wy[j];
    }
    var sum = vec3<f32>(0.0);
    for (var j = 0; j < n.y; j++) {
        let y = clamp(lo.y + j, 0, last.y);
        var row = vec3<f32>(0.0);
        for (var i = 0; i < n.x; i++) {
            row += textureLoad(frame, vec2<i32>(clamp(lo.x + i, 0, last.x), y), 0).rgb * wx[i];
        }
        sum += row * wy[j];
    }
    let total = tx * ty;
    var c = clamp(sum / total, vec3<f32>(0.0), vec3<f32>(1.0));
    if SRGB_TARGET { c = to_linear(c); }
    return vec4<f32>(c, 1.0);
}
"#;
