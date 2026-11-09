//! Frames decoded through VA-API (AMD's and Intel's cards, `super::decode`)
//! copied on the card into the app's buffers: a frame's two planes (the
//! luma, the U, V pairs) come as DMA-BUFs, the decoder's own memory, which
//! Vulkan takes in as images and copies from into a buffer of the device
//! the frame is for (the renderer's, or ours). Nothing comes to the CPU.
//!
//! The decoder decodes into a few surfaces in turn: each plane is taken in
//! once ([`Imports`]) and copied from again whenever a frame is decoded
//! into it.

use std::collections::HashMap;
use std::ffi::c_int;
use std::os::fd::{AsRawFd, BorrowedFd, IntoRawFd};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use ash::vk;
use wgpu::hal::api::Vulkan;

/// A plane of a decoded frame, as `hcav_frame_dmabuf` hands it over: the
/// DMA-BUF (borrowed: the decoder's until it lets go of the frame), its
/// size, its layout (a DRM format modifier), and where the plane is in it.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub(crate) struct Plane {
    pub fd: c_int,
    pub size: u64,
    pub modifier: u64,
    pub offset: u64,
    pub pitch: u64,
}

/// What a Vulkan device takes to copy from DMA-BUFs.
pub(crate) struct Importer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    instance: ash::Instance,
    physical: vk::PhysicalDevice,
    raw: ash::Device,
    external_fd: ash::khr::external_memory_fd::Device,
    /// The device's queue family, and the one the planes come from: another
    /// driver's ("foreign" where the device has that, else "external").
    family: u32,
    foreign: u32,
    /// Formats and layouts asked about: whether Vulkan copies from them.
    copyable: Mutex<HashMap<(vk::Format, u64), bool>>,
}

impl Importer {
    /// For `device` (submitted to by `queue`): where its Vulkan takes in
    /// DMA-BUFs in the layouts drivers lay them out in.
    pub(crate) fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> Result<Self> {
        // SAFETY: only read from; the handles stay valid while the device
        // (kept here) lives.
        let hal = unsafe { device.as_hal::<Vulkan>() }.context("not on Vulkan")?;
        let enabled = hal.enabled_device_extensions();
        for e in [ash::khr::external_memory_fd::NAME, ash::ext::external_memory_dma_buf::NAME, ash::ext::image_drm_format_modifier::NAME] {
            ensure!(enabled.contains(&e), "the card's Vulkan doesn't take in DMA-BUFs ({} missing)", e.to_string_lossy());
        }
        let foreign = if enabled.contains(&ash::ext::queue_family_foreign::NAME) { vk::QUEUE_FAMILY_FOREIGN_EXT } else { vk::QUEUE_FAMILY_EXTERNAL };
        let instance = hal.shared_instance().raw_instance().clone();
        let raw = hal.raw_device().clone();
        let external_fd = ash::khr::external_memory_fd::Device::new(&instance, &raw);
        Ok(Self {
            device: device.clone(),
            queue: queue.clone(),
            physical: hal.raw_physical_device(),
            family: hal.queue_family_index(),
            foreign,
            instance,
            raw,
            external_fd,
            copyable: Mutex::new(HashMap::new()),
        })
    }

    /// Whether Vulkan takes in a DMA-BUF of `format` laid out as
    /// `modifier`, as one plane, and copies from it.
    fn copies(&self, format: vk::Format, modifier: u64) -> bool {
        *self.copyable.lock().unwrap().entry((format, modifier)).or_insert_with(|| {
            let query = |list: &mut vk::DrmFormatModifierPropertiesListEXT| {
                let mut props = vk::FormatProperties2::default().push_next(list);
                // SAFETY: a valid physical device and output structs.
                unsafe { self.instance.get_physical_device_format_properties2(self.physical, format, &mut props) };
            };
            let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
            query(&mut list);
            let mut entries = vec![vk::DrmFormatModifierPropertiesEXT::default(); list.drm_format_modifier_count as usize];
            let mut list = vk::DrmFormatModifierPropertiesListEXT::default().drm_format_modifier_properties(&mut entries);
            query(&mut list);
            let listed = entries
                .iter()
                .any(|m| m.drm_format_modifier == modifier && m.drm_format_modifier_plane_count == 1 && m.drm_format_modifier_tiling_features.contains(vk::FormatFeatureFlags::TRANSFER_SRC));
            listed && self.importable(format, modifier)
        })
    }

