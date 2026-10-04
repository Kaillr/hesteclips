//! Video decoding for playback, in-process: Media Foundation's source reader
//! with the GPU's hardware decoder, then our own D3D11 video processor pass
//! that scales the NV12 frame and converts it to full-range RGBA. One reader
//! stays open per clip, so playing, seeking and stepping never start anything
//! new (an ffmpeg process per play took 120-500 ms to show its first frame;
//! this takes a few ms per frame in order).
//!
//! The source reader's own video processing ("advanced video processing")
//! isn't used: it re-times frames to an even rate (clip length / frame
//! count), so in a recording with dropped frames every timestamp drifts,
//! by 50 frames at 30 s in one real clip. The decoder's own output keeps the
//! file's timestamps.
//!
//! Frames either come back to the CPU as RGBA, or (`share_on`) stay on the
//! GPU in a pool of shareable textures that the app's renderer opens once and
//! draws directly: no copy back, no upload.
//!
//! Frames are numbered by time from the start of the clip (as the editor and
//! the scrub proxy count them). Media Foundation shifts every timestamp by the
//! stream's reordering delay (B-frames), so the first frame's time is zero.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::{CloseHandle, GENERIC_ALL, HANDLE, RECT};
use windows::Win32::Graphics::Dxgi::{DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE, IDXGIResource1};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_TYPE, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601,
    DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_RATIONAL,
};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::core::{GUID, HSTRING, Interface};

use super::d3d::Gpu;
use super::system::{com_init, mf_startup};

const STREAM: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
/// Media Foundation's time unit: 100 ns.
const UNITS: f64 = 10_000_000.0;
/// Further ahead than this, seeking beats decoding forward: a seek costs
/// ~55 ms plus decoding from the keyframe (~1.6 ms a frame), so past ~60
/// frames (1 s at 60 fps) it's the cheaper way on average.
const MAX_SKIP: u64 = 60;

/// Shareable textures decoded frames are drawn into (see [`Surfaces`]).
const POOL: usize = 24;

/// A decoded picture, opaque RGBA: in `rgba` (rows top to bottom, no
/// padding), or on the GPU in `gpu` (and `rgba` is empty).
pub struct Picture {
    /// Frame number from the start of the clip.
    pub index: u64,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub gpu: Option<Surface>,
}

/// A fixed set of shareable RGBA textures on the GPU. Each decoded frame takes
/// a free one and gives it back when its [`Surface`] is dropped; the one freed
/// longest ago is reused first, so a texture the screen may still be drawing
/// isn't overwritten.
pub struct Surfaces {
    /// Unique per pool, for caching what's been opened from it.
    pub id: usize,
    textures: Vec<ID3D11Texture2D>,
    handles: Vec<HANDLE>,
    free: Mutex<VecDeque<usize>>,
    /// Signalled after each frame is written, for the other device to wait
    /// on before reading it (with its NT handle). Without it the other API
    /// could draw a texture's previous contents (another, old frame).
    fence: Option<(ID3D11Fence, HANDLE)>,
    fence_value: std::sync::atomic::AtomicU64,
}
unsafe impl Send for Surfaces {}
unsafe impl Sync for Surfaces {}

impl Drop for Surfaces {
    fn drop(&mut self) {
        for h in self.handles.iter().chain(self.fence.as_ref().map(|(_, h)| h)) {
            unsafe {
                let _ = CloseHandle(*h);
            }
        }
    }
}

/// One texture of a [`Surfaces`] pool, holding a frame.
pub struct Surface {
    pool: Arc<Surfaces>,
    slot: usize,
    /// The fence value signalled once this frame was written.
    ready: u64,
}

impl Surface {
    /// The pool and slot: the same texture every time, so open it once.
    pub fn key(&self) -> (usize, usize) {
        (self.pool.id, self.slot)
    }

    /// An NT handle to the texture, to open it on another device (D3D12).
    /// Owned by the pool; valid while it lives.
    pub fn handle(&self) -> isize {
        self.pool.handles[self.slot].0 as isize
    }

