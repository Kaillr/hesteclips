//! The GPU side of video: a D3D11 device, Windows Graphics Capture of a
//! monitor, and conversion of captured frames to NV12 at the output size with
//! the GPU's video processor (scaling and color conversion in one pass, no CPU
//! copies).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709, DXGI_FORMAT_B8G8R8A8_UNORM,
    DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::core::{IInspectable, Interface};

/// A COM object moved to another thread. D3D11 objects are free-threaded
/// (the device is multithread-protected); WinRT capture objects are agile.
pub(crate) struct Shared<T>(pub T);
unsafe impl<T> Send for Shared<T> {}
unsafe impl<T> Sync for Shared<T> {}

/// The D3D11 device everything video runs on.
#[derive(Clone)]
pub(crate) struct Gpu {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
}
unsafe impl Send for Gpu {}
unsafe impl Sync for Gpu {}

impl Gpu {
    pub(crate) fn new() -> Result<Self> {
        let mut device = None;
        let mut context = None;
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .context("couldn't open the graphics card (Direct3D 11)")?;
        }
        let (device, context) = (device.context("no D3D11 device")?, context.context("no D3D11 context")?);
        // Capture callbacks, the pacer and the encoder all use the one immediate
        // context; let D3D serialize them.
        if let Ok(mt) = device.cast::<ID3D11Multithread>() {
            unsafe {
                let _ = mt.SetMultithreadProtected(true);
            }
        }
        Ok(Self { device, context })
    }

    /// The adapter's LUID, to pick the hardware encoder on the same GPU.
    pub(crate) fn adapter_luid(&self) -> Option<u64> {
        let dxgi: IDXGIDevice = self.device.cast().ok()?;
        let desc = unsafe { dxgi.GetAdapter().ok()?.GetDesc().ok()? };
        Some(((desc.AdapterLuid.HighPart as u32 as u64) << 32) | desc.AdapterLuid.LowPart as u64)
    }

    pub(crate) fn texture(&self, width: u32, height: u32, format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT, bind: u32, usage: D3D11_USAGE, cpu: u32) -> Result<ID3D11Texture2D> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: usage,
            BindFlags: bind,
            CPUAccessFlags: cpu,
            MiscFlags: 0,
        };
        let mut tex = None;
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut tex))? };
        tex.context("no texture")
    }
}

// ---------------------------------------------------------------------------
// Windows Graphics Capture
// ---------------------------------------------------------------------------

/// The newest captured screen image, copied out of WGC's frame pool so the pool
/// can keep going. Read by the pacer.
pub(crate) struct Latest {
    pub texture: ID3D11Texture2D,
    pub width: u32,
    pub height: u32,
    has_frame: AtomicBool,
}
unsafe impl Send for Latest {}
unsafe impl Sync for Latest {}

impl Latest {
    pub(crate) fn has_frame(&self) -> bool {
        self.has_frame.load(Ordering::Acquire)
    }
}

/// A running capture of one monitor.
pub(crate) struct Capture {
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    pub latest: Arc<Latest>,
}
unsafe impl Send for Capture {}

pub(crate) fn capture_item(monitor: HMONITOR) -> Result<GraphicsCaptureItem> {
    if !GraphicsCaptureSession::IsSupported().unwrap_or(false) {
        bail!("screen capture needs Windows 10 version 1903 or later");
    }
    let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
    unsafe { interop.CreateForMonitor::<GraphicsCaptureItem>(monitor) }.context("can't capture this display")
}

impl Capture {
    pub(crate) fn start(gpu: &Gpu, item: &GraphicsCaptureItem) -> Result<Self> {
        let size = item.Size()?;
        let (width, height) = (size.Width.max(2) as u32, size.Height.max(2) as u32);
        let latest = Arc::new(Latest {
            texture: gpu.texture(width, height, DXGI_FORMAT_B8G8R8A8_UNORM, (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32, D3D11_USAGE_DEFAULT, 0)?,
            width,
            height,
            has_frame: AtomicBool::new(false),
        });
        let dxgi: IDXGIDevice = gpu.device.cast()?;
        let winrt_device: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? }.cast()?;
        let format = DirectXPixelFormat::B8G8R8A8UIntNormalized;
        // Free-threaded: frames arrive on a system thread, no message loop needed.
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&winrt_device, format, 2, size)?;
        let session = pool.CreateCaptureSession(item)?;
        let _ = session.SetIsCursorCaptureEnabled(true);
        // Windows 11: no yellow "being captured" border around the screen.
        let _ = session.SetIsBorderRequired(false);

