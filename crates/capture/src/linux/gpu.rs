//! The graphics card, for pictures that arrive on it: KWin (and Mutter) can
//! hand over each picture as a DMA-BUF, the card's own memory, instead of
//! copying it into ours. Imported into Vulkan here, through wgpu, where it's
//! scaled, blended and converted to NV12 for the encoder (`super::gpu_frame`)
//! without the picture ever coming down to the CPU; only the finished NV12
//! frame does.
//!
//! One device for the app, made the first time it's asked for. `None` where
//! Vulkan can't import DMA-BUFs (no Vulkan, an old driver): pictures then
//! come in our memory and are worked on the CPU (`super::image`).

use std::ffi::CStr;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result, anyhow, bail, ensure};
use ash::vk;
use wgpu::hal::api::Vulkan;

/// The extensions importing a DMA-BUF takes.
const EXTENSIONS: [&CStr; 4] = [
    ash::khr::external_memory_fd::NAME,
    ash::ext::external_memory_dma_buf::NAME,
    ash::ext::image_drm_format_modifier::NAME,
    ash::ext::queue_family_foreign::NAME,
];

/// What encoding H.264 through Vulkan Video takes (`super::vkenc`).
const VIDEO_EXTENSIONS: [&CStr; 3] = [ash::khr::video_queue::NAME, ash::khr::video_encode_queue::NAME, ash::khr::video_encode_h264::NAME];

/// The DRM fourcc codes of the formats asked for, as PipeWire's video formats.
pub(crate) const DRM_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");
pub(crate) const DRM_ARGB8888: u32 = u32::from_le_bytes(*b"AR24");
pub(crate) const DRM_XBGR8888: u32 = u32::from_le_bytes(*b"XB24");
pub(crate) const DRM_ABGR8888: u32 = u32::from_le_bytes(*b"AB24");

/// The card, with what it can import.
pub(crate) struct Gpu {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    /// The layouts (DRM format modifiers) a 4-byte BGRA/RGBA picture can be
    /// imported in: what we offer the compositor.
    pub modifiers: Vec<u64>,
    /// The card's UUID, which CUDA knows it by too (`super::nvenc`).
    pub uuid: [u8; 16],
    /// Where the card encodes H.264 through Vulkan Video (`super::vkenc`).
    pub video: Option<VideoQueue>,
    external_fd: ash::khr::external_memory_fd::Device,
}

/// The card's video encoding queue: ours alone (wgpu has its own queue).
pub(crate) struct VideoQueue {
    pub family: u32,
    /// Taken while submitting to it.
    pub queue: Mutex<vk::Queue>,
}

/// The card, if pictures can be taken from it.
pub(crate) fn gpu() -> Option<Arc<Gpu>> {
    static GPU: OnceLock<Option<Arc<Gpu>>> = OnceLock::new();
    GPU.get_or_init(|| match Gpu::new() {
        Ok(g) => Some(Arc::new(g)),
        Err(e) => {
            eprintln!("graphics card capture: {e:#}; pictures are worked on the CPU");
            None
        }
    })
    .clone()
}