    /// The pool's shared fence (NT handle, owned by the pool) and the value
    /// that means this frame is written: wait for it before drawing it.
    /// `None` if the graphics card couldn't share a fence (then the frame was
    /// finished before it was handed over).
    pub fn fence(&self) -> Option<(isize, u64)> {
        self.pool.fence.as_ref().map(|(_, h)| (h.0 as isize, self.ready))
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        self.pool.free.lock().unwrap().push_back(self.slot);
    }
}

pub struct Decoder {
    reader: IMFSourceReader,
    _manager: IMFDXGIDeviceManager,
    gpu: Gpu,
    /// Output size.
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    convert: Convert,
    /// Frames stay on the GPU, in these.
    pool: Option<Arc<Surfaces>>,
    /// Media Foundation's time of frame 0.
    offset: i64,
    /// The frame the next read returns, when known (none after a seek).
    next_index: Option<u64>,
    /// A frame already read but not yet handed out.
    peeked: Option<(u64, IMFSample)>,
}

// The reader is used from one thread at a time, which owns the decoder.
unsafe impl Send for Decoder {}

impl Decoder {
    /// Open `path`, decoding to `width` wide (height keeps the aspect, even).
    /// With `share_on` (a graphics card's LUID), frames stay on that card in
    /// shareable textures instead of coming back as RGBA.
    pub fn open(path: &Path, width: u32, share_on: Option<u64>) -> Result<Self> {
        com_init();
        mf_startup()?;
        let gpu = match share_on {
            Some(luid) => Gpu::on_adapter(luid)?,
            None => Gpu::new()?,
        };
        if share_on.is_some_and(|l| gpu.adapter_luid() != Some(l)) {
            bail!("that graphics card isn't available to share frames on");
        }
        unsafe {
            let mut token = 0u32;
            let mut manager = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
            let manager = manager.context("no device manager")?;
            manager.ResetDevice(&gpu.device, token)?;

            let mut attrs = None;
            MFCreateAttributes(&mut attrs, 3)?;
            let attrs = attrs.context("no attributes")?;
            attrs.SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, &manager)?;
            attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
            let reader = MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), &attrs).context("couldn't open the video")?;
            reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
            reader.SetStreamSelection(STREAM, true)?;

            let native = reader.GetNativeMediaType(STREAM, 0)?;
            let size = native.GetUINT64(&MF_MT_FRAME_SIZE)?;
            let (sw, sh) = ((size >> 32) as u32, size as u32);
            let rate = native.GetUINT64(&MF_MT_FRAME_RATE).unwrap_or(60 << 32 | 1);
            let fps = (rate >> 32) as f64 / (rate as u32).max(1) as f64;
            let height = (((width as f64) * sh as f64 / sw.max(1) as f64 / 2.0).round() as u32 * 2).max(2);
            // SD video is usually BT.601; everything else BT.709.
            let bt601 = native.GetUINT32(&MF_MT_YUV_MATRIX).ok() == Some(MFVideoTransferMatrix_BT601.0 as u32);

            // The decoder's own output: NV12 textures on the GPU, with the file's times.
            let out = MFCreateMediaType()?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            reader.SetCurrentMediaType(STREAM, None, &out).context("the video can't be decoded to NV12")?;

            let pool = match share_on {
                Some(_) => Some(Arc::new(surfaces(&gpu, width, height)?)),
                None => None,
            };
            let targets: Vec<ID3D11Texture2D> = match &pool {
                Some(p) => p.textures.clone(),
                None => vec![gpu.texture(width, height, DXGI_FORMAT_R8G8B8A8_UNORM, D3D11_BIND_RENDER_TARGET.0 as u32, D3D11_USAGE_DEFAULT, 0)?],
            };
            let convert = Convert::new(&gpu, (sw, sh), (width, height), fps, bt601, targets)?;
            let mut d = Self { reader, _manager: manager, gpu, width, height, fps, convert, pool, offset: 0, next_index: None, peeked: None };
            // The first frame's time is frame 0's; keep the frame for the first read.
            let (time, sample) = d.read_sample()?.context("the video has no frames")?;
            d.offset = time;
            d.peeked = Some((0, sample));
            d.next_index = Some(0);
            Ok(d)
        }
    }

    /// Number frames on this rate's grid instead of the file's own (to match
    /// how the app counts them, from ffprobe's average rate).
    pub fn set_fps(&mut self, fps: f64) {
        if fps > 0.0 {
            self.fps = fps;
        }
    }

    /// The frame showing at `index`: the last one at or before it, so a
    /// dropped frame's slot shows the frame before the gap (as the scrub proxy
    /// and playback do). Decoded forward from where the reader is when that's
    /// close, else from the keyframe before it. `None` past the end.
    pub fn frame(&mut self, index: u64) -> Result<Option<Picture>> {
        let near = self.next_index.is_some_and(|n| n <= index && index - n <= MAX_SKIP);
        if !near {
            self.seek(index)?;
        }
        let mut last: Option<(u64, IMFSample)> = None;
        loop {
            match self.read()? {
                Some((i, sample)) if i <= index => last = Some((i, sample)),
                Some((i, sample)) => {
                    // Past it: that's the answer's end. Keep this one for the
                    // next read (it's the next frame, needed anyway).
                    let Some((li, ls)) = last else { return self.picture(i, &sample).map(Some) };
                    self.peeked = Some((i, sample));
                    self.next_index = Some(i);
                    return self.picture(li, &ls).map(Some);
                }
                None => return last.map(|(i, s)| self.picture(i, &s)).transpose(),
            }
        }
    }

    /// The frame after the last one read, with its pixels. `None` at the end.
    pub fn next(&mut self) -> Result<Option<Picture>> {
        match self.read()? {
            Some((i, sample)) => self.picture(i, &sample).map(Some),
            None => Ok(None),
        }
    }

    fn seek(&mut self, index: u64) -> Result<()> {
        self.peeked = None;
        self.next_index = None;
        let t = (index as f64 / self.fps * UNITS) as i64 + self.offset;
        let pos = PROPVARIANT::from(t);
        unsafe { self.reader.SetCurrentPosition(&GUID::zeroed(), &pos)? };
        Ok(())
    }

    /// The next sample in presentation order, numbered.
    fn read(&mut self) -> Result<Option<(u64, IMFSample)>> {
        if let Some(p) = self.peeked.take() {
            self.next_index = Some(p.0 + 1);
            return Ok(Some(p));
        }
        let Some((time, sample)) = self.read_sample()? else {
            self.next_index = None;
            return Ok(None);
        };
        // The frame whose slot on the grid contains this time, rounding down
        // like the app does (a frame a hair early still counts as its own).
        let index = (((time - self.offset) as f64 / UNITS) * self.fps + 1e-6).floor().max(0.0) as u64;
        self.next_index = Some(index + 1);
        Ok(Some((index, sample)))
    }

    fn read_sample(&mut self) -> Result<Option<(i64, IMFSample)>> {
        loop {
            let mut flags = 0u32;
            let mut time = 0i64;
            let mut sample = None;
            unsafe { self.reader.ReadSample(STREAM, 0, None, Some(&mut flags), Some(&mut time), Some(&mut sample))? };
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                return Ok(None);
            }
            if let Some(s) = sample {
                return Ok(Some((time, s)));
            }
        }
    }

    /// Convert a decoded frame and copy it off the GPU.
    fn picture(&mut self, index: u64, sample: &IMFSample) -> Result<Picture> {
        let (texture, slice) = unsafe {
            let buffer = sample.GetBufferByIndex(0)?;
            let dxgi: IMFDXGIBuffer = buffer.cast().context("the decoder didn't give a GPU frame")?;
            let mut texture: Option<ID3D11Texture2D> = None;
            dxgi.GetResource(&ID3D11Texture2D::IID, &mut texture as *mut _ as *mut _)?;
            (texture.context("no texture")?, dxgi.GetSubresourceIndex()?)
        };
        let Some(pool) = self.pool.clone() else {
            self.convert.run(&texture, slice, 0)?;
            let rgba = self.convert.read(&self.gpu)?;
            return Ok(Picture { index, width: self.width, height: self.height, rgba, gpu: None });
        };
        // A free texture: there's always one unless the app holds on to frames.
        let deadline = Instant::now() + Duration::from_secs(2);
        let slot = loop {
            if let Some(s) = pool.free.lock().unwrap().pop_front() {
                break s;
            }
            if Instant::now() > deadline {
                bail!("no free frame texture");
            }
            std::thread::sleep(Duration::from_millis(2));
        };
        self.convert.run(&texture, slice, slot)?;
        let ready = match &pool.fence {
            // The other device waits for this value before reading the frame.
            Some((fence, _)) => {
                let v = pool.fence_value.fetch_add(1, Ordering::Relaxed) + 1;
                unsafe {
                    let ctx4: ID3D11DeviceContext4 = self.gpu.context.cast()?;
                    ctx4.Signal(fence, v)?;
                    self.gpu.context.Flush();
                }
                v
            }
            // No shared fence: finish writing here before handing it over.
            None => {
                self.gpu.wait_idle()?;
                0
            }
        };
        let surface = Surface { pool, slot, ready };
        Ok(Picture { index, width: self.width, height: self.height, rgba: Vec::new(), gpu: Some(surface) })
    }
}

