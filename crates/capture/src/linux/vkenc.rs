//! Recording with the graphics card's video encoder through Vulkan Video
//! (H.264): AMD's and Intel's cards with Mesa's drivers, and NVIDIA's where
//! its own interface can't start (`nvenc`). The frame is made on the card
//! (`super::gpu_frame`) and copied there into the encoder's NV12 picture by
//! wgpu, in the same submission; it's encoded on the card's video queue,
//! which only we use. Only the encoded frame comes down to the CPU.
//!
//! The H.264 stream is ours to describe: its parameter sets (SPS, PPS) and
//! each frame's place: an IDR every `gop` frames and P frames between, each
//! referring to the one before (two reconstructed pictures, used in turn).
//! As with NVENC, the pacer puts each frame in a free slot, and a thread of
//! ours records and submits its encoding, a few in flight, and takes each
//! one's bitstream when it's done, in order, for the writer.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError};
use std::sync::{Arc, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use ash::vk;
use ash::vk::native::*;
use wgpu::hal::api::Vulkan;

use super::gpu::Gpu;
use super::gpu_frame::{GpuPicture, Nv12Source, Nv12Target};
use super::screen::{Frame, Order};
use crate::mp4file::{EncodedFrame, VideoFrame};
use crate::mp4mux::{AvcConfig, annexb_to_avcc};
use crate::writer::{Command as WriterCommand, Media};

/// The pictures' format: NV12.
const NV12: vk::Format = vk::Format::G8_B8R8_2PLANE_420_UNORM;

/// Frames on the video queue at most (see `nvenc::IN_FLIGHT`).
const IN_FLIGHT: usize = 3;

/// How many frames can wait for the encoder: as for NVENC.
fn slots(fps: u32) -> usize {
    (fps as usize / 20).clamp(6, 32)
}

/// The H.264 level for this size and rate: the lowest whose limits (frame
/// size and macroblocks a second, Table A-1) it fits, from 4.1.
fn level(width: u32, height: u32, fps: u32) -> StdVideoH264LevelIdc {
    let frame = (width.div_ceil(16) * height.div_ceil(16)) as u64;
    let rate = frame * fps as u64;
    [
        (StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_4_1, 8192, 245_760),
        (StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_4_2, 8704, 522_240),
        (StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_5_0, 22_080, 589_824),
        (StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_5_1, 36_864, 983_040),
        (StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_5_2, 36_864, 2_073_600),
        (StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_6_0, 139_264, 4_177_920),
        (StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_6_1, 139_264, 8_355_840),
    ]
    .iter()
    .find(|(_, fs, mbps)| frame <= *fs && rate <= *mbps)
    .map_or(StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_6_2, |l| l.0)
}

/// The profile everything is made for: H.264 High, 4:2:0, 8 bits, for
/// recording rendered pictures with little delay. Made afresh for each use,
/// as it's chained into other structs.
struct Profile {
    h264: vk::VideoEncodeH264ProfileInfoKHR<'static>,
    usage: vk::VideoEncodeUsageInfoKHR<'static>,
}

impl Profile {
    fn new() -> Self {
        Self {
            h264: vk::VideoEncodeH264ProfileInfoKHR::default().std_profile_idc(StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH),
            usage: vk::VideoEncodeUsageInfoKHR::default()
                .video_usage_hints(vk::VideoEncodeUsageFlagsKHR::RECORDING)
                .video_content_hints(vk::VideoEncodeContentFlagsKHR::RENDERED)
                .tuning_mode(vk::VideoEncodeTuningModeKHR::LOW_LATENCY),
        }
    }

    fn info(&mut self) -> vk::VideoProfileInfoKHR<'_> {
        vk::VideoProfileInfoKHR::default()
            .video_codec_operation(vk::VideoCodecOperationFlagsKHR::ENCODE_H264)
            .chroma_subsampling(vk::VideoChromaSubsamplingFlagsKHR::TYPE_420)
            .luma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8)
            .chroma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8)
            .push_next(&mut self.h264)
            .push_next(&mut self.usage)
    }
}

/// What the card can do for this profile.
struct Caps {
    min_coded: vk::Extent2D,
    max_coded: vk::Extent2D,
    granularity: vk::Extent2D,
    max_dpb_slots: u32,
    max_references: u32,
    /// Reference pictures in images of their own (else layers of one).
    separate_references: bool,
    std_header_version: vk::ExtensionProperties,
    size_alignment: u64,
    feedback: vk::VideoEncodeFeedbackFlagsKHR,
    rate_modes: vk::VideoEncodeRateControlModeFlagsKHR,
    max_bitrate: u64,
    quality_levels: u32,
    max_level: StdVideoH264LevelIdc,
    syntax: vk::VideoEncodeH264StdFlagsKHR,
}

fn caps(gpu: &Gpu) -> Result<Caps> {
    // SAFETY: the device is Vulkan.
    let hal = unsafe { gpu.device.as_hal::<Vulkan>() }.context("not a Vulkan device")?;
    let shared = hal.shared_instance();
    let fns = ash::khr::video_queue::Instance::new(shared.entry(), shared.raw_instance());
    let mut profile = Profile::new();
    let info = profile.info();
    let mut h264 = vk::VideoEncodeH264CapabilitiesKHR::default();
    let mut encode = vk::VideoEncodeCapabilitiesKHR::default();
    let mut video = vk::VideoCapabilitiesKHR::default().push_next(&mut encode).push_next(&mut h264);
    // SAFETY: a valid physical device and profile, and output structs.
    unsafe { (fns.fp().get_physical_device_video_capabilities_khr)(hal.raw_physical_device(), &info, &mut video) }
        .result()
        .map_err(|e| anyhow!("the card can't encode H.264 High: {e}"))?;
    let (min_coded, max_coded, granularity) = (video.min_coded_extent, video.max_coded_extent, video.picture_access_granularity);
    let (max_dpb_slots, max_references, std_header_version, size_alignment) =
        (video.max_dpb_slots, video.max_active_reference_pictures, video.std_header_version, video.min_bitstream_buffer_size_alignment);
    let separate_references = video.flags.contains(vk::VideoCapabilityFlagsKHR::SEPARATE_REFERENCE_IMAGES);
    Ok(Caps {
        min_coded,
        max_coded,
        granularity,
        max_dpb_slots,
        max_references,
        separate_references,
        std_header_version,
        size_alignment,
        feedback: encode.supported_encode_feedback_flags,
        rate_modes: encode.rate_control_modes,
        max_bitrate: encode.max_bitrate,
        quality_levels: encode.max_quality_levels,
        max_level: h264.max_level_idc,
        syntax: h264.std_syntax_flags,
    })
}

/// A picture format the encoder takes: its flags and tiling too.
#[derive(Clone, Copy)]
struct Format {
    format: vk::Format,
    flags: vk::ImageCreateFlags,
    tiling: vk::ImageTiling,
}

/// A picture format for `usage` with this profile: NV12 if the card takes
/// it, with the flags and tiling it wants.
fn picture_format(gpu: &Gpu, usage: vk::ImageUsageFlags) -> Result<Format> {
    // SAFETY: the device is Vulkan.
    let hal = unsafe { gpu.device.as_hal::<Vulkan>() }.context("not a Vulkan device")?;
    let shared = hal.shared_instance();
    let fns = ash::khr::video_queue::Instance::new(shared.entry(), shared.raw_instance());
    let mut profile = Profile::new();
    let profiles = [profile.info()];
    let mut list = vk::VideoProfileListInfoKHR::default().profiles(&profiles);
    let info = vk::PhysicalDeviceVideoFormatInfoKHR::default().image_usage(usage).push_next(&mut list);
    let get = fns.fp().get_physical_device_video_format_properties_khr;
    let mut count = 0;
    // SAFETY: valid arguments; first the count, then that many.
    unsafe { get(hal.raw_physical_device(), &info, &mut count, std::ptr::null_mut()) }.result().map_err(|e| anyhow!("the card's picture formats for encoding: {e}"))?;
    let mut formats = vec![vk::VideoFormatPropertiesKHR::default(); count as usize];
    // SAFETY: as above.
    unsafe { get(hal.raw_physical_device(), &info, &mut count, formats.as_mut_ptr()) }.result().map_err(|e| anyhow!("the card's picture formats for encoding: {e}"))?;
    let mut nv12: Vec<Format> =
        formats[..count as usize].iter().filter(|f| f.format == NV12).map(|f| Format { format: f.format, flags: f.image_create_flags, tiling: f.image_tiling }).collect();
    nv12.sort_by_key(|f| f.tiling != vk::ImageTiling::OPTIMAL);
    nv12.into_iter().next().context("the card's encoder doesn't take NV12 pictures")
}