        let pool_size = Mutex::new(size);
        let latest2 = latest.clone();
        let gpu2 = gpu.clone();
        let device2 = Shared(winrt_device);
        pool.FrameArrived(&TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(move |pool, _| {
            // Whole captures: the wrappers are what make these Send.
            let (gpu2, device2) = (&gpu2, &device2);
            let Some(pool) = pool.as_ref() else { return Ok(()) };
            let frame = pool.TryGetNextFrame()?;
            let content = frame.ContentSize()?;
            let copied = (|| -> windows::core::Result<()> {
                let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
                let tex: ID3D11Texture2D = unsafe { access.GetInterface()? };
                let (w, h) = (
                    (content.Width.max(0) as u32).min(latest2.width),
                    (content.Height.max(0) as u32).min(latest2.height),
                );
                let region = D3D11_BOX { left: 0, top: 0, front: 0, right: w, bottom: h, back: 1 };
                unsafe { gpu2.context.CopySubresourceRegion(&latest2.texture, 0, 0, 0, 0, &tex, 0, Some(&region)) };
                Ok(())
            })();
            let _ = frame.Close();
            if copied.is_ok() {
                latest2.has_frame.store(true, Ordering::Release);
            }
            // The display changed size: resize the pool to match.
            let mut size = pool_size.lock().unwrap();
            if content.Width != size.Width || content.Height != size.Height {
                *size = SizeInt32 { Width: content.Width.max(2), Height: content.Height.max(2) };
                let _ = pool.Recreate(&device2.0, format, 2, *size);
            }
            Ok(())
        }))?;
        session.StartCapture()?;
        Ok(Self { pool, session, latest })
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

// ---------------------------------------------------------------------------
// BGRA → NV12
// ---------------------------------------------------------------------------

/// NV12 textures the converter writes and the encoder reads. A slot is taken
/// for each frame and given back once the encoder is done with it.
pub(crate) struct TexturePool {
    pub textures: Vec<ID3D11Texture2D>,
    free: Mutex<Vec<usize>>,
}
unsafe impl Send for TexturePool {}
unsafe impl Sync for TexturePool {}

impl TexturePool {
    pub(crate) fn new(gpu: &Gpu, width: u32, height: u32, count: usize) -> Result<Arc<Self>> {
        let make = |bind: u32| (0..count).map(|_| gpu.texture(width, height, DXGI_FORMAT_NV12, bind, D3D11_USAGE_DEFAULT, 0)).collect::<Result<Vec<_>>>();
        // Hardware encoders like textures marked for them; not every driver allows it.
        let textures = make((D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_VIDEO_ENCODER.0) as u32)
            .or_else(|_| make(D3D11_BIND_RENDER_TARGET.0 as u32))
            .context("couldn't create video frames on the graphics card")?;
        Ok(Arc::new(Self { textures, free: Mutex::new((0..count).rev().collect()) }))
    }

    pub(crate) fn take(&self) -> Option<usize> {
        self.free.lock().unwrap().pop()
    }

    pub(crate) fn give(&self, slot: usize) {
        let mut free = self.free.lock().unwrap();
        if !free.contains(&slot) {
            free.push(slot);
        }
    }
}

/// Scales and converts the latest screen image into a pool slot.
pub(crate) struct Converter {
    gpu: Gpu,
    video: ID3D11VideoContext,
    processor: ID3D11VideoProcessor,
    input: ID3D11VideoProcessorInputView,
    outputs: Vec<ID3D11VideoProcessorOutputView>,
}
unsafe impl Send for Converter {}

impl Converter {
    pub(crate) fn new(gpu: &Gpu, source: &Latest, pool: &TexturePool, width: u32, height: u32, fps: u32) -> Result<Self> {
        let vdev: ID3D11VideoDevice = gpu.device.cast().context("the graphics card has no video processor")?;
        let video: ID3D11VideoContext = gpu.context.cast()?;
        let rate = DXGI_RATIONAL { Numerator: fps, Denominator: 1 };
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: source.width,
            InputHeight: source.height,
            OutputFrameRate: rate,
            OutputWidth: width,
            OutputHeight: height,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_QUALITY,
        };
        unsafe {
            let enumerator = vdev.CreateVideoProcessorEnumerator(&desc)?;
            let processor = vdev.CreateVideoProcessor(&enumerator, 0)?;
            let in_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 } },
            };
            let mut input = None;
            vdev.CreateVideoProcessorInputView(&source.texture, &enumerator, &in_desc, Some(&mut input))?;
            let out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
            };
            let mut outputs = Vec::new();
            for tex in &pool.textures {
                let mut view = None;
                vdev.CreateVideoProcessorOutputView(tex, &enumerator, &out_desc, Some(&mut view))?;
                outputs.push(view.context("no output view")?);
            }

            // sRGB desktop in, BT.709 limited range out — what the file says it is.
            if let Ok(video1) = video.cast::<ID3D11VideoContext1>() {
                video1.VideoProcessorSetStreamColorSpace1(&processor, 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
                video1.VideoProcessorSetOutputColorSpace1(&processor, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709);
            }
            let full = RECT { left: 0, top: 0, right: source.width as i32, bottom: source.height as i32 };
            let out = RECT { left: 0, top: 0, right: width as i32, bottom: height as i32 };
            video.VideoProcessorSetStreamFrameFormat(&processor, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            video.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&full));
            video.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&out));
            video.VideoProcessorSetOutputTargetRect(&processor, true, Some(&out));
            video.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            Ok(Self { gpu: gpu.clone(), video, processor, input: input.context("no input view")?, outputs })
        }
    }

    /// Convert the latest screen image into pool slot `slot`.
    pub(crate) fn convert(&self, slot: usize) -> Result<()> {
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            ppPastSurfaces: std::ptr::null_mut(),
            pInputSurface: std::mem::ManuallyDrop::new(Some(self.input.clone())),
            ppFutureSurfaces: std::ptr::null_mut(),
            ppPastSurfacesRight: std::ptr::null_mut(),
            pInputSurfaceRight: std::mem::ManuallyDrop::new(None),
            ppFutureSurfacesRight: std::ptr::null_mut(),
        };
        let mut streams = [stream];
        let result = unsafe { self.video.VideoProcessorBlt(&self.processor, &self.outputs[slot], 0, &streams) };
        unsafe {
            std::mem::ManuallyDrop::drop(&mut streams[0].pInputSurface);
            self.gpu.context.Flush();
        }
        result.context("couldn't convert a screen frame")
    }
}