impl Gpu {
    fn new() -> Result<Self> {
        if std::env::var_os("HESTECLIPS_NO_GPU_CAPTURE").is_some() {
            bail!("turned off by HESTECLIPS_NO_GPU_CAPTURE");
        }
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor { backends: wgpu::Backends::VULKAN, ..wgpu::InstanceDescriptor::new_without_display_handle() });
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            ..Default::default()
        }))
        .context("no Vulkan graphics card")?;
        let (open, modifiers, uuid, video) = {
            // SAFETY: the adapter is Vulkan (the only backend asked for).
            let hal = unsafe { adapter.as_hal::<Vulkan>() }.context("not a Vulkan adapter")?;
            let caps = hal.physical_device_capabilities();
            if let Some(missing) = EXTENSIONS.iter().find(|e| !caps.supports_extension(e)) {
                bail!("the driver can't import DMA-BUFs ({} missing)", missing.to_string_lossy());
            }
            let (instance, physical) = (hal.shared_instance().raw_instance(), hal.raw_physical_device());
            let modifiers = importable_modifiers(instance, physical);
            let uuid = {
                let mut id = vk::PhysicalDeviceIDProperties::default();
                let mut props = vk::PhysicalDeviceProperties2::default().push_next(&mut id);
                // SAFETY: a valid physical device and output structs.
                unsafe { instance.get_physical_device_properties2(physical, &mut props) };
                id.device_uuid
            };
            if modifiers.is_empty() {
                bail!("the driver can't import a BGRA picture in any layout");
            }
            // A queue that encodes H.264, where Vulkan Video can, and wgpu
            // copies into the encoder's NV12 pictures (`super::vkenc`).
            let video = (VIDEO_EXTENSIONS.iter().all(|e| caps.supports_extension(e))
                && adapter.features().contains(wgpu::Features::TEXTURE_FORMAT_NV12)
                && has_synchronization2(instance, physical))
            .then(|| h264_encode_family(instance, physical))
            .flatten();
            let limits = adapter.limits();
            let open = |video: Option<u32>| {
                let features = if video.is_some() { wgpu::Features::TEXTURE_FORMAT_NV12 } else { wgpu::Features::empty() };
                // SAFETY: only extensions and queues the device has are added.
                unsafe {
                    hal.open_with_callback(
                        features,
                        &limits,
                        &wgpu::MemoryHints::Performance,
                        Some(Box::new(move |args: wgpu::hal::vulkan::CreateDeviceCallbackArgs| {
                            let video_extensions: &[&CStr] = if video.is_some() { &VIDEO_EXTENSIONS } else { &[] };
                            for &e in EXTENSIONS.iter().chain(video_extensions) {
                                if !args.extensions.contains(&e) {
                                    args.extensions.push(e);
                                }
                            }
                            if let Some(family) = video {
                                static PRIORITY: [f32; 1] = [1.0];
                                args.queue_create_infos.push(vk::DeviceQueueCreateInfo::default().queue_family_index(family).queue_priorities(&PRIORITY));
                                // The encoder's pictures are shared by wgpu's queue
                                // and the video queue, and wgpu's barriers on them
                                // name its own family: allowed with this. Made
                                // once, for the device's life.
                                let sync2 = Box::leak(Box::new(vk::PhysicalDeviceSynchronization2Features::default().synchronization2(true)));
                                *args.create_info = args.create_info.push_next(sync2);
                            }
                        })),
                    )
                }
                .map(|o| (o, features))
            };
            let (open, video) = match video.map(|f| (open(Some(f)), f)) {
                Some((Ok(o), f)) => (o, Some(f)),
                Some((Err(e), _)) => {
                    eprintln!("graphics card: can't open it with its video encoder ({e}); without it");
                    (open(None).map_err(|e| anyhow!("can't open the graphics card: {e}"))?, None)
                }
                None => (open(None).map_err(|e| anyhow!("can't open the graphics card: {e}"))?, None),
            };
            (open, modifiers, uuid, video)
        };
        let (open, features) = open;
        // SAFETY: `open` was made from this adapter, with these features.
        let (device, queue) = unsafe {
            adapter.create_device_from_hal(
                open,
                &wgpu::DeviceDescriptor { label: Some("hesteclips capture"), required_features: features, required_limits: adapter.limits(), ..Default::default() },
            )
        }
        .context("can't use the graphics card")?;
        let (external_fd, video) = {
            // SAFETY: the device is Vulkan.
            let hal = unsafe { device.as_hal::<Vulkan>() }.context("not a Vulkan device")?;
            let external_fd = ash::khr::external_memory_fd::Device::new(hal.shared_instance().raw_instance(), hal.raw_device());
            // SAFETY: the device was made with one queue in this family.
            let video = video.map(|family| VideoQueue { family, queue: Mutex::new(unsafe { hal.raw_device().get_device_queue(family, 0) }) });
            (external_fd, video)
        };
        Ok(Self { device, queue, modifiers, uuid, video, external_fd })
    }

    /// A storage buffer of `size` bytes whose memory CUDA can take in too
    /// (NVIDIA's encoder reads the frames made in it, `super::nvenc`): see
    /// [`shared_buffer`].
    pub(crate) fn shared_buffer(&self, size: u64) -> Result<(wgpu::Buffer, OwnedFd, u64)> {
        shared_buffer(&self.device, size)
    }

    /// Take in a DMA-BUF picture (one plane; `fd` is borrowed, and dup'ed for
    /// Vulkan) as a texture to read from. The compositor's buffers are a few,
    /// used in turn: import each once and keep it.
    pub(crate) fn import(&self, fd: BorrowedFd<'_>, width: u32, height: u32, fourcc: u32, modifier: u64, offset: u64, stride: u64) -> Result<wgpu::Texture> {
        let (vk_format, format) = match fourcc {
            DRM_XRGB8888 | DRM_ARGB8888 => (vk::Format::B8G8R8A8_UNORM, wgpu::TextureFormat::Bgra8Unorm),
            DRM_XBGR8888 | DRM_ABGR8888 => (vk::Format::R8G8B8A8_UNORM, wgpu::TextureFormat::Rgba8Unorm),
            _ => bail!("a picture format we don't take ({fourcc:#x})"),
        };
        // SAFETY: the device is Vulkan.
        let hal = unsafe { self.device.as_hal::<Vulkan>() }.context("not a Vulkan device")?;
        let raw = hal.raw_device();
        let planes = [vk::SubresourceLayout { offset, size: 0, row_pitch: stride, array_pitch: 0, depth_pitch: 0 }];
        let mut explicit = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default().drm_format_modifier(modifier).plane_layouts(&planes);
        let mut external = vk::ExternalMemoryImageCreateInfo::default().handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk_format)
            .extent(vk::Extent3D { width, height, depth: 1 })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut external)
            .push_next(&mut explicit);
        // SAFETY: a valid create info; the image is destroyed on failure below,
        // else owned by the texture.
        let image = unsafe { raw.create_image(&info, None) }.context("Vulkan won't make the picture's image")?;
        let memory = (|| -> Result<vk::DeviceMemory> {
            let dup = fd.try_clone_to_owned().context("can't duplicate the picture's file descriptor")?;
            let mut fd_props = vk::MemoryFdPropertiesKHR::default();
            // SAFETY: a DMA-BUF fd and a struct to fill.
            unsafe { self.external_fd.get_memory_fd_properties(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT, dup.as_raw_fd(), &mut fd_props) }
                .context("Vulkan can't read the picture's memory")?;
            // SAFETY: a valid image.
            let req = unsafe { raw.get_image_memory_requirements(image) };
            let type_index = (req.memory_type_bits & fd_props.memory_type_bits).trailing_zeros();
            if type_index >= 32 {
                bail!("no memory type fits the picture");
            }
            let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
            // Vulkan owns the fd once the allocation succeeds.
            let mut import = vk::ImportMemoryFdInfoKHR::default().handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT).fd(dup.as_raw_fd());
            let alloc = vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(type_index).push_next(&mut import).push_next(&mut dedicated);
            // SAFETY: a valid allocate info importing the fd.
            let memory = unsafe { raw.allocate_memory(&alloc, None) }.context("Vulkan won't take the picture's memory")?;
            let _ = dup.into_raw_fd();
            // SAFETY: fresh memory made for this image.
            if let Err(e) = unsafe { raw.bind_image_memory(image, memory, 0) } {
                // SAFETY: unused memory of ours.
                unsafe { raw.free_memory(memory, None) };
                bail!("Vulkan won't bind the picture's memory: {e}");
            }
            Ok(memory)
        })();
        let memory = match memory {
            Ok(m) => m,
            Err(e) => {
                // SAFETY: an image of ours, unused.
                unsafe { raw.destroy_image(image, None) };
                return Err(e);
            }
        };
        let size = wgpu::Extent3d { width, height, depth_or_array_layers: 1 };
        let usage = wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC;
        let desc = wgpu::TextureDescriptor { label: Some("dmabuf"), size, mip_level_count: 1, sample_count: 1, dimension: wgpu::TextureDimension::D2, format, usage, view_formats: &[] };
        let hal_desc = wgpu::hal::TextureDescriptor {
            label: Some("dmabuf"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUses::RESOURCE | wgpu::TextureUses::COPY_SRC,
            memory_flags: wgpu::hal::MemoryFlags::empty(),
            view_formats: vec![],
        };
        // SAFETY: the image and memory are ours, and handed to the texture,
        // which destroys them when dropped.
        let texture = unsafe { hal.texture_from_raw(image, &hal_desc, None, wgpu::hal::vulkan::TextureMemory::Dedicated(memory)) };
        drop(hal);
        // SAFETY: the texture was made on this device, in its undefined
        // starting layout.
        Ok(unsafe { self.device.create_texture_from_hal::<Vulkan>(texture, &desc, wgpu::TextureUses::UNINITIALIZED) })
    }
}