    /// Whether an image of `format` in `modifier`'s layout can be imported
    /// from a DMA-BUF and copied from.
    fn importable(&self, format: vk::Format, modifier: u64) -> bool {
        let mut drm = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default().drm_format_modifier(modifier).sharing_mode(vk::SharingMode::EXCLUSIVE);
        let mut external = vk::PhysicalDeviceExternalImageFormatInfo::default().handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let info = vk::PhysicalDeviceImageFormatInfo2::default()
            .format(format)
            .ty(vk::ImageType::TYPE_2D)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::TRANSFER_SRC)
            .push_next(&mut external)
            .push_next(&mut drm);
        let mut external_props = vk::ExternalImageFormatProperties::default();
        let mut props = vk::ImageFormatProperties2::default().push_next(&mut external_props);
        // SAFETY: a valid physical device and structs.
        let ok = unsafe { self.instance.get_physical_device_image_format_properties2(self.physical, &info, &mut props) }.is_ok();
        ok && external_props.external_memory_properties.external_memory_features.contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE)
    }

    /// `plane`'s DMA-BUF taken in as an image of `format`, `size` big (the
    /// fd is borrowed; Vulkan gets its own).
    fn import(&self, plane: &Plane, format: vk::Format, (w, h): (u32, u32)) -> Result<(vk::Image, vk::DeviceMemory)> {
        ensure!(self.copies(format, plane.modifier), "the card's Vulkan can't copy from the decoder's frames (format {format:?}, layout {:#x})", plane.modifier);
        let layouts = [vk::SubresourceLayout { offset: plane.offset, size: 0, row_pitch: plane.pitch, array_pitch: 0, depth_pitch: 0 }];
        let mut explicit = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default().drm_format_modifier(plane.modifier).plane_layouts(&layouts);
        let mut external = vk::ExternalMemoryImageCreateInfo::default().handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D { width: w, height: h, depth: 1 })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut external)
            .push_next(&mut explicit);
        let raw = &self.raw;
        // SAFETY: a valid create info; the image is destroyed on failure
        // below, else by `Imports`.
        let image = unsafe { raw.create_image(&info, None) }.context("Vulkan won't make an image of the decoder's frame")?;
        let memory = (|| -> Result<vk::DeviceMemory> {
            // SAFETY: the fd is valid while the decoder holds the frame.
            let fd = unsafe { BorrowedFd::borrow_raw(plane.fd) };
            let dup = fd.try_clone_to_owned().context("can't duplicate the frame's file descriptor")?;
            let mut fd_props = vk::MemoryFdPropertiesKHR::default();
            // SAFETY: a DMA-BUF fd and a struct to fill.
            unsafe { self.external_fd.get_memory_fd_properties(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT, dup.as_raw_fd(), &mut fd_props) }
                .context("Vulkan can't read the frame's memory")?;
            // SAFETY: a valid image.
            let req = unsafe { raw.get_image_memory_requirements(image) };
            ensure!(req.size <= plane.size, "the frame's plane is bigger than its memory");
            let type_index = (req.memory_type_bits & fd_props.memory_type_bits).trailing_zeros();
            ensure!(type_index < 32, "no memory type fits the frame");
            let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
            let mut import = vk::ImportMemoryFdInfoKHR::default().handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT).fd(dup.as_raw_fd());
            let alloc = vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(type_index).push_next(&mut import).push_next(&mut dedicated);
            // SAFETY: a valid allocate info importing the fd.
            let memory = unsafe { raw.allocate_memory(&alloc, None) }.context("Vulkan won't take the frame's memory")?;
            // Vulkan owns the fd now.
            let _ = dup.into_raw_fd();
            // SAFETY: fresh memory made for this image.
            if let Err(e) = unsafe { raw.bind_image_memory(image, memory, 0) } {
                // SAFETY: unused memory of ours.
                unsafe { raw.free_memory(memory, None) };
                bail!("Vulkan won't bind the frame's memory: {e}");
            }
            Ok(memory)
        })();
        match memory {
            Ok(m) => Ok((image, m)),
            Err(e) => {
                // SAFETY: an image of ours, unused.
                unsafe { raw.destroy_image(image, None) };
                Err(e)
            }
        }
    }
}

/// A plane taken in, and whether it's been copied from (then it was given
/// back in the general layout).
struct Imported {
    image: vk::Image,
    memory: vk::DeviceMemory,
    used: bool,
    /// When it was last copied from (`Imports::clock`).
    last: u64,
}

