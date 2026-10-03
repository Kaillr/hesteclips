//! Decoded frames drawn straight from the GPU (Windows): the decoder renders
//! each frame into one of a pool of shareable D3D11 textures
//! (`capture::win::decode::Surfaces`); here each is opened once in wgpu's
//! D3D12 device and registered with egui, so showing a frame is choosing its
//! texture. Nothing is copied back to the CPU or uploaded again (that was a
//! ~3.7 MB round trip per 1280x720 frame).
//!
//! Needs wgpu on its DX12 backend (asked for at startup) and the decoder on
//! the same graphics card; otherwise frames come back as RGBA as before.

use std::collections::HashMap;
use std::sync::OnceLock;

use capture::win::decode::Surface;
use eframe::egui_wgpu::RenderState;
use eframe::wgpu;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D12::ID3D12Resource;

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

/// The pool textures opened so far, as egui textures. Freed when dropped.
#[derive(Default)]
pub struct Frames {
    opened: HashMap<(usize, usize), (wgpu::Texture, egui::TextureId)>,
}

impl Frames {
    /// The egui texture showing `s` (opened the first time it's seen).
    pub fn texture(&mut self, s: &Surface, width: u32, height: u32) -> Option<egui::TextureId> {
        if let Some((_, id)) = self.opened.get(&s.key()) {
            return Some(*id);
        }
        let rs = RENDER.get()?;
        let started = std::time::Instant::now();
        let texture = unsafe { open(rs, s.handle(), width, height) }.inspect_err(|e| eprintln!("sharing a frame failed: {e}")).ok()?;
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let id = rs.renderer.write().register_native_texture(&rs.device, &view, wgpu::FilterMode::Linear);
        self.opened.insert(s.key(), (texture, id));
        if std::env::var_os("HESTECLIPS_DEBUG_VIDEO").is_some() {
            eprintln!("{:>8.3} gpu frames: opened texture {:?} in {:.1} ms ({} open)", crate::player::uptime(), s.key(), started.elapsed().as_secs_f64() * 1000.0, self.opened.len());
        }
        Some(id)
    }
}

impl Drop for Frames {
    fn drop(&mut self) {
        if let Some(rs) = RENDER.get() {
            let mut renderer = rs.renderer.write();
            for (_, (texture, id)) in self.opened.drain() {
                renderer.free_texture(&id);
                texture.destroy();
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