/// A memory type with `wanted` among `allowed`, else (`fallback`) any allowed.
fn memory_type(props: &vk::PhysicalDeviceMemoryProperties, allowed: u32, wanted: vk::MemoryPropertyFlags, fallback: Option<vk::MemoryPropertyFlags>) -> Option<u32> {
    let find = |flags: vk::MemoryPropertyFlags| (0..props.memory_type_count).find(|&i| allowed & (1 << i) != 0 && props.memory_types[i as usize].property_flags.contains(flags));
    find(wanted).or_else(|| fallback.and_then(find))
}

/// A frame being encoded: its bitstream's buffer (mapped), and its commands.
struct Output {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    map: *const u8,
    size: u64,
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
}

/// The Vulkan objects, used by the encoding thread and freed when it's done.
struct Session {
    gpu: Arc<Gpu>,
    device: ash::Device,
    video: ash::khr::video_queue::Device,
    encode: ash::khr::video_encode_queue::Device,
    session: vk::VideoSessionKHR,
    session_memory: Vec<vk::DeviceMemory>,
    params: vk::VideoSessionParametersKHR,
    /// The parameter sets (SPS, PPS) as the card wrote them, before each IDR.
    headers: Vec<u8>,
    /// The two reconstructed pictures: each a view, and its layer there.
    dpb: Vec<(vk::ImageView, u32)>,
    /// Their images (two, or one of two layers), with their layers.
    dpb_images: Vec<(vk::Image, vk::DeviceMemory, u32)>,
    outputs: Vec<Output>,
    queries: vk::QueryPool,
    pool: vk::CommandPool,
    /// The frames' size.
    coded: vk::Extent2D,
    /// Rate control: average and most bits a second, frames a second, and the mode.
    rate: (u64, u64, u32, vk::VideoEncodeRateControlModeFlagsKHR),
    gop: u32,
    quality: u32,
}

// SAFETY: the mapped pointers are only read by the encoding thread, after the
// card is done writing; everything else is handles.
unsafe impl Send for Session {}
// SAFETY: as above.
unsafe impl Sync for Session {}

impl Drop for Session {
    fn drop(&mut self) {
        let d = &self.device;
        if let Some(v) = &self.gpu.video {
            let queue = v.queue.lock().unwrap();
            // SAFETY: ours, and nothing else submits to it.
            let _ = unsafe { d.queue_wait_idle(*queue) };
        }
        // SAFETY: the card is done with all of them, and they're ours (the
        // pictures' images are wgpu's, dropped after this).
        unsafe {
            for o in &self.outputs {
                d.destroy_fence(o.fence, None);
                d.unmap_memory(o.memory);
                d.destroy_buffer(o.buffer, None);
                d.free_memory(o.memory, None);
            }
            d.destroy_command_pool(self.pool, None);
            d.destroy_query_pool(self.queries, None);
            let mut views: Vec<vk::ImageView> = self.dpb.iter().map(|d| d.0).collect();
            views.dedup();
            for view in views {
                d.destroy_image_view(view, None);
            }
            for (image, memory, _) in &self.dpb_images {
                d.destroy_image(*image, None);
                d.free_memory(*memory, None);
            }
            if self.params != vk::VideoSessionParametersKHR::null() {
                (self.video.fp().destroy_video_session_parameters_khr)(d.handle(), self.params, std::ptr::null());
            }
            if self.session != vk::VideoSessionKHR::null() {
                (self.video.fp().destroy_video_session_khr)(d.handle(), self.session, std::ptr::null());
            }
            for m in &self.session_memory {
                d.free_memory(*m, None);
            }
        }
    }
}

/// One frame's picture (wgpu's, copied into), our view of it for the
/// encoder, and which picture of ours it holds.
struct Slot {
    target: Nv12Target,
    image: vk::Image,
    view: vk::ImageView,
    device: ash::Device,
    holds: Option<u64>,
}

impl Drop for Slot {
    fn drop(&mut self) {
        // SAFETY: our view, not on the card (taken out, or never encoded);
        // the image goes after it, with the target's texture.
        unsafe { self.device.destroy_image_view(self.view, None) };
    }
}

/// Whether the card's encoder makes what it's given, asked once a run: five
/// frames drawn on the card (1080p: cropped from whole macroblocks), a
/// keyframe and four from it, encoded, decoded on the CPU and compared with
/// what went in (~80 ms). One that fails isn't used (ffmpeg encodes): it's
/// untried on most cards, and a recording of garbage would only show once
/// it was made.
fn check(gpu: &Arc<Gpu>) -> Result<()> {
    static CHECKED: OnceLock<Result<(), String>> = OnceLock::new();
    CHECKED
        .get_or_init(|| check_now(gpu).inspect_err(|e| eprintln!("the card's video encoder (Vulkan Video) failed its check: {e:#}")).map_err(|e| format!("{e:#}")))
        .clone()
        .map_err(|e| anyhow!("it failed its check ({e})"))
}

fn check_now(gpu: &Arc<Gpu>) -> Result<()> {
    let (w, h, fps, n) = (1920u32, 1080u32, 60u32, 5u32);
    let mut pic = GpuPicture::new(gpu.clone(), w, h, crate::ScreenPlacement::whole(crate::Framing::Fit));
    let (tx, rx) = mpsc::channel();
    let enc = VkEncoder::open(w, h, fps, 20_000, tx, &pic, false)?.context("no encoder")?;
    let mut wanted = Vec::new();
    for i in 0..n {
        // Made at half the size: the card scales it up, as a screen's.
        ensure!(pic.draw(&check_picture(w / 2, h / 2, i), i as u64 + 1), "a picture couldn't be made");
        let mut want = Vec::new();
        pic.nv12(&mut want);
        wanted.push(want);
        let until = Instant::now() + Duration::from_secs(2);
        while !enc.encode_gpu(&pic, i as u64, i as i64) {
            ensure!(!enc.failed() && Instant::now() < until, "it took no frame");
            thread::sleep(Duration::from_micros(200));
        }
    }
    enc.finish();
    let (mut config, mut frames) = (None, Vec::new());
    for command in rx.try_iter() {
        let WriterCommand::Media(Media::Video { frame, pts, key }) = command else { continue };
        if config.is_none() {
            config = frame.0.config.clone();
        }
        frames.push((frame.0.data.clone(), (pts * fps as f64).round() as i64, key));
    }
    ensure!(frames.len() == n as usize, "{} frames of {n} came out", frames.len());
    let config = config.context("no parameter sets")?;
    let frames: Vec<(&[u8], i64, bool)> = frames.iter().map(|(d, pts, key)| (&d[..], *pts, *key)).collect();
    let decoded = super::decode::decode_h264(&config.record(), &frames, (w, h))?;
    ensure!(decoded.len() == n as usize, "{} frames of {n} decode", decoded.len());
    for (i, ((_, got), want)) in decoded.iter().zip(&wanted).enumerate() {
        let off = got.iter().zip(want).map(|(a, b)| a.abs_diff(*b) as u64).sum::<u64>() as f64 / want.len().max(1) as f64;
        ensure!(off < 3.0, "frame {i} comes back off by {off:.1} a byte on average");
    }
    Ok(())
}