/// A decoder's planes taken in, by DMA-BUF (its inode: the same buffer
/// handed over again has the same one) and where the plane is in it.
pub(crate) struct Imports {
    importer: Arc<Importer>,
    planes: HashMap<(u64, u64, u64), Imported>,
    clock: u64,
}

/// Planes kept taken in, at most: a decoder's surfaces (up to ~20 for
/// H.264 with frame threads) twice over, with room.
const KEPT: usize = 64;

impl Imports {
    pub(crate) fn new(importer: Arc<Importer>) -> Self {
        Self { importer, planes: HashMap::new(), clock: 0 }
    }

    /// Copy a frame (`size`, its `planes`: the luma, R8; the U, V pairs,
    /// two bytes each) into `dst`: the luma's rows `pitch` bytes apart, then
    /// the pairs' rows from `chroma_at`. Done when this returns.
    pub(crate) fn copy(&mut self, planes: &[Plane; 2], (w, h): (u32, u32), dst: &wgpu::Buffer, pitch: u32, chroma_at: u64) -> Result<()> {
        let sizes = [(w, h), (w.div_ceil(2), h.div_ceil(2))];
        let formats = [vk::Format::R8_UNORM, vk::Format::R8G8_UNORM];
        self.clock += 1;
        let mut keys = [(0, 0, 0); 2];
        for i in 0..2 {
            let p = &planes[i];
            // SAFETY: an output struct; the fd is valid while the decoder
            // holds the frame.
            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            ensure!(unsafe { libc::fstat(p.fd, &mut st) } == 0, "the frame's DMA-BUF is gone");
            let key = (st.st_ino, p.offset, p.pitch);
            if !self.planes.contains_key(&key) {
                self.forget_oldest();
                let (image, memory) = self.importer.import(p, formats[i], sizes[i])?;
                self.planes.insert(key, Imported { image, memory, used: false, last: 0 });
            }
            keys[i] = key;
        }
        let images = keys.map(|k| self.planes[&k].image);
        let used = keys.map(|k| self.planes[&k].used);
        let im = &self.importer;
        // SAFETY: only its handle is taken.
        let dst_raw = unsafe { dst.as_hal::<Vulkan>() }.map(|b| unsafe { b.raw_handle() }).context("not a Vulkan buffer")?;
        let mut enc = im.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("VA-API frame") });
        // SAFETY: recorded into the encoder's own command buffer, which is
        // only ended by `finish`; the images and buffer outlive the copy
        // (waited for below).
        let recorded = unsafe {
            enc.as_hal_mut::<Vulkan, _, _>(|e| {
                let cb = e.context("not a Vulkan command encoder")?.raw_handle();
                let range = vk::ImageSubresourceRange { aspect_mask: vk::ImageAspectFlags::COLOR, base_mip_level: 0, level_count: 1, base_array_layer: 0, layer_count: 1 };
                // The decoder's side hands each plane over (from its own
                // queue: nothing to wait for, it's finished decoding it);
                // the first time in no layout of ours yet, then in the
                // general one, which it went back in.
                let acquire = |i: usize| {
                    vk::ImageMemoryBarrier::default()
                        .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
                        .old_layout(if used[i] { vk::ImageLayout::GENERAL } else { vk::ImageLayout::UNDEFINED })
                        .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                        .src_queue_family_index(im.foreign)
                        .dst_queue_family_index(im.family)
                        .image(images[i])
                        .subresource_range(range)
                };
                let release = |i: usize| {
                    vk::ImageMemoryBarrier::default()
                        .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                        .new_layout(vk::ImageLayout::GENERAL)
                        .src_queue_family_index(im.family)
                        .dst_queue_family_index(im.foreign)
                        .image(images[i])
                        .subresource_range(range)
                };
                // What came before with the buffer (the last frame in it
                // read, wgpu clearing it) is done first.
                let before = [vk::MemoryBarrier::default().src_access_mask(vk::AccessFlags::MEMORY_WRITE).dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)];
                im.raw.cmd_pipeline_barrier(cb, vk::PipelineStageFlags::ALL_COMMANDS, vk::PipelineStageFlags::TRANSFER, vk::DependencyFlags::empty(), &before, &[], &[acquire(0), acquire(1)]);
                let region = |i: usize, offset: u64, texel: u32| vk::BufferImageCopy {
                    buffer_offset: offset,
                    buffer_row_length: pitch / texel,
                    buffer_image_height: 0,
                    image_subresource: vk::ImageSubresourceLayers { aspect_mask: vk::ImageAspectFlags::COLOR, mip_level: 0, base_array_layer: 0, layer_count: 1 },
                    image_offset: vk::Offset3D::default(),
                    image_extent: vk::Extent3D { width: sizes[i].0, height: sizes[i].1, depth: 1 },
                };
                im.raw.cmd_copy_image_to_buffer(cb, images[0], vk::ImageLayout::TRANSFER_SRC_OPTIMAL, dst_raw, &[region(0, 0, 1)]);
                im.raw.cmd_copy_image_to_buffer(cb, images[1], vk::ImageLayout::TRANSFER_SRC_OPTIMAL, dst_raw, &[region(1, chroma_at, 2)]);
                // The frame is there for whatever reads the buffer next;
                // the planes go back to the decoder's side.
                let after = [vk::MemoryBarrier::default().src_access_mask(vk::AccessFlags::TRANSFER_WRITE).dst_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE)];
                im.raw.cmd_pipeline_barrier(cb, vk::PipelineStageFlags::TRANSFER, vk::PipelineStageFlags::ALL_COMMANDS, vk::DependencyFlags::empty(), &after, &[], &[release(0), release(1)]);
                Ok::<(), anyhow::Error>(())
            })
        };
        recorded?;
        let index = im.queue.submit([enc.finish()]);
        // The decoder decodes into the planes again once it has the frame
        // back: the copy is waited for.
        im.device
            .poll(wgpu::PollType::Wait { submission_index: Some(index), timeout: Some(Duration::from_secs(5)) })
            .map_err(|e| anyhow!("the card didn't copy the frame: {e}"))?;
        for k in keys {
            let p = self.planes.get_mut(&k).expect("just taken in");
            p.used = true;
            p.last = self.clock;
        }
        Ok(())
    }

    /// Make room for another plane: the one copied from longest ago goes.
    fn forget_oldest(&mut self) {
        if self.planes.len() < KEPT {
            return;
        }
        if let Some(k) = self.planes.iter().min_by_key(|(_, p)| p.last).map(|(k, _)| *k) {
            let p = self.planes.remove(&k).expect("just found");
            // SAFETY: ours, and no copy from it is running (each is waited for).
            unsafe {
                self.importer.raw.destroy_image(p.image, None);
                self.importer.raw.free_memory(p.memory, None);
            }
        }
    }
}