/// The shared textures for a pool.
fn surfaces(gpu: &Gpu, width: u32, height: u32) -> Result<Surfaces> {
    static NEXT: AtomicUsize = AtomicUsize::new(1);
    let mut textures = Vec::new();
    let mut handles = Vec::new();
    for _ in 0..POOL {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: (D3D11_RESOURCE_MISC_SHARED.0 | D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0) as u32,
        };
        unsafe {
            let mut tex = None;
            gpu.device.CreateTexture2D(&desc, None, Some(&mut tex))?;
            let tex = tex.context("no texture")?;
            let res: IDXGIResource1 = tex.cast()?;
            let handle = res.CreateSharedHandle(None, (DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE).0 | GENERIC_ALL.0, None)?;
            textures.push(tex);
            handles.push(handle);
        }
    }
    let fence = unsafe {
        (|| -> Result<(ID3D11Fence, HANDLE)> {
            let device5: ID3D11Device5 = gpu.device.cast()?;
            let mut fence: Option<ID3D11Fence> = None;
            device5.CreateFence(0, D3D11_FENCE_FLAG_SHARED, &mut fence)?;
            let fence = fence.context("no fence")?;
            let handle = fence.CreateSharedHandle(None, GENERIC_ALL.0, None)?;
            Ok((fence, handle))
        })()
        .inspect_err(|e| eprintln!("decoder: no shared fence, waiting on the CPU instead: {e:#}"))
        .ok()
    };
    Ok(Surfaces {
        id: NEXT.fetch_add(1, Ordering::Relaxed),
        textures,
        handles,
        free: Mutex::new((0..POOL).collect()),
        fence,
        fence_value: std::sync::atomic::AtomicU64::new(0),
    })
}