/// A picture for checking the encoder (and testing it): gradients moving
/// with `i`, every pixel different from its neighbours.
fn check_picture(w: u32, h: u32, i: u32) -> Frame {
    let mut data = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            data.extend_from_slice(&[((x + y) / 2 + i * 3) as u8, (y + i * 2) as u8, (x + i * 4) as u8, 255]);
        }
    }
    Frame { width: w, height: h, stride: w as usize * 4, order: Order::Bgr, data, texture: None }
}

/// A running encoder.
pub(crate) struct VkEncoder {
    free: Receiver<Slot>,
    frames: Option<SyncSender<(Slot, i64)>>,
    worker: Option<JoinHandle<()>>,
    session: Option<Arc<Session>>,
    broken: Arc<AtomicBool>,
    /// What it is, for the log.
    about: String,
}

impl VkEncoder {
    /// Start encoding `width`×`height` frames at `fps` and `kbps` into `out`,
    /// made on the card by `picture`; for an `export` (not live) at the best
    /// quality level but one. `Ok(None)` where the card can't encode H.264
    /// through Vulkan Video; an error where it fails its check ([`check`]).
    pub(crate) fn start(width: u32, height: u32, fps: u32, kbps: u32, out: Sender<WriterCommand>, picture: &dyn Nv12Source, export: bool) -> Result<Option<Self>> {
        let gpu = picture.gpu().clone();
        if gpu.video.is_none() {
            return Ok(None);
        }
        check(&gpu)?;
        let enc = Self::open(width, height, fps, kbps, out, picture, export)?;
        if let Some(e) = &enc {
            eprintln!("video encoder: {}", e.about);
        }
        Ok(enc)
    }