impl Gpu {
    /// A texture's pixels, 4 bytes each in its own order, rows packed: waits
    /// for the card (for screenshots and tests; slow next to a frame).
    pub(crate) fn read(&self, texture: &wgpu::Texture) -> Vec<u8> {
        let (w, h) = (texture.width(), texture.height());
        let row = w * 4;
        let padded = row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor { label: Some("read back"), size: padded as u64 * h as u64, usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false });
        let mut enc = self.device.create_command_encoder(&Default::default());
        enc.copy_texture_to_buffer(
            texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo { buffer: &buffer, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(padded), rows_per_image: Some(h) } },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        self.queue.submit([enc.finish()]);
        buffer.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
        let Ok(data) = buffer.slice(..).get_mapped_range() else { return Vec::new() };
        let mut out = Vec::with_capacity((row * h) as usize);
        for r in data.chunks(padded as usize).take(h as usize) {
            out.extend_from_slice(&r[..row as usize]);
        }
        out
    }
}

/// A buffer of `size` bytes on `device` (ours, or the app's renderer's)
/// whose memory CUDA can take in too: NVIDIA's encoder reads the frames made
/// in it (`super::nvenc`), its decoder's frames are copied into it
/// (`super::decode`). Its memory as an opaque fd, and the size of that
/// memory, which CUDA is told (it can be more than `size`).
pub(crate) fn shared_buffer(device: &wgpu::Device, size: u64) -> Result<(wgpu::Buffer, OwnedFd, u64)> {
    // SAFETY: the device is Vulkan.
    let hal = unsafe { device.as_hal::<Vulkan>() }.context("not a Vulkan device")?;
    ensure!(hal.enabled_device_extensions().contains(&ash::khr::external_memory_fd::NAME), "the graphics card's memory can't be shared");
    let raw = hal.raw_device();
    let external_fd = ash::khr::external_memory_fd::Device::new(hal.shared_instance().raw_instance(), raw);
    let opaque = vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD;
    let mut external = vk::ExternalMemoryBufferCreateInfo::default().handle_types(opaque);
    let info = vk::BufferCreateInfo::default()
        .size(size)
        .usage(vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .push_next(&mut external);
    // SAFETY: a valid create info.
    let buffer = unsafe { raw.create_buffer(&info, None) }.context("Vulkan won't make a buffer to share")?;
    let memory = (|| -> Result<vk::DeviceMemory> {
        // SAFETY: a buffer of ours.
        let req = unsafe { raw.get_buffer_memory_requirements(buffer) };
        // SAFETY: a valid physical device.
        let props = unsafe { hal.shared_instance().raw_instance().get_physical_device_memory_properties(hal.raw_physical_device()) };
        let type_index = (0..props.memory_type_count)
            .find(|&i| req.memory_type_bits & (1 << i) != 0 && props.memory_types[i as usize].property_flags.contains(vk::MemoryPropertyFlags::DEVICE_LOCAL))
            .context("no memory on the card for a shared buffer")?;
        let mut export = vk::ExportMemoryAllocateInfo::default().handle_types(opaque);
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().buffer(buffer);
        let alloc = vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(type_index).push_next(&mut export).push_next(&mut dedicated);
        // SAFETY: a valid allocate info.
        let memory = unsafe { raw.allocate_memory(&alloc, None) }.context("Vulkan won't allocate memory to share")?;
        // SAFETY: fresh memory for this buffer.
        if let Err(e) = unsafe { raw.bind_buffer_memory(buffer, memory, 0) } {
            // SAFETY: unused memory of ours.
            unsafe { raw.free_memory(memory, None) };
            bail!("Vulkan won't bind memory to share: {e}");
        }
        Ok(memory)
    })();
    let memory = match memory {
        Ok(m) => m,
        Err(e) => {
            // SAFETY: a buffer of ours, unused.
            unsafe { raw.destroy_buffer(buffer, None) };
            return Err(e);
        }
    };
    // SAFETY: a buffer of ours.
    let allocated = unsafe { raw.get_buffer_memory_requirements(buffer) }.size;
    // SAFETY: the buffer and memory are ours, and handed to wgpu, which
    // destroys them when the buffer is dropped.
    let hal_buffer = unsafe { wgpu::hal::vulkan::Buffer::from_raw_managed(buffer, memory, 0, size) };
    let fd = {
        let info = vk::MemoryGetFdInfoKHR::default().memory(memory).handle_type(opaque);
        // SAFETY: memory made exportable as an opaque fd.
        unsafe { external_fd.get_memory_fd(&info) }
    };
    drop(hal);
    let desc = wgpu::BufferDescriptor { label: Some("shared"), size, usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false };
    // SAFETY: made on this device, as `desc` says, and bound to memory.
    let shared = unsafe { device.create_buffer_from_hal::<Vulkan>(hal_buffer, &desc) };
    let fd = fd.map_err(|e| anyhow!("Vulkan won't share the memory: {e}"))?;
    // SAFETY: a new fd, ours.
    Ok((shared, unsafe { OwnedFd::from_raw_fd(fd) }, allocated))
}

/// The Vulkan UUID of `device`'s card (CUDA knows it by it too).
pub(crate) fn uuid_of(device: &wgpu::Device) -> Option<[u8; 16]> {
    // SAFETY: only read from.
    let hal = unsafe { device.as_hal::<Vulkan>() }?;
    let (instance, physical) = (hal.shared_instance().raw_instance(), hal.raw_physical_device());
    let mut id = vk::PhysicalDeviceIDProperties::default();
    let mut props = vk::PhysicalDeviceProperties2::default().push_next(&mut id);
    // SAFETY: a valid physical device and output structs.
    unsafe { instance.get_physical_device_properties2(physical, &mut props) };
    Some(id.device_uuid)
}

/// Whether the card has synchronization2 (Vulkan 1.3's), which sharing the
/// encoder's pictures with wgpu takes.
fn has_synchronization2(instance: &ash::Instance, physical: vk::PhysicalDevice) -> bool {
    let mut sync2 = vk::PhysicalDeviceSynchronization2Features::default();
    let mut features = vk::PhysicalDeviceFeatures2::default().push_next(&mut sync2);
    // SAFETY: a valid physical device and output structs.
    unsafe { instance.get_physical_device_features2(physical, &mut features) };
    sync2.synchronization2 == vk::TRUE
}

/// A queue family that encodes H.264 through Vulkan Video, other than wgpu's
/// (the first).
fn h264_encode_family(instance: &ash::Instance, physical: vk::PhysicalDevice) -> Option<u32> {
    // SAFETY: a valid physical device.
    let count = unsafe { instance.get_physical_device_queue_family_properties2_len(physical) };
    let mut video = vec![vk::QueueFamilyVideoPropertiesKHR::default(); count];
    let flags: Vec<vk::QueueFlags> = {
        let mut props: Vec<vk::QueueFamilyProperties2> = video.iter_mut().map(|v| vk::QueueFamilyProperties2::default().push_next(v)).collect();
        // SAFETY: as many output structs as families.
        unsafe { instance.get_physical_device_queue_family_properties2(physical, &mut props) };
        props.iter().map(|p| p.queue_family_properties.queue_flags).collect()
    };
    (1..count)
        .find(|&i| flags[i].contains(vk::QueueFlags::VIDEO_ENCODE_KHR) && video[i].video_codec_operations.contains(vk::VideoCodecOperationFlagsKHR::ENCODE_H264))
        .map(|i| i as u32)
}

/// The DRM format modifiers a B8G8R8A8 picture can be imported in, sampled
/// and copied from, in one plane.
fn importable_modifiers(instance: &ash::Instance, physical: vk::PhysicalDevice) -> Vec<u64> {
    let query = |list: &mut vk::DrmFormatModifierPropertiesListEXT| {
        let mut props = vk::FormatProperties2::default().push_next(list);
        // SAFETY: a valid physical device and output struct.
        unsafe { instance.get_physical_device_format_properties2(physical, vk::Format::B8G8R8A8_UNORM, &mut props) };
    };
    let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
    query(&mut list);
    let mut entries = vec![vk::DrmFormatModifierPropertiesEXT::default(); list.drm_format_modifier_count as usize];
    let mut list = vk::DrmFormatModifierPropertiesListEXT::default().drm_format_modifier_properties(&mut entries);
    query(&mut list);
    let needed = vk::FormatFeatureFlags::SAMPLED_IMAGE | vk::FormatFeatureFlags::TRANSFER_SRC;
    entries.iter().filter(|m| m.drm_format_modifier_plane_count == 1 && m.drm_format_modifier_tiling_features.contains(needed)).map(|m| m.drm_format_modifier).collect()
}

#[cfg(test)]
mod tests {
    #[test]
    #[ignore = "uses the graphics card: cargo test -p capture gpu_opens -- --ignored"]
    fn gpu_opens() {
        let gpu = super::gpu().expect("a graphics card that imports DMA-BUFs");
        eprintln!("{} modifiers: {:x?}", gpu.modifiers.len(), gpu.modifiers);
    }
}