impl Drop for Imports {
    fn drop(&mut self) {
        for p in self.planes.values() {
            // SAFETY: ours, and no copy from it is running.
            unsafe {
                self.importer.raw.destroy_image(p.image, None);
                self.importer.raw.free_memory(p.memory, None);
            }
        }
    }
}

/// The DRM render node of `device`'s card (`/dev/dri/renderD128`, say), for
/// VA-API to decode on the same card.
pub(crate) fn render_node(device: &wgpu::Device) -> Result<PathBuf> {
    // SAFETY: only read from.
    let hal = unsafe { device.as_hal::<Vulkan>() }.context("not on Vulkan")?;
    let (instance, physical) = (hal.shared_instance().raw_instance(), hal.raw_physical_device());
    // SAFETY: a valid physical device.
    let extensions = unsafe { instance.enumerate_device_extension_properties(physical) }.unwrap_or_default();
    let has_drm = extensions.iter().any(|e| e.extension_name_as_c_str() == Ok(ash::ext::physical_device_drm::NAME));
    if has_drm {
        let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
        let mut props = vk::PhysicalDeviceProperties2::default().push_next(&mut drm);
        // SAFETY: a valid physical device and output structs.
        unsafe { instance.get_physical_device_properties2(physical, &mut props) };
        if drm.has_render == vk::TRUE {
            let want = libc::makedev(drm.render_major as u32, drm.render_minor as u32);
            let named = PathBuf::from(format!("/dev/dri/renderD{}", drm.render_minor));
            if std::fs::metadata(&named).is_ok_and(|m| m.rdev() == want) {
                return Ok(named);
            }
            let found = std::fs::read_dir("/dev/dri").into_iter().flatten().flatten().map(|e| e.path()).find(|p| std::fs::metadata(p).is_ok_and(|m| m.rdev() == want));
            return found.context("the card's render node isn't in /dev/dri");
        }
    }
    // A driver that doesn't say: the first card's, as on most computers
    // (a frame from another card fails the check, `Card::check`).
    let first = PathBuf::from("/dev/dri/renderD128");
    ensure!(first.exists(), "no render node in /dev/dri");
    Ok(first)
}