    /// [`Self::start`], unchecked.
    fn open(width: u32, height: u32, fps: u32, kbps: u32, out: Sender<WriterCommand>, picture: &dyn Nv12Source, export: bool) -> Result<Option<Self>> {
        let gpu = picture.gpu().clone();
        let Some(video) = &gpu.video else { return Ok(None) };
        let family = video.family;
        let caps = caps(&gpu)?;
        let src = picture_format(&gpu, vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR | vk::ImageUsageFlags::TRANSFER_DST)?;
        let dpb_format = picture_format(&gpu, vk::ImageUsageFlags::VIDEO_ENCODE_DPB_KHR)?;
        ensure!(caps.max_dpb_slots >= 2 && caps.max_references >= 1, "the card's encoder keeps no reference pictures");
        ensure!(caps.feedback.contains(vk::VideoEncodeFeedbackFlagsKHR::BITSTREAM_BUFFER_OFFSET | vk::VideoEncodeFeedbackFlagsKHR::BITSTREAM_BYTES_WRITTEN), "the card's encoder doesn't say where it wrote");
        // The pictures' size, which the SPS crops whole macroblocks to; the
        // pictures themselves as big as the card works in.
        let coded = vk::Extent2D { width, height };
        let mbs = (width.next_multiple_of(16), height.next_multiple_of(16));
        let gran = caps.granularity;
        let size = (mbs.0.next_multiple_of(gran.width.max(1)), mbs.1.next_multiple_of(gran.height.max(1)));
        let (min, max) = (caps.min_coded, caps.max_coded);
        ensure!(width >= min.width && height >= min.height && size.0 <= max.width && size.1 <= max.height, "the card's encoder doesn't take {width}x{height}");
        let mode = if caps.rate_modes.contains(vk::VideoEncodeRateControlModeFlagsKHR::VBR) {
            vk::VideoEncodeRateControlModeFlagsKHR::VBR
        } else if caps.rate_modes.contains(vk::VideoEncodeRateControlModeFlagsKHR::CBR) {
            vk::VideoEncodeRateControlModeFlagsKHR::CBR
        } else {
            bail!("the card's encoder can't be held to a bitrate");
        };
        let kbps = kbps.max(100) as u64;
        let most = caps.max_bitrate.max(1);
        let avg = (kbps * 1000).min(most);
        let rate = (avg, if mode == vk::VideoEncodeRateControlModeFlagsKHR::VBR { (kbps * 1500).min(most) } else { avg }, fps.max(1), mode);
        // The fastest quality level at high rates (as NVENC's presets).
        let quality = if export {
            caps.quality_levels.saturating_sub(2)
        } else if width as u64 * height as u64 * fps as u64 > 1920 * 1080 * 144 {
            0
        } else {
            caps.quality_levels.saturating_sub(1) / 2
        };

        // SAFETY: the device is Vulkan.
        let hal = unsafe { gpu.device.as_hal::<Vulkan>() }.context("not a Vulkan device")?;
        let device = hal.raw_device().clone();
        let instance = hal.shared_instance().raw_instance().clone();
        let physical = hal.raw_physical_device();
        drop(hal);
        // SAFETY: a valid physical device.
        let memory_props = unsafe { instance.get_physical_device_memory_properties(physical) };
        let video_fns = ash::khr::video_queue::Device::new(&instance, &device);
        let encode_fns = ash::khr::video_encode_queue::Device::new(&instance, &device);
        let mut session = Session {
            gpu: gpu.clone(),
            device: device.clone(),
            video: video_fns,
            encode: encode_fns,
            session: vk::VideoSessionKHR::null(),
            session_memory: Vec::new(),
            params: vk::VideoSessionParametersKHR::null(),
            headers: Vec::new(),
            dpb: Vec::new(),
            dpb_images: Vec::new(),
            outputs: Vec::new(),
            queries: vk::QueryPool::null(),
            pool: vk::CommandPool::null(),
            coded,
            rate,
            gop: crate::keyframe_every(fps),
            quality,
        };
        let d = &device;
        let allocate = |req: vk::MemoryRequirements, wanted: vk::MemoryPropertyFlags, fallback: Option<vk::MemoryPropertyFlags>, dedicated: Option<vk::Image>| -> Result<vk::DeviceMemory> {
            let index = memory_type(&memory_props, req.memory_type_bits, wanted, fallback).context("no memory for the encoder")?;
            let mut dedicated_info = vk::MemoryDedicatedAllocateInfo::default();
            let mut info = vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(index);
            if let Some(image) = dedicated {
                dedicated_info = dedicated_info.image(image);
                info = info.push_next(&mut dedicated_info);
            }
            // SAFETY: a valid allocate info.
            unsafe { d.allocate_memory(&info, None) }.map_err(|e| anyhow!("memory for the encoder: {e}"))
        };

        // The session, and its memory.
        {
            let mut profile = Profile::new();
            let info = profile.info();
            let create = vk::VideoSessionCreateInfoKHR::default()
                .queue_family_index(family)
                .video_profile(&info)
                .picture_format(src.format)
                .max_coded_extent(vk::Extent2D { width: size.0, height: size.1 })
                .reference_picture_format(dpb_format.format)
                .max_dpb_slots(2)
                .max_active_reference_pictures(1)
                .std_header_version(&caps.std_header_version);
            // SAFETY: a valid create info.
            unsafe { (session.video.fp().create_video_session_khr)(d.handle(), &create, std::ptr::null(), &mut session.session) }
                .result()
                .map_err(|e| anyhow!("the card won't open an encoding session: {e}"))?;
            let get = session.video.fp().get_video_session_memory_requirements_khr;
            let mut count = 0;
            // SAFETY: a session of ours; first the count, then that many.
            unsafe { get(d.handle(), session.session, &mut count, std::ptr::null_mut()) }.result()?;
            let mut reqs = vec![vk::VideoSessionMemoryRequirementsKHR::default(); count as usize];
            // SAFETY: as above.
            unsafe { get(d.handle(), session.session, &mut count, reqs.as_mut_ptr()) }.result()?;
            let mut binds = Vec::new();
            for r in &reqs[..count as usize] {
                let memory = allocate(r.memory_requirements, vk::MemoryPropertyFlags::DEVICE_LOCAL, Some(vk::MemoryPropertyFlags::empty()), None)?;
                session.session_memory.push(memory);
                binds.push(vk::BindVideoSessionMemoryInfoKHR::default().memory_bind_index(r.memory_bind_index).memory(memory).memory_offset(0).memory_size(r.memory_requirements.size));
            }
            // SAFETY: memory made for these requirements.
            unsafe { (session.video.fp().bind_video_session_memory_khr)(d.handle(), session.session, binds.len() as u32, binds.as_ptr()) }
                .result()
                .map_err(|e| anyhow!("memory for the encoding session: {e}"))?;
        }

        // The parameter sets: SPS and PPS.
        {
            let mut vui_flags: StdVideoH264SpsVuiFlags = unsafe { std::mem::zeroed() };
            vui_flags.set_video_signal_type_present_flag(1);
            vui_flags.set_color_description_present_flag(1);
            let mut vui: StdVideoH264SequenceParameterSetVui = unsafe { std::mem::zeroed() };
            vui.flags = vui_flags;
            // BT.709, studio range: what the frames are made in.
            vui.video_format = 5;
            vui.colour_primaries = 1;
            vui.transfer_characteristics = 1;
            vui.matrix_coefficients = 1;
            let mut sps_flags: StdVideoH264SpsFlags = unsafe { std::mem::zeroed() };
            sps_flags.set_direct_8x8_inference_flag(1);
            sps_flags.set_frame_mbs_only_flag(1);
            sps_flags.set_vui_parameters_present_flag(1);
            let crop = ((mbs.0 - width) / 2, (mbs.1 - height) / 2);
            if crop != (0, 0) {
                sps_flags.set_frame_cropping_flag(1);
            }
            let mut sps: StdVideoH264SequenceParameterSet = unsafe { std::mem::zeroed() };
            sps.flags = sps_flags;
            sps.profile_idc = StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH;
            sps.level_idc = level(width, height, fps).min(caps.max_level);
            sps.chroma_format_idc = StdVideoH264ChromaFormatIdc_STD_VIDEO_H264_CHROMA_FORMAT_IDC_420;
            // frame_num to 255 and picture order to 511: a GOP is at most 120 frames.
            sps.log2_max_frame_num_minus4 = 4;
            sps.pic_order_cnt_type = StdVideoH264PocType_STD_VIDEO_H264_POC_TYPE_0;
            sps.log2_max_pic_order_cnt_lsb_minus4 = 4;
            sps.max_num_ref_frames = 1;
            sps.pic_width_in_mbs_minus1 = mbs.0 / 16 - 1;
            sps.pic_height_in_map_units_minus1 = mbs.1 / 16 - 1;
            sps.frame_crop_right_offset = crop.0;
            sps.frame_crop_bottom_offset = crop.1;
            sps.pSequenceParameterSetVui = &vui;
            let syntax = caps.syntax;
            let mut pps_flags: StdVideoH264PpsFlags = unsafe { std::mem::zeroed() };
            if syntax.contains(vk::VideoEncodeH264StdFlagsKHR::ENTROPY_CODING_MODE_FLAG_SET) {
                pps_flags.set_entropy_coding_mode_flag(1);
            }
            if syntax.contains(vk::VideoEncodeH264StdFlagsKHR::TRANSFORM_8X8_MODE_FLAG_SET) {
                pps_flags.set_transform_8x8_mode_flag(1);
            }
            let mut pps: StdVideoH264PictureParameterSet = unsafe { std::mem::zeroed() };
            pps.flags = pps_flags;
            let (spss, ppss) = ([sps], [pps]);
            let add = vk::VideoEncodeH264SessionParametersAddInfoKHR::default().std_sp_ss(&spss).std_pp_ss(&ppss);
            let mut h264 = vk::VideoEncodeH264SessionParametersCreateInfoKHR::default().max_std_sps_count(1).max_std_pps_count(1).parameters_add_info(&add);
            let mut quality_info = vk::VideoEncodeQualityLevelInfoKHR::default().quality_level(quality);
            let create = vk::VideoSessionParametersCreateInfoKHR::default().video_session(session.session).push_next(&mut h264).push_next(&mut quality_info);
            // SAFETY: a valid create info for our session.
            unsafe { (session.video.fp().create_video_session_parameters_khr)(d.handle(), &create, std::ptr::null(), &mut session.params) }
                .result()
                .map_err(|e| anyhow!("the encoder won't take the parameter sets: {e}"))?;
            let mut h264_get = vk::VideoEncodeH264SessionParametersGetInfoKHR::default().write_std_sps(true).write_std_pps(true);
            let get_info = vk::VideoEncodeSessionParametersGetInfoKHR::default().video_session_parameters(session.params).push_next(&mut h264_get);
            let get = session.encode.fp().get_encoded_video_session_parameters_khr;
            let mut len = 0usize;
            // SAFETY: valid arguments; first the size, then the bytes.
            unsafe { get(d.handle(), &get_info, std::ptr::null_mut(), &mut len, std::ptr::null_mut()) }.result()?;
            session.headers = vec![0; len];
            // SAFETY: as above, with room for them.
            unsafe { get(d.handle(), &get_info, std::ptr::null_mut(), &mut len, session.headers.as_mut_ptr().cast()) }.result()?;
            session.headers.truncate(len);
        }

        // The reconstructed pictures, only the video queue's: two images
        // where the card takes them so, else two layers of one.
        {
            let (images, layers) = if caps.separate_references { (2, 1) } else { (1, 2) };
            for _ in 0..images {
                let mut profile = Profile::new();
                let profiles = [profile.info()];
                let mut list = vk::VideoProfileListInfoKHR::default().profiles(&profiles);
                let info = vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(dpb_format.format)
                    .extent(vk::Extent3D { width: size.0, height: size.1, depth: 1 })
                    .mip_levels(1)
                    .array_layers(layers)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .tiling(dpb_format.tiling)
                    .usage(vk::ImageUsageFlags::VIDEO_ENCODE_DPB_KHR)
                    .sharing_mode(vk::SharingMode::EXCLUSIVE)
                    .initial_layout(vk::ImageLayout::UNDEFINED)
                    .push_next(&mut list);
                // SAFETY: a valid create info.
                let image = unsafe { d.create_image(&info, None) }.map_err(|e| anyhow!("the encoder's reference pictures: {e}"))?;
                // SAFETY: an image of ours.
                let req = unsafe { d.get_image_memory_requirements(image) };
                let memory = match allocate(req, vk::MemoryPropertyFlags::DEVICE_LOCAL, None, Some(image)) {
                    Ok(m) => m,
                    Err(e) => {
                        // SAFETY: an unused image of ours.
                        unsafe { d.destroy_image(image, None) };
                        return Err(e);
                    }
                };
                session.dpb_images.push((image, memory, layers));
                // SAFETY: memory made for it.
                unsafe { d.bind_image_memory(image, memory, 0) }?;
                let view = vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(if layers > 1 { vk::ImageViewType::TYPE_2D_ARRAY } else { vk::ImageViewType::TYPE_2D })
                    .format(dpb_format.format)
                    .subresource_range(vk::ImageSubresourceRange { aspect_mask: vk::ImageAspectFlags::COLOR, base_mip_level: 0, level_count: 1, base_array_layer: 0, layer_count: layers });
                // SAFETY: a valid view of our image.
                let view = unsafe { d.create_image_view(&view, None) }?;
                session.dpb.extend((0..layers).map(|layer| (view, layer)));
            }
        }

        // Where the frames' bitstreams go, and the commands that make them.
        {
            let pool = vk::CommandPoolCreateInfo::default().queue_family_index(family).flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
            // SAFETY: a valid create info.
            session.pool = unsafe { d.create_command_pool(&pool, None) }?;
            let alloc = vk::CommandBufferAllocateInfo::default().command_pool(session.pool).level(vk::CommandBufferLevel::PRIMARY).command_buffer_count(IN_FLIGHT as u32 + 1);
            // SAFETY: a pool of ours.
            let cmds = unsafe { d.allocate_command_buffers(&alloc) }?;
            let mut profile = Profile::new();
            let mut feedback = vk::QueryPoolVideoEncodeFeedbackCreateInfoKHR::default()
                .encode_feedback_flags(vk::VideoEncodeFeedbackFlagsKHR::BITSTREAM_BUFFER_OFFSET | vk::VideoEncodeFeedbackFlagsKHR::BITSTREAM_BYTES_WRITTEN);
            let mut info = profile.info();
            let queries = vk::QueryPoolCreateInfo::default().query_type(vk::QueryType::VIDEO_ENCODE_FEEDBACK_KHR).query_count(IN_FLIGHT as u32 + 1).push_next(&mut feedback).push_next(&mut info);
            // SAFETY: a valid create info.
            session.queries = unsafe { d.create_query_pool(&queries, None) }.map_err(|e| anyhow!("the encoder's feedback: {e}"))?;
            let align = caps.size_alignment.max(1);
            let bytes = (size.0 as u64 * size.1 as u64 * 3 / 2 + 65_536).next_multiple_of(align);
            for cmd in cmds {
                let mut profile = Profile::new();
                let profiles = [profile.info()];
                let mut list = vk::VideoProfileListInfoKHR::default().profiles(&profiles);
                let info = vk::BufferCreateInfo::default().size(bytes).usage(vk::BufferUsageFlags::VIDEO_ENCODE_DST_KHR).sharing_mode(vk::SharingMode::EXCLUSIVE).push_next(&mut list);
                // SAFETY: a valid create info.
                let buffer = unsafe { d.create_buffer(&info, None) }?;
                // SAFETY: a buffer of ours.
                let req = unsafe { d.get_buffer_memory_requirements(buffer) };
                let host = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
                let memory = match allocate(req, host | vk::MemoryPropertyFlags::HOST_CACHED, Some(host), None) {
                    Ok(m) => m,
                    Err(e) => {
                        // SAFETY: an unused buffer of ours.
                        unsafe { d.destroy_buffer(buffer, None) };
                        return Err(e);
                    }
                };
                // SAFETY: memory made for it, then mapped whole while it lives.
                let map = unsafe {
                    d.bind_buffer_memory(buffer, memory, 0)?;
                    d.map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())?
                };
                // SAFETY: a valid create info.
                let fence = unsafe { d.create_fence(&vk::FenceCreateInfo::default(), None) }?;
                session.outputs.push(Output { buffer, memory, map: map.cast(), size: bytes, cmd, fence });
            }
        }

