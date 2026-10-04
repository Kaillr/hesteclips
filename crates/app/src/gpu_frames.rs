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

/// The pool textures opened so far, as egui textures. Freed when dropped.
#[derive(Default)]
pub struct Frames {
    opened: HashMap<(usize, usize), (wgpu::Texture, egui::TextureId)>,
    /// Each pool's shared fence, opened on D3D12.
    fences: HashMap<usize, ID3D12Fence>,
    /// Pools seen, oldest first: a new one comes with each change of size.
    pools: Vec<usize>,
}

impl Frames {
    /// The egui texture showing `s` (opened the first time it's seen). Call
    /// it each time a new frame goes on screen: the next draw waits until the
    /// decoder has finished writing it (else D3D12 could draw what the texture
    /// held before: another, older frame).
    pub fn texture(&mut self, s: &Surface, width: u32, height: u32) -> Option<egui::TextureId> {
        let rs = RENDER.get()?;
        self.forget_old_pools(rs, s.key().0);
        self.wait_for(rs, s);
        if let Some((_, id)) = self.opened.get(&s.key()) {
            return Some(*id);
        }
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

impl Frames {
    /// Keep the current pool's textures and the one before it (its frames can
    /// still be on screen for a moment); free older ones.
    fn forget_old_pools(&mut self, rs: &RenderState, pool: usize) {
        if self.pools.contains(&pool) {
            return;
        }
        self.pools.push(pool);
        while self.pools.len() > 2 {
            let old = self.pools.remove(0);
            self.fences.remove(&old);
            let mut renderer = rs.renderer.write();
            self.opened.retain(|(p, _), (texture, id)| {
                if *p == old {
                    renderer.free_texture(id);
                    texture.destroy();
                }
                *p != old
            });
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