/// The GPU pass from a decoded NV12 frame to RGBA at the output size, and the
/// staging texture it's read back through.
struct Convert {
    video: ID3D11VideoContext,
    vdev: ID3D11VideoDevice,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    /// What it draws into: one texture (read back) or the shared pool's.
    targets: Vec<ID3D11Texture2D>,
    outputs: Vec<ID3D11VideoProcessorOutputView>,
    staging: ID3D11Texture2D,
    /// Input views by decoder texture and array slice (the decoder cycles
    /// through a fixed set of surfaces).
    inputs: HashMap<(usize, u32), ID3D11VideoProcessorInputView>,
    source: (u32, u32),
    size: (u32, u32),
}

impl Convert {
    fn new(gpu: &Gpu, source: (u32, u32), size: (u32, u32), fps: f64, bt601: bool, targets: Vec<ID3D11Texture2D>) -> Result<Self> {
        let vdev: ID3D11VideoDevice = gpu.device.cast().context("the graphics card has no video processor")?;
        let video: ID3D11VideoContext = gpu.context.cast()?;
        let rate = DXGI_RATIONAL { Numerator: fps.round().max(1.0) as u32, Denominator: 1 };
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: source.0,
            InputHeight: source.1,
            OutputFrameRate: rate,
            OutputWidth: size.0,
            OutputHeight: size.1,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_QUALITY,
        };
        unsafe {
            let enumerator = vdev.CreateVideoProcessorEnumerator(&desc)?;
            let processor = vdev.CreateVideoProcessor(&enumerator, 0)?;
            let staging = gpu.texture(size.0, size.1, DXGI_FORMAT_R8G8B8A8_UNORM, 0, D3D11_USAGE_STAGING, D3D11_CPU_ACCESS_READ.0 as u32)?;
            let out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
            };
            let mut outputs = Vec::new();
            for t in &targets {
                let mut view = None;
                vdev.CreateVideoProcessorOutputView(t, &enumerator, &out_desc, Some(&mut view))?;
                outputs.push(view.context("no output view")?);
            }