        // The slots: each an NV12 picture of the encoder's, which wgpu
        // copies the frame into (made in one buffer, which they share: the
        // pacer makes one frame at a time).
        let pitch = size.0.next_multiple_of(256);
        let staging = {
            let desc = wgpu::BufferDescriptor { label: Some("nv12 for vulkan video"), size: (pitch as u64 * size.1 as u64 * 3 / 2).next_multiple_of(4), usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC, mapped_at_creation: false };
            gpu.device.create_buffer(&desc)
        };
        let count = slots(fps);
        let (free_tx, free) = mpsc::channel();
        let mut slots_made = Vec::new();
        for _ in 0..count {
            let (texture, image) = shared_picture(&gpu, &src, size, family)?;
            let view = vk::ImageViewCreateInfo::default()
                .image(image)
                .view_type(vk::ImageViewType::TYPE_2D)
                .format(src.format)
                .subresource_range(vk::ImageSubresourceRange { aspect_mask: vk::ImageAspectFlags::COLOR, base_mip_level: 0, level_count: 1, base_array_layer: 0, layer_count: 1 });
            let mut usage = vk::ImageViewUsageCreateInfo::default().usage(vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR);
            // SAFETY: a valid view of the picture's image.
            let view = unsafe { d.create_image_view(&view.push_next(&mut usage), None) }?;
            slots_made.push(Slot { target: picture.nv12_target(staging.clone(), pitch, size, Some(texture)), image, view, device: device.clone(), holds: None });
        }
        let session = Arc::new(session);
        let broken = Arc::new(AtomicBool::new(false));
        let (frames, frames_rx) = mpsc::sync_channel(count);
        let about = format!(
            "Vulkan Video ({width}x{height} at {fps} fps, quality level {quality} of {}, {} {} kbps)",
            caps.quality_levels,
            if mode == vk::VideoEncodeRateControlModeFlagsKHR::VBR { "VBR" } else { "CBR" },
            kbps
        );
        let mut enc = Self { free, frames: Some(frames), worker: None, session: Some(session.clone()), broken: broken.clone(), about };
        for slot in slots_made {
            let _ = free_tx.send(slot);
        }
        enc.worker = Some(thread::Builder::new().name("vulkan encode".into()).spawn(move || encode_frames(&session, frames_rx, free_tx, &broken, fps, out))?);
        Ok(Some(enc))
    }

    /// Encode frame `n`, made on the card by `picture` (its picture number
    /// `version`). False if it's skipped: every slot is busy, or encoding
    /// stopped working.
    pub(crate) fn encode_gpu(&self, picture: &dyn Nv12Source, version: u64, n: i64) -> bool {
        if self.broken.load(Ordering::Relaxed) {
            return false;
        }
        let Ok(mut slot) = self.free.try_recv() else { return false };
        if slot.holds != Some(version) {
            picture.nv12_to(&slot.target);
            slot.holds = Some(version);
        }
        self.frames.as_ref().is_some_and(|f| f.send((slot, n)).is_ok())
    }

    /// Whether encoding stopped working.
    pub(crate) fn failed(&self) -> bool {
        self.broken.load(Ordering::Relaxed)
    }

    /// Encode what's handed in and wait for the last frame.
    pub(crate) fn finish(mut self) {
        drop(self.frames.take());
        if let Some(t) = self.worker.take() {
            let _ = t.join();
        }
    }
}

impl Drop for VkEncoder {
    fn drop(&mut self) {
        // The encoding thread finishes first, then the session goes (our
        // views of the pictures with it), and only then the pictures.
        drop(self.frames.take());
        if let Some(t) = self.worker.take() {
            let _ = t.join();
        }
        let slots: Vec<Slot> = self.free.try_iter().collect();
        drop(self.session.take());
        drop(slots);
    }
}

/// An NV12 picture for the encoder that wgpu copies frames into: made by
/// us, for this profile, shared by wgpu's queue and the video queue, and
/// handed to wgpu (which frees it). Its texture and image.
fn shared_picture(gpu: &Gpu, format: &Format, size: (u32, u32), family: u32) -> Result<(wgpu::Texture, vk::Image)> {
    // SAFETY: the device is Vulkan.
    let hal = unsafe { gpu.device.as_hal::<Vulkan>() }.context("not a Vulkan device")?;
    let d = hal.raw_device();
    let mut profile = Profile::new();
    let profiles = [profile.info()];
    let mut list = vk::VideoProfileListInfoKHR::default().profiles(&profiles);
    let families = [hal.queue_family_index(), family];
    let info = vk::ImageCreateInfo::default()
        .flags(format.flags)
        .image_type(vk::ImageType::TYPE_2D)
        .format(NV12)
        .extent(vk::Extent3D { width: size.0, height: size.1, depth: 1 })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(format.tiling)
        .usage(vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR | vk::ImageUsageFlags::TRANSFER_DST)
        .sharing_mode(vk::SharingMode::CONCURRENT)
        .queue_family_indices(&families)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .push_next(&mut list);
    // SAFETY: a valid create info.
    let image = unsafe { d.create_image(&info, None) }.map_err(|e| anyhow!("a picture for the encoder: {e}"))?;
    let memory = (|| -> Result<vk::DeviceMemory> {
        // SAFETY: an image of ours.
        let req = unsafe { d.get_image_memory_requirements(image) };
        // SAFETY: a valid physical device.
        let props = unsafe { hal.shared_instance().raw_instance().get_physical_device_memory_properties(hal.raw_physical_device()) };
        let index = memory_type(&props, req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL, None).context("no memory on the card for the encoder's pictures")?;
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let alloc = vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(index).push_next(&mut dedicated);
        // SAFETY: a valid allocate info.
        let memory = unsafe { d.allocate_memory(&alloc, None) }.map_err(|e| anyhow!("memory for the encoder's pictures: {e}"))?;
        // SAFETY: fresh memory for this image.
        if let Err(e) = unsafe { d.bind_image_memory(image, memory, 0) } {
            // SAFETY: unused memory of ours.
            unsafe { d.free_memory(memory, None) };
            bail!("memory for the encoder's pictures: {e}");
        }
        Ok(memory)
    })();
    let memory = match memory {
        Ok(m) => m,
        Err(e) => {
            // SAFETY: an unused image of ours.
            unsafe { d.destroy_image(image, None) };
            return Err(e);
        }
    };
    let extent = wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 };
    let hal_desc = wgpu::hal::TextureDescriptor {
        label: Some("encoder picture"),
        size: extent,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::NV12,
        usage: wgpu::TextureUses::COPY_DST,
        memory_flags: wgpu::hal::MemoryFlags::empty(),
        view_formats: vec![],
    };
    // SAFETY: the image and memory are ours, handed to the texture, which
    // destroys them when dropped.
    let texture = unsafe { hal.texture_from_raw(image, &hal_desc, None, wgpu::hal::vulkan::TextureMemory::Dedicated(memory)) };
    drop(hal);
    let desc = wgpu::TextureDescriptor {
        label: Some("encoder picture"),
        size: extent,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::NV12,
        usage: wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    };
    // SAFETY: made on this device as `desc` says, in its undefined layout.
    Ok((unsafe { gpu.device.create_texture_from_hal::<Vulkan>(texture, &desc, wgpu::TextureUses::UNINITIALIZED) }, image))
}

