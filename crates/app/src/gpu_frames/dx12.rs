//! Decoded frames from D3D11 shared textures, opened in wgpu's D3D12 device.

use std::collections::HashMap;

use capture::decode::Surface;
use eframe::egui_wgpu::RenderState;
use eframe::wgpu;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D12::{ID3D12Fence, ID3D12Resource};

use super::{Planes, RENDER, Shown};

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
            return Some(Shown { key: s.key(), planes: Planes::Rgba(view.clone()) });
        }
        let texture = unsafe { open(rs, s.handle(), width, height) }.inspect_err(|e| eprintln!("sharing a frame failed: {e}")).ok()?;
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        self.opened.insert(s.key(), (texture, view.clone()));
        Some(Shown { key: s.key(), planes: Planes::Rgba(view) })
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
    fn drop(&mut self) {
        super::forget_pools(&self.pools);
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