            // Studio-range YCbCr in, full-range RGB out: what the file says it
            // is, and what the screen wants.
            let in_space: DXGI_COLOR_SPACE_TYPE = if bt601 { DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601 } else { DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709 };
            let video1: ID3D11VideoContext1 = video.cast().context("the graphics card's video processor is too old")?;
            video1.VideoProcessorSetStreamColorSpace1(&processor, 0, in_space);
            video1.VideoProcessorSetOutputColorSpace1(&processor, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
            let src = RECT { left: 0, top: 0, right: source.0 as i32, bottom: source.1 as i32 };
            let dst = RECT { left: 0, top: 0, right: size.0 as i32, bottom: size.1 as i32 };
            video.VideoProcessorSetStreamFrameFormat(&processor, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            video.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&src));
            video.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&dst));
            video.VideoProcessorSetOutputTargetRect(&processor, true, Some(&dst));
            video.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            video.VideoProcessorSetOutputAlphaFillMode(&processor, D3D11_VIDEO_PROCESSOR_ALPHA_FILL_MODE_OPAQUE, 0);
            Ok(Self { video, vdev, enumerator, processor, targets, outputs, staging, inputs: HashMap::new(), source, size })
        }
    }

    /// Convert array slice `slice` of the decoder's `texture` into target `to`.
    fn run(&mut self, texture: &ID3D11Texture2D, slice: u32, to: usize) -> Result<()> {
        let key = (texture.as_raw() as usize, slice);
        if !self.inputs.contains_key(&key) {
            let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: slice } },
            };
            let mut view = None;
            unsafe { self.vdev.CreateVideoProcessorInputView(texture, &self.enumerator, &desc, Some(&mut view))? };
            // A new decoder (after some seeks) means new surfaces: drop stale views.
            if self.inputs.len() > 64 {
                self.inputs.clear();
            }
            self.inputs.insert(key, view.context("no input view")?);
        }
        let input = self.inputs[&key].clone();
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            ppPastSurfaces: std::ptr::null_mut(),
            pInputSurface: std::mem::ManuallyDrop::new(Some(input)),
            ppFutureSurfaces: std::ptr::null_mut(),
            ppPastSurfacesRight: std::ptr::null_mut(),
            pInputSurfaceRight: std::mem::ManuallyDrop::new(None),
            ppFutureSurfacesRight: std::ptr::null_mut(),
        };
        let mut streams = [stream];
        let result = unsafe { self.video.VideoProcessorBlt(&self.processor, &self.outputs[to], 0, &streams) };
        unsafe { std::mem::ManuallyDrop::drop(&mut streams[0].pInputSurface) };
        result?;
        let _ = self.source;
        Ok(())
    }

    /// The (single) target's pixels: tight RGBA rows.
    fn read(&self, gpu: &Gpu) -> Result<Vec<u8>> {
        let (w, h) = self.size;
        let row = w as usize * 4;
        let mut rgba = vec![0u8; row * h as usize];
        unsafe {
            gpu.context.CopyResource(&self.staging, &self.targets[0]);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            gpu.context.Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            let src = mapped.pData as *const u8;
            if mapped.RowPitch as usize == row {
                std::ptr::copy_nonoverlapping(src, rgba.as_mut_ptr(), rgba.len());
            } else {
                for y in 0..h as usize {
                    std::ptr::copy_nonoverlapping(src.add(y * mapped.RowPitch as usize), rgba[y * row..].as_mut_ptr(), row);
                }
            }
            gpu.context.Unmap(&self.staging, 0);
        }
        if rgba.is_empty() {
            bail!("empty frame");
        }
        Ok(rgba)
    }
}