/// Where a frame is in the stream.
#[derive(Clone, Copy)]
struct Place {
    idr: bool,
    idr_id: u16,
    frame_num: u32,
    poc: i32,
    /// The reconstructed picture's layer, and the one it refers to.
    setup: u32,
}

/// Record and submit frame `place`'s encoding from `slot`'s picture into
/// output `o`.
fn submit(s: &Session, slot: &Slot, o: usize, place: Place, before: Option<Place>, first: bool) -> Result<()> {
    let d = &s.device;
    let out = &s.outputs[o];
    let (picture, picture_view) = (slot.image, slot.view);
    let range = |layers| vk::ImageSubresourceRange { aspect_mask: vk::ImageAspectFlags::COLOR, base_mip_level: 0, level_count: 1, base_array_layer: 0, layer_count: layers };
    // SAFETY: a command buffer of ours that isn't on the card (its fence was waited for).
    unsafe {
        d.reset_command_buffer(out.cmd, vk::CommandBufferResetFlags::empty())?;
        d.begin_command_buffer(out.cmd, &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT))?;
        d.cmd_reset_query_pool(out.cmd, s.queries, o as u32, 1);
        // The picture from wgpu's copy (it's left as wgpu last had it), the
        // reference pictures from the last frame's encoding.
        let mut images = vec![vk::ImageMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::MEMORY_WRITE)
            .dst_access_mask(vk::AccessFlags::MEMORY_READ)
            .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .new_layout(vk::ImageLayout::VIDEO_ENCODE_SRC_KHR)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(picture)
            .subresource_range(range(1))];
        if first {
            for (image, _, layers) in &s.dpb_images {
                images.push(
                    vk::ImageMemoryBarrier::default()
                        .dst_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE)
                        .old_layout(vk::ImageLayout::UNDEFINED)
                        .new_layout(vk::ImageLayout::VIDEO_ENCODE_DPB_KHR)
                        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .image(*image)
                        .subresource_range(range(*layers)),
                );
            }
        }
        let memory = [vk::MemoryBarrier::default().src_access_mask(vk::AccessFlags::MEMORY_WRITE).dst_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE)];
        d.cmd_pipeline_barrier(out.cmd, vk::PipelineStageFlags::ALL_COMMANDS, vk::PipelineStageFlags::ALL_COMMANDS, vk::DependencyFlags::empty(), &memory, &[], &images);
    }

    let coded = s.coded;
    let resource = |slot: u32| {
        let (view, layer) = s.dpb[slot as usize];
        vk::VideoPictureResourceInfoKHR::default().coded_extent(coded).base_array_layer(layer).image_view_binding(view)
    };
    let reference = |p: &Place| {
        let mut info: StdVideoEncodeH264ReferenceInfo = unsafe { std::mem::zeroed() };
        info.primary_pic_type = if p.idr { StdVideoH264PictureType_STD_VIDEO_H264_PICTURE_TYPE_IDR } else { StdVideoH264PictureType_STD_VIDEO_H264_PICTURE_TYPE_P };
        info.FrameNum = p.frame_num;
        info.PicOrderCnt = p.poc;
        info
    };
    let setup_resource = resource(place.setup);
    let setup_info = reference(&place);
    let mut setup_dpb = vk::VideoEncodeH264DpbSlotInfoKHR::default().std_reference_info(&setup_info);
    let setup_slot = vk::VideoReferenceSlotInfoKHR::default().slot_index(place.setup as i32).picture_resource(&setup_resource).push_next(&mut setup_dpb);
    let before_resource = before.map(|b| resource(b.setup));
    let before_info = before.map(|b| reference(&b));
    let mut before_dpb = before_info.as_ref().map(|i| vk::VideoEncodeH264DpbSlotInfoKHR::default().std_reference_info(i));
    let before_slot = match (&before, &before_resource, &mut before_dpb) {
        (Some(b), Some(r), Some(dpb)) => Some(vk::VideoReferenceSlotInfoKHR::default().slot_index(b.setup as i32).picture_resource(r).push_next(dpb)),
        _ => None,
    };
    // Bound for this encoding: the picture it makes (in no slot yet), and
    // the one it refers to.
    let mut bound = vec![vk::VideoReferenceSlotInfoKHR::default().slot_index(-1).picture_resource(&setup_resource)];
    bound.extend(before_slot);

    let (avg, most, fps, mode) = s.rate;
    // The H.264 part of the layer says nothing more (no QP limits), but
    // NVIDIA's driver rejects the encoding without it: "invalid video std
    // parameters" when the commands are ended.
    let mut h264_layer = vk::VideoEncodeH264RateControlLayerInfoKHR::default();
    let layers = [vk::VideoEncodeRateControlLayerInfoKHR::default().average_bitrate(avg).max_bitrate(most).frame_rate_numerator(fps).frame_rate_denominator(1).push_next(&mut h264_layer)];
    let rc_flags = vk::VideoEncodeH264RateControlFlagsKHR::REGULAR_GOP | vk::VideoEncodeH264RateControlFlagsKHR::REFERENCE_PATTERN_FLAT;
    let mut h264_rc = vk::VideoEncodeH264RateControlInfoKHR::default().flags(rc_flags).gop_frame_count(s.gop).idr_period(s.gop).consecutive_b_frame_count(0).temporal_layer_count(1);
    let mut rc = vk::VideoEncodeRateControlInfoKHR::default().rate_control_mode(mode).layers(&layers).virtual_buffer_size_in_ms(2000).initial_virtual_buffer_size_in_ms(1000);
    let mut begin = vk::VideoBeginCodingInfoKHR::default().video_session(s.session).video_session_parameters(s.params).reference_slots(&bound);
    // The session's rate control is said again each time once it's set.
    let (mut h264_rc2, mut rc2) = (h264_rc, rc);
    if !first {
        begin = begin.push_next(&mut rc2).push_next(&mut h264_rc2);
    }

    // References: none in list 1 (no B-frames), and in list 0 the frame
    // before, unless it's an IDR ("minus 1" of none is -1).
    let mut lists: StdVideoEncodeH264ReferenceListsInfo = unsafe { std::mem::zeroed() };
    lists.RefPicList0 = [0xFF; 32];
    lists.RefPicList1 = [0xFF; 32];
    lists.num_ref_idx_l1_active_minus1 = 0xFF;
    if before.is_none() {
        lists.num_ref_idx_l0_active_minus1 = 0xFF;
    }
    if let Some(b) = &before {
        lists.RefPicList0[0] = b.setup as u8;
    }
    let mut pic_flags: StdVideoEncodeH264PictureInfoFlags = unsafe { std::mem::zeroed() };
    pic_flags.set_IdrPicFlag(u32::from(place.idr));
    pic_flags.set_is_reference(1);
    let mut pic: StdVideoEncodeH264PictureInfo = unsafe { std::mem::zeroed() };
    pic.flags = pic_flags;
    pic.idr_pic_id = place.idr_id;
    pic.primary_pic_type = setup_info.primary_pic_type;
    pic.frame_num = place.frame_num;
    pic.PicOrderCnt = place.poc;
    pic.pRefLists = &lists;
    let mut slice: StdVideoEncodeH264SliceHeader = unsafe { std::mem::zeroed() };
    slice.slice_type = if place.idr { StdVideoH264SliceType_STD_VIDEO_H264_SLICE_TYPE_I } else { StdVideoH264SliceType_STD_VIDEO_H264_SLICE_TYPE_P };
    let nalus = [vk::VideoEncodeH264NaluSliceInfoKHR::default().std_slice_header(&slice)];
    let mut h264_pic = vk::VideoEncodeH264PictureInfoKHR::default().nalu_slice_entries(&nalus).std_picture_info(&pic);
    let src = vk::VideoPictureResourceInfoKHR::default().coded_extent(coded).image_view_binding(picture_view);
    let refs: Vec<vk::VideoReferenceSlotInfoKHR> = bound.iter().skip(1).copied().collect();
    let encode = vk::VideoEncodeInfoKHR::default()
        .dst_buffer(out.buffer)
        .dst_buffer_offset(0)
        .dst_buffer_range(out.size)
        .src_picture_resource(src)
        .setup_reference_slot(&setup_slot)
        .reference_slots(&refs)
        .push_next(&mut h264_pic);

    // SAFETY: all of it valid while recorded.
    unsafe {
        (s.video.fp().cmd_begin_video_coding_khr)(out.cmd, &begin);
        if first {
            let mut quality = vk::VideoEncodeQualityLevelInfoKHR::default().quality_level(s.quality);
            let control = vk::VideoCodingControlInfoKHR::default()
                .flags(vk::VideoCodingControlFlagsKHR::RESET | vk::VideoCodingControlFlagsKHR::ENCODE_RATE_CONTROL | vk::VideoCodingControlFlagsKHR::ENCODE_QUALITY_LEVEL)
                .push_next(&mut rc)
                .push_next(&mut h264_rc)
                .push_next(&mut quality);
            (s.video.fp().cmd_control_video_coding_khr)(out.cmd, &control);
        }
        d.cmd_begin_query(out.cmd, s.queries, o as u32, vk::QueryControlFlags::empty());
        (s.encode.fp().cmd_encode_video_khr)(out.cmd, &encode);
        d.cmd_end_query(out.cmd, s.queries, o as u32);
        (s.video.fp().cmd_end_video_coding_khr)(out.cmd, &vk::VideoEndCodingInfoKHR::default());
        // The picture back as wgpu left it, and the bitstream to us.
        let back = [vk::ImageMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::MEMORY_READ)
            .dst_access_mask(vk::AccessFlags::MEMORY_WRITE)
            .old_layout(vk::ImageLayout::VIDEO_ENCODE_SRC_KHR)
            .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(picture)
            .subresource_range(range(1))];
        let host = [vk::MemoryBarrier::default().src_access_mask(vk::AccessFlags::MEMORY_WRITE).dst_access_mask(vk::AccessFlags::HOST_READ)];
        d.cmd_pipeline_barrier(out.cmd, vk::PipelineStageFlags::ALL_COMMANDS, vk::PipelineStageFlags::ALL_COMMANDS | vk::PipelineStageFlags::HOST, vk::DependencyFlags::empty(), &host, &[], &back);
        d.end_command_buffer(out.cmd).map_err(|e| anyhow!("recording a frame's encoding: {e}"))?;
        d.reset_fences(&[out.fence])?;
        let cmds = [out.cmd];
        let submit = [vk::SubmitInfo::default().command_buffers(&cmds)];
        let video = s.gpu.video.as_ref().context("no video queue")?;
        let queue = video.queue.lock().unwrap();
        d.queue_submit(*queue, &submit, out.fence).map_err(|e| anyhow!("submitting a frame: {e}"))?;
    }
    Ok(())
}

