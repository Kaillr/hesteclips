//! Decoded frames from IOSurfaces (VideoToolbox's NV12 buffers): each plane
//! opened as a Metal texture over the same memory, once per buffer. The
//! decoder reuses a fixed set of buffers, so after the first few frames
//! nothing new is opened at all.
//!
//! No fence is needed: VideoToolbox hands over a buffer only once it's
//! written, and the player holds it (and so keeps the decoder from reusing
//! it) until the GPU is done drawing it.

use std::collections::HashMap;

use capture::decode::Surface;
use eframe::wgpu;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_io_surface::IOSurfaceRef;
use objc2_metal::{MTLDevice, MTLPixelFormat, MTLStorageMode, MTLTexture, MTLTextureDescriptor, MTLTextureType, MTLTextureUsage};

use super::{Planes, RENDER, Shown};

/// Whether frames can be drawn from the GPU: the renderer runs on Metal.
pub fn available() -> bool {
    RENDER.get().is_some_and(|rs| rs.adapter.get_info().backend == wgpu::Backend::Metal)
}

/// The buffers opened so far, by IOSurface id. Freed when dropped.
#[derive(Default)]
pub struct Frames {
    opened: HashMap<u32, (wgpu::TextureView, wgpu::TextureView)>,
    /// A unique number per player, so its bind groups can be told apart
    /// from another player's (IOSurface ids are unique system-wide anyway).
    pool: usize,
}

impl Frames {
    /// `s`, ready to draw (its planes opened the first time it's seen).
    pub fn show(&mut self, s: &Surface, width: u32, height: u32) -> Option<Shown> {
        let rs = RENDER.get()?;
        if self.pool == 0 {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);
            self.pool = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let surface = s.io_surface()?;
        let id = surface.id();
        if !self.opened.contains_key(&id) {
            // A buffer pool that grew past what's useful (the decoder replaced
            // its pool): start over rather than hold old buffers open.
            if self.opened.len() > 64 {
                self.opened.clear();
            }
            let open = |plane: usize, format: wgpu::TextureFormat, w: u32, h: u32| unsafe { open(&rs.device, &surface, plane, format, w, h) };
            let y = open(0, wgpu::TextureFormat::R8Unorm, width, height).inspect_err(|e| eprintln!("sharing a frame failed: {e}")).ok()?;
            let cbcr = open(1, wgpu::TextureFormat::Rg8Unorm, width.div_ceil(2), height.div_ceil(2)).inspect_err(|e| eprintln!("sharing a frame failed: {e}")).ok()?;
            let views = (y.create_view(&Default::default()), cbcr.create_view(&Default::default()));
            self.opened.insert(id, views);
        }
        let (y, cbcr) = self.opened[&id].clone();
        Some(Shown { key: (self.pool, id as usize), planes: Planes::Nv12 { y, cbcr, bt601: s.bt601 } })
    }
}

impl Drop for Frames {
    fn drop(&mut self) {
        super::forget_pools(&[self.pool]);
    }
}

/// Plane `plane` of `surface` as a wgpu texture (`w`×`h`, `format`), sharing
/// its memory.
pub(super) unsafe fn open(device: &wgpu::Device, surface: &IOSurfaceRef, plane: usize, format: wgpu::TextureFormat, w: u32, h: u32) -> Result<wgpu::Texture, String> {
    let mtl_format = match format {
        wgpu::TextureFormat::R8Unorm => MTLPixelFormat::R8Unorm,
        wgpu::TextureFormat::Rg8Unorm => MTLPixelFormat::RG8Unorm,
        _ => return Err("unsupported plane format".into()),
    };
    let raw: Retained<ProtocolObject<dyn MTLTexture>> = unsafe {
        let hal = device.as_hal::<wgpu::hal::api::Metal>().ok_or("the renderer isn't on Metal")?;
        let desc = MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(mtl_format, w as usize, h as usize, false);
        desc.setUsage(MTLTextureUsage::ShaderRead);
        desc.setStorageMode(MTLStorageMode::Shared);
        hal.raw_device().newTextureWithDescriptor_iosurface_plane(&desc, surface, plane).ok_or("Metal couldn't open the frame")?
    };
    let size = wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 };
    let hal_texture = unsafe {
        wgpu::hal::metal::Device::texture_from_raw(raw, format, MTLTextureType::Type2D, 1, 1, wgpu::hal::CopyExtent { width: w, height: h, depth: 1 }, None)
    };
    let desc = wgpu::TextureDescriptor {
        label: Some("decoded frame"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    };
    Ok(unsafe { device.create_texture_from_hal::<wgpu::hal::api::Metal>(hal_texture, &desc, wgpu::wgt::TextureUses::RESOURCE) })
}