/// Wait for output `o`'s frame and take its bitstream (with the parameter
/// sets before an IDR), as Annex B.
fn take(s: &Session, o: usize, idr: bool) -> Result<Vec<u8>> {
    let d = &s.device;
    let out = &s.outputs[o];
    // SAFETY: a fence of ours, submitted with the frame.
    unsafe { d.wait_for_fences(&[out.fence], true, u64::MAX) }?;
    let mut feedback = [[0u32; 3]];
    // SAFETY: the frame's query, done.
    unsafe { d.get_query_pool_results(s.queries, o as u32, &mut feedback, vk::QueryResultFlags::WITH_STATUS_KHR) }?;
    let [offset, bytes, status] = feedback[0];
    ensure!(status as i32 == vk::QueryResultStatusKHR::COMPLETE.as_raw(), "the card didn't encode a frame (status {})", status as i32);
    ensure!(offset as u64 + bytes as u64 <= out.size, "the encoded frame doesn't fit its buffer");
    let mut au = Vec::with_capacity(bytes as usize + s.headers.len());
    if idr {
        au.extend_from_slice(&s.headers);
    }
    // SAFETY: mapped, coherent memory the card is done writing.
    au.extend_from_slice(unsafe { std::slice::from_raw_parts(out.map.add(offset as usize), bytes as usize) });
    Ok(au)
}

/// Encode each frame as it comes, a few on the card at once, and take each
/// one out when it's done, in order (as `nvenc::encode_frames`).
fn encode_frames(s: &Session, frames: Receiver<(Slot, i64)>, free: Sender<Slot>, broken: &AtomicBool, fps: u32, out: Sender<WriterCommand>) {
    let mut flight: VecDeque<(Slot, i64, usize, bool)> = VecDeque::new();
    let mut config: Option<AvcConfig> = None;
    let (mut writer_gone, mut ended, mut first) = (false, false, true);
    let (mut in_gop, mut idr_id, mut submitted) = (0u32, 0u16, 0usize);
    let mut before: Option<Place> = None;
    let stop = |why: &anyhow::Error| {
        if !broken.swap(true, Ordering::Relaxed) {
            eprintln!("video encoder (Vulkan Video) stopped: {why:#}");
        }
    };
    loop {
        let next = if flight.is_empty() {
            match frames.recv() {
                Ok(f) => Some(f),
                Err(_) => break,
            }
        } else if ended {
            None
        } else {
            match frames.try_recv() {
                Ok(f) => Some(f),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => {
                    ended = true;
                    None
                }
            }
        };
        if let Some((slot, n)) = next {
            if broken.load(Ordering::Relaxed) {
                let _ = free.send(slot);
                continue;
            }
            let idr = in_gop == 0;
            if idr && !first {
                idr_id = idr_id.wrapping_add(1);
            }
            let place = Place { idr, idr_id, frame_num: in_gop % 256, poc: (2 * in_gop % 256) as i32, setup: in_gop % 2 };
            let o = submitted % s.outputs.len();
            match submit(s, &slot, o, place, if idr { None } else { before }, first) {
                Ok(()) => {
                    first = false;
                    before = Some(place);
                    in_gop = (in_gop + 1) % s.gop;
                    submitted += 1;
                    flight.push_back((slot, n, o, idr));
                }
                Err(e) => {
                    stop(&e);
                    let _ = free.send(slot);
                    continue;
                }
            }
            if flight.len() < IN_FLIGHT {
                continue;
            }
        }
        let Some((slot, n, o, idr)) = flight.pop_front() else { continue };
        let taken = take(s, o, idr);
        let _ = free.send(slot);
        let au = match taken {
            Ok(au) => au,
            Err(e) => {
                stop(&e);
                continue;
            }
        };
        let frame = annexb_to_avcc(&au);
        if frame.data.is_empty() || writer_gone {
            continue;
        }
        if let Some(c) = frame.config {
            config = Some(c);
        }
        let key = frame.idr;
        let media = Media::Video {
            frame: VideoFrame(Arc::new(EncodedFrame { data: Arc::from(frame.data), config: if key { config.clone() } else { None } })),
            pts: n as f64 / fps as f64,
            key,
        };
        writer_gone = out.send(WriterCommand::Media(media)).is_err();
    }
}

#[cfg(test)]
mod tests {
    use super::super::gpu;
    use super::super::gpu_frame::nv12_len;
    use super::*;

    /// The frames the writer got, as H.264 (Annex B) for ffmpeg, and their times.
    fn stream(rx: &Receiver<WriterCommand>) -> (Vec<u8>, Vec<(f64, bool)>) {
        let (mut out, mut frames) = (Vec::new(), Vec::new());
        for command in rx.try_iter() {
            let WriterCommand::Media(Media::Video { frame, pts, key }) = command else { continue };
            if let Some(c) = &frame.0.config {
                for nal in [&c.sps, &c.pps] {
                    out.extend_from_slice(&[0, 0, 0, 1]);
                    out.extend_from_slice(nal);
                }
            }
            let mut d = &frame.0.data[..];
            while d.len() >= 4 {
                let n = u32::from_be_bytes([d[0], d[1], d[2], d[3]]) as usize;
                out.extend_from_slice(&[0, 0, 0, 1]);
                out.extend_from_slice(&d[4..4 + n]);
                d = &d[4 + n..];
            }
            frames.push((pts, key));
        }
        (out, frames)
    }

    /// What the card's encoder makes through Vulkan Video of frames made on
    /// the card is what went in, give or take a little compression, across
    /// two GOPs and a size that isn't whole macroblocks (cropped); frame N
    /// is at N/fps, the first a keyframe with the parameter sets.
    #[test]
    #[ignore = "uses the card's video encoder: cargo test -p capture vulkan_video -- --ignored"]
    fn vulkan_video_encodes_what_it_is_given() {
        let card = gpu::gpu().expect("a graphics card");
        if card.video.is_none() {
            eprintln!("no Vulkan Video encoder: nothing to test");
            return;
        }
        let place = crate::ScreenPlacement::whole(crate::Framing::Fit);
        let (w, h, fps, n) = (640u32, 360u32, 30u32, 70u32);
        let mut pic = GpuPicture::new(card.clone(), w, h, place);
        let (tx, rx) = mpsc::channel();
        let enc = VkEncoder::start(w, h, fps, 20_000, tx, &pic, false).unwrap().expect("an encoder");
        let mut wanted = Vec::new();
        for i in 0..n {
            assert!(pic.draw(&check_picture(w, h, i), i as u64 + 1));
            let mut want = Vec::new();
            pic.nv12(&mut want);
            wanted.push(want);
            let until = Instant::now() + Duration::from_secs(5);
            while !enc.encode_gpu(&pic, i as u64, i as i64) {
                assert!(Instant::now() < until, "the encoder took no frame for 5 s");
                std::thread::sleep(Duration::from_micros(200));
            }
        }
        enc.finish();
        let (h264, frames) = stream(&rx);
        assert_eq!(frames.len(), n as usize, "every frame came out");
        let keys: Vec<usize> = frames.iter().enumerate().filter(|(_, f)| f.1).map(|(i, _)| i).collect();
        assert_eq!(keys, vec![0, 30, 60], "a keyframe every second");
        for (i, (pts, _)) in frames.iter().enumerate() {
            assert!((pts - i as f64 / fps as f64).abs() < 1e-9, "frame {i} at {pts}");
        }
        let dir = std::env::temp_dir().join(format!("hc-vkenc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("out.h264");
        std::fs::write(&file, &h264).unwrap();
        let decoded = std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-f", "h264", "-i"])
            .arg(&file)
            .args(["-f", "rawvideo", "-pix_fmt", "nv12", "-"])
            .output()
            .unwrap();
        let probe = std::process::Command::new("ffprobe")
            .args(["-v", "error", "-show_entries", "stream=width,height,profile,level,color_space,color_primaries,color_transfer,color_range", "-of", "compact=p=0"])
            .arg(&file)
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        eprintln!("{}", String::from_utf8_lossy(&probe.stdout).trim());
        assert!(decoded.status.success(), "ffmpeg reads it: {}", String::from_utf8_lossy(&decoded.stderr));
        let size = nv12_len(w, h);
        assert_eq!(decoded.stdout.len(), size * n as usize, "{w}x{h}, every frame");
        for (i, (got, want)) in decoded.stdout.chunks(size).zip(&wanted).enumerate() {
            let off: u64 = got.iter().zip(want).map(|(a, b)| (*a as i32 - *b as i32).unsigned_abs() as u64).sum();
            let mean = off as f64 / size as f64;
            assert!(mean < 2.0, "frame {i} off by {mean:.2} on average");
        }
        eprintln!("{n} frames match");
    }

    /// How many frames a second the card encodes through Vulkan Video, and
    /// the CPU time each costs us, with 8 different pictures already on the
    /// card (as `nvenc::tests::nvenc_throughput`).
    #[test]
    #[ignore = "uses the card's video encoder, takes a minute: cargo test --release -p capture vulkan_video_throughput -- --ignored --nocapture"]
    fn vulkan_video_throughput() {
        let card = gpu::gpu().expect("a graphics card");
        if card.video.is_none() {
            eprintln!("no Vulkan Video encoder");
            return;
        }
        let place = crate::ScreenPlacement::whole(crate::Framing::Fit);
        let cpu_time = || {
            let mut u = unsafe { std::mem::zeroed::<libc::rusage>() };
            // SAFETY: a valid rusage to fill.
            unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
            (u.ru_utime.tv_sec + u.ru_stime.tv_sec) as f64 + (u.ru_utime.tv_usec + u.ru_stime.tv_usec) as f64 / 1e6
        };
        for (w, h) in [(1920u32, 1080u32), (2560, 1440)] {
            let textures: Vec<Frame> = (0..8)
                .map(|k| {
                    let t = card.device.create_texture(&wgpu::TextureDescriptor {
                        label: None,
                        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format: wgpu::TextureFormat::Bgra8Unorm,
                        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                        view_formats: &[],
                    });
                    let data: Vec<u8> = (0..w * h)
                        .flat_map(|p| {
                            let (x, y) = (p % w, p / w);
                            [((x + y) / 4 + k * 9) as u8, (y / 2 + k * 5) as u8, ((x ^ y) / 8 + k * 3) as u8, 255]
                        })
                        .collect();
                    card.queue.write_texture(
                        t.as_image_copy(),
                        &data,
                        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: Some(h) },
                        wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                    );
                    Frame { width: w, height: h, stride: 0, order: Order::Bgr, data: Vec::new(), texture: Some(t) }
                })
                .collect();
            let mut pic = GpuPicture::new(card.clone(), w, h, place);
            let (tx, rx) = mpsc::channel();
            let enc = VkEncoder::start(w, h, 540, 50_000, tx, &pic, false).unwrap().expect("an encoder");
            let frames = 1620u32;
            let (t0, c0) = (Instant::now(), cpu_time());
            let mut waited = 0u32;
            for i in 0..frames {
                assert!(pic.draw(&textures[(i % 8) as usize], i as u64 + 1));
                while !enc.encode_gpu(&pic, i as u64, i as i64) {
                    waited += 1;
                    std::thread::sleep(Duration::from_micros(100));
                }
            }
            enc.finish();
            let (secs, cpu) = (t0.elapsed().as_secs_f64(), cpu_time() - c0);
            let out = rx.try_iter().filter(|c| matches!(c, WriterCommand::Media(_))).count();
            assert_eq!(out, frames as usize);
            eprintln!(
                "{w}x{h}: {:.0} fps, {:.2} ms CPU a frame ({:.0}% of a core at 540 fps), waited for a slot {waited} times",
                frames as f64 / secs,
                cpu * 1000.0 / frames as f64,
                cpu * 1000.0 / frames as f64 * 54.0
            );
        }
    }
}
