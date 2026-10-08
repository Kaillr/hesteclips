//! The GPU side of video: a D3D11 device, Windows Graphics Capture of a
//! monitor or an app's window, and conversion of captured frames to NV12 at the
//! output size with the GPU's video processor (scaling, letterboxing and color
//! conversion in one pass, no CPU copies).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{HMODULE, HWND, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709, DXGI_FORMAT_B8G8R8A8_UNORM,
    DXGI_FORMAT_NV12, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
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
        Self::create(None)
    }

    /// On the graphics card with this LUID (to share textures with another
    /// API's device on it), else the default one.
    pub(crate) fn on_adapter(luid: u64) -> Result<Self> {
        use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter, IDXGIFactory1};
        let adapter = unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
            (0..)
                .map_while(|i| factory.EnumAdapters1(i).ok())
                .find(|a| a.GetDesc1().is_ok_and(|d| ((d.AdapterLuid.HighPart as u32 as u64) << 32 | d.AdapterLuid.LowPart as u64) == luid))
        };
        Self::create(adapter.and_then(|a| a.cast::<IDXGIAdapter>().ok()))
    }

    fn create(adapter: Option<windows::Win32::Graphics::Dxgi::IDXGIAdapter>) -> Result<Self> {
        let mut device = None;
        let mut context = None;
        // A specific adapter needs the "unknown" driver type.
        let driver = if adapter.is_some() { windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN } else { D3D_DRIVER_TYPE_HARDWARE };
        unsafe {
            D3D11CreateDevice(
                adapter.as_ref(),
                driver,
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

    /// Wait until the GPU has finished everything submitted so far.
    pub(crate) fn wait_idle(&self) -> Result<()> {
        unsafe {
            let desc = D3D11_QUERY_DESC { Query: D3D11_QUERY_EVENT, MiscFlags: 0 };
            let mut query = None;
            self.device.CreateQuery(&desc, Some(&mut query))?;
            let query = query.context("no query")?;
            self.context.End(&query);
            self.context.Flush();
            let mut done = 0i32;
            let start = std::time::Instant::now();
            // S_FALSE (not done yet) is a success too: the flag says when it's done.
            while self.context.GetData(&query, Some(&mut done as *mut i32 as *mut _), 4, 0).is_err() || done == 0 {
                if start.elapsed() > std::time::Duration::from_secs(1) {
                    bail!("the graphics card didn't finish");
                }
                // A frame's conversion takes a millisecond or two: sleep in short
                // steps rather than spin a core waiting for it.
                std::thread::sleep(std::time::Duration::from_micros(250));
            }
        }
        Ok(())
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

/// The newest captured image, copied out of WGC's frame pool so the pool can
/// keep going. Outlives any one capture: when a recorded app's window closes
/// and reopens, the new capture writes into the same texture. Read by the pacer.
pub(crate) struct Latest {
    pub texture: ID3D11Texture2D,
    pub width: u32,
    pub height: u32,
    /// The valid part of `texture`, from its top-left corner: the captured
    /// window's size, or the whole texture.
    content: Mutex<(u32, u32)>,
    has_frame: AtomicBool,
    /// Black because there's nothing to capture yet (no app is open).
    waiting: AtomicBool,
    /// The app being captured (its executable), when capturing apps.
    app: Mutex<Option<String>>,
    /// The last real picture, kept while a still (the away screen) covers it,
    /// so turning the away screen off can bring it back: a minimized window
    /// sends no new frames. With its valid size.
    saved: Mutex<Option<(ID3D11Texture2D, (u32, u32))>>,
    gpu: Gpu,
}
unsafe impl Send for Latest {}
unsafe impl Sync for Latest {}

impl Latest {
    pub(crate) fn new(gpu: &Gpu, width: u32, height: u32) -> Result<Arc<Self>> {
        let bind = (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32;
        Ok(Arc::new(Self {
            texture: gpu.texture(width, height, DXGI_FORMAT_B8G8R8A8_UNORM, bind, D3D11_USAGE_DEFAULT, 0)?,
            width,
            height,
            content: Mutex::new((width, height)),
            has_frame: AtomicBool::new(false),
            waiting: AtomicBool::new(false),
            app: Mutex::new(None),
            saved: Mutex::new(None),
            gpu: gpu.clone(),
        }))
    }

    pub(crate) fn has_frame(&self) -> bool {
        self.has_frame.load(Ordering::Acquire)
    }

    pub(crate) fn app(&self) -> Option<String> {
        self.app.lock().unwrap().clone()
    }

    pub(crate) fn set_app(&self, app: Option<String>) {
        *self.app.lock().unwrap() = app;
    }

    pub(crate) fn waiting(&self) -> bool {
        self.waiting.load(Ordering::Acquire)
    }

    pub(crate) fn content(&self) -> (u32, u32) {
        *self.content.lock().unwrap()
    }

    /// Show a still picture (the away screen), fitted to the frame like a
    /// window. Black if it doesn't fit (a display smaller than the picture).
    pub(crate) fn show_still(&self, image: &crate::StillImage) -> Result<()> {
        if image.width > self.width || image.height > self.height || image.bgra.len() < (image.width * image.height * 4) as usize {
            return self.clear();
        }
        // Covering a real picture: keep it, to bring back (`restore`).
        if self.has_frame.load(Ordering::Acquire) && !self.waiting.load(Ordering::Acquire) {
            let bind = (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32;
            let mut saved = self.saved.lock().unwrap();
            if saved.is_none() {
                *saved = Some((self.gpu.texture(self.width, self.height, DXGI_FORMAT_B8G8R8A8_UNORM, bind, D3D11_USAGE_DEFAULT, 0)?, (0, 0)));
            }
            if let Some((copy, size)) = saved.as_mut() {
                unsafe { self.gpu.context.CopyResource(&*copy, &self.texture) };
                *size = self.content();
            }
        }
        let region = D3D11_BOX { left: 0, top: 0, front: 0, right: image.width, bottom: image.height, back: 1 };
        unsafe {
            self.gpu.context.UpdateSubresource(&self.texture, 0, Some(&region), image.bgra.as_ptr().cast(), image.width * 4, 0);
        }
        *self.content.lock().unwrap() = (image.width, image.height);
        self.waiting.store(true, Ordering::Release);
        self.has_frame.store(true, Ordering::Release);
        Ok(())
    }

    /// Bring back the picture a still covered (see `saved`). False if there's
    /// none (nothing was showing before it).
    pub(crate) fn restore(&self) -> bool {
        let saved = self.saved.lock().unwrap();
        let Some((copy, size)) = saved.as_ref().filter(|(_, s)| s.0 > 0) else { return false };
        unsafe { self.gpu.context.CopyResource(&self.texture, copy) };
        *self.content.lock().unwrap() = *size;
        self.waiting.store(false, Ordering::Release);
        true
    }

    /// Nothing's being recorded, but the picture stays as it is: the last
    /// one shown (black if there's none, or if a still covered it and
    /// nothing was showing before).
    pub(crate) fn hold(&self) -> Result<()> {
        let still_up = self.waiting.load(Ordering::Acquire);
        if !self.has_frame.load(Ordering::Acquire) || still_up && !self.restore() {
            return self.clear();
        }
        self.waiting.store(true, Ordering::Release);
        Ok(())
    }

    /// A new picture from the CPU (a webcam frame): `width`×`height` BGRA rows
    /// `pitch` bytes apart, starting at `data`.
    pub(crate) fn upload_bgra(&self, data: *const u8, pitch: u32, width: u32, height: u32) {
        let (w, h) = (width.min(self.width), height.min(self.height));
        let region = D3D11_BOX { left: 0, top: 0, front: 0, right: w, bottom: h, back: 1 };
        unsafe { self.gpu.context.UpdateSubresource(&self.texture, 0, Some(&region), data.cast(), pitch, 0) };
        *self.content.lock().unwrap() = (w, h);
        self.waiting.store(false, Ordering::Release);
        self.has_frame.store(true, Ordering::Release);
    }

    /// Show black, as a full-size frame.
    pub(crate) fn clear(&self) -> Result<()> {
        unsafe {
            let mut view = None;
            self.gpu.device.CreateRenderTargetView(&self.texture, None, Some(&mut view))?;
            let view = view.context("no render target view")?;
            self.gpu.context.ClearRenderTargetView(&view, &[0.0, 0.0, 0.0, 1.0]);
        }
        *self.content.lock().unwrap() = (self.width, self.height);
        self.waiting.store(true, Ordering::Release);
        self.has_frame.store(true, Ordering::Release);
        Ok(())
    }
}

/// A running capture of a monitor or a window.
pub(crate) struct Capture {
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    item: GraphicsCaptureItem,
    closed_token: i64,
    closed: Arc<AtomicBool>,
}
unsafe impl Send for Capture {}

fn check_supported() -> Result<()> {
    if !GraphicsCaptureSession::IsSupported().unwrap_or(false) {
        bail!("screen capture needs Windows 10 version 1903 or later");
    }
    Ok(())
}

pub(crate) fn monitor_item(monitor: HMONITOR) -> Result<GraphicsCaptureItem> {
    check_supported()?;
    let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
    unsafe { interop.CreateForMonitor::<GraphicsCaptureItem>(monitor) }.context("can't capture this display")
}

pub(crate) fn window_item(window: HWND) -> Result<GraphicsCaptureItem> {
    check_supported()?;
    let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
    unsafe { interop.CreateForWindow::<GraphicsCaptureItem>(window) }.context("can't capture this window")
}

impl Capture {
    /// Capture `item` into `latest`, at most `fps` × 4 times a second. For a
    /// window, only its client area is kept (no title bar or borders).
    pub(crate) fn start(gpu: &Gpu, item: &GraphicsCaptureItem, latest: &Arc<Latest>, window: Option<HWND>, fps: u32) -> Result<Self> {
        let size = item.Size()?;
        let dxgi: IDXGIDevice = gpu.device.cast()?;
        let winrt_device: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? }.cast()?;
        let format = DirectXPixelFormat::B8G8R8A8UIntNormalized;
        // Free-threaded: frames arrive on a system thread, no message loop needed.
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&winrt_device, format, 2, size)?;
        let session = pool.CreateCaptureSession(item)?;
        // `HESTECLIPS_CURSOR_CAPTURE=0` leaves the cursor out: a test for a
        // cursor that vanishes in some games while they're captured.
        let cursor = std::env::var_os("HESTECLIPS_CURSOR_CAPTURE").is_none_or(|v| v != "0");
        let _ = session.SetIsCursorCaptureEnabled(cursor);
        // Windows 11: no yellow "being captured" border.
        let _ = session.SetIsBorderRequired(false);
        // Not every display refresh (540 a second on a 540 Hz monitor), just
        // enough for the recording: less work for Windows' compositor, which
        // games feel. Four times the recording rate: measured, Windows delivers
        // well under the limit (at half a frame period a 60 fps recording got
        // only 44 frames a second; at a quarter, 184). Windows 11 24H2 and
        // later; ignored before.
        let interval = 10_000_000 / (4 * fps.max(1) as i64);
        let _ = session.SetMinUpdateInterval(windows::Foundation::TimeSpan { Duration: interval });

        let pool_size = Mutex::new(size);
        let latest2 = latest.clone();
        let gpu2 = gpu.clone();
        let device2 = Shared(winrt_device);
        let window2 = Shared(window);
        pool.FrameArrived(&TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(move |pool, _| {
            // Whole captures: the wrappers are what make these Send.
            let (gpu2, device2, window2) = (&gpu2, &device2, &window2);
            let Some(pool) = pool.as_ref() else { return Ok(()) };
            let frame = pool.TryGetNextFrame()?;
            let content = frame.ContentSize()?;
            let copied = (|| -> windows::core::Result<Option<(u32, u32)>> {
                let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
                let tex: ID3D11Texture2D = unsafe { access.GetInterface()? };
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                unsafe { tex.GetDesc(&mut desc) };
                let (cw, ch) = (content.Width.max(0) as u32, content.Height.max(0) as u32);
                // The window just grew (restored from minimized, resized) and the
                // pool's buffers haven't caught up: this frame's texture is smaller
                // than its content. Copying the content's size would read past the
                // texture (which crashes the driver), so skip it; the pool is
                // resized below and the next frame is whole.
                if cw > desc.Width || ch > desc.Height {
                    return Ok(None);
                }
                // The part of the frame to keep: a window's client area, else all of it.
                let (x, y, w, h) = window2
                    .0
                    .and_then(super::system::client_area_in_capture)
                    .map(|(x, y, w, h)| (x.min(cw), y.min(ch), w.min(cw.saturating_sub(x)), h.min(ch.saturating_sub(y))))
                    .filter(|&(_, _, w, h)| w >= 2 && h >= 2)
                    .unwrap_or((0, 0, cw, ch));
                let (w, h) = (w.min(latest2.width), h.min(latest2.height));
                if w < 2 || h < 2 {
                    return Ok(None);
                }
                let region = D3D11_BOX { left: x, top: y, front: 0, right: x + w, bottom: y + h, back: 1 };
                unsafe { gpu2.context.CopySubresourceRegion(&latest2.texture, 0, 0, 0, 0, &tex, 0, Some(&region)) };
                Ok(Some((w, h)))
            })();
            let _ = frame.Close();
            if let Ok(Some(size)) = copied {
                *latest2.content.lock().unwrap() = size;
                latest2.waiting.store(false, Ordering::Release);
                latest2.has_frame.store(true, Ordering::Release);
            }
            // The display or window changed size: resize the pool to match.
            let mut size = pool_size.lock().unwrap();
            if content.Width != size.Width || content.Height != size.Height {
                *size = SizeInt32 { Width: content.Width.max(2), Height: content.Height.max(2) };
                let _ = pool.Recreate(&device2.0, format, 2, *size);
            }
            Ok(())
        }))?;
        // A captured window closing ends its capture.
        let closed = Arc::new(AtomicBool::new(false));
        let closed2 = closed.clone();
        let closed_token = item.Closed(&TypedEventHandler::<GraphicsCaptureItem, IInspectable>::new(move |_, _| {
            closed2.store(true, Ordering::Release);
            Ok(())
        }))?;
        session.StartCapture()?;
        Ok(Self { pool, session, item: item.clone(), closed_token, closed })
    }

    /// The captured window closed; this capture will deliver nothing more.
    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.item.RemoveClosed(self.closed_token);
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

/// Scales and converts the latest captured image into one of its targets,
/// fitted into the frame with black bars when its shape differs, then draws
/// the webcam over it if there is one. Targets are NV12 frames for the
/// encoder, or an RGB picture for the preview.
pub(crate) struct Converter {
    gpu: Gpu,
    width: u32,
    height: u32,
    video: ID3D11VideoContext,
    base: Layer,
    overlay: Option<(Layer, crate::webcam::SharedPlacement)>,
}
unsafe impl Send for Converter {}

/// One picture drawn into the targets: its own video processor (with state for
/// where it goes), reading `source`.
struct Layer {
    source: Arc<Latest>,
    processor: ID3D11VideoProcessor,
    input: ID3D11VideoProcessorInputView,
    outputs: Vec<ID3D11VideoProcessorOutputView>,
}

impl Layer {
    fn new(gpu: &Gpu, video: &ID3D11VideoContext, source: &Arc<Latest>, targets: &[ID3D11Texture2D], yuv: bool, width: u32, height: u32, fps: u32) -> Result<Self> {
        let vdev: ID3D11VideoDevice = gpu.device.cast().context("the graphics card has no video processor")?;
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
            for tex in targets {
                let mut view = None;
                vdev.CreateVideoProcessorOutputView(tex, &enumerator, &out_desc, Some(&mut view))?;
                outputs.push(view.context("no output view")?);
            }

            // sRGB in; BT.709 limited range out for the encoder (what the file
            // says it is), unchanged sRGB for the preview.
            if let Ok(video1) = video.cast::<ID3D11VideoContext1>() {
                let out_space = if yuv { DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709 } else { DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709 };
                video1.VideoProcessorSetStreamColorSpace1(&processor, 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
                video1.VideoProcessorSetOutputColorSpace1(&processor, out_space);
            }
            let out = RECT { left: 0, top: 0, right: width as i32, bottom: height as i32 };
            video.VideoProcessorSetStreamFrameFormat(&processor, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            video.VideoProcessorSetOutputTargetRect(&processor, true, Some(&out));
            // Black bars around a picture whose shape differs from the frame.
            let black = D3D11_VIDEO_COLOR { Anonymous: D3D11_VIDEO_COLOR_0 { RGBA: D3D11_VIDEO_COLOR_RGBA { R: 0.0, G: 0.0, B: 0.0, A: 1.0 } } };
            video.VideoProcessorSetOutputBackgroundColor(&processor, false, &black);
            video.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            video.VideoProcessorSetOutputAlphaFillMode(&processor, D3D11_VIDEO_PROCESSOR_ALPHA_FILL_MODE_OPAQUE, 0);
            Ok(Self { source: source.clone(), processor, input: input.context("no input view")?, outputs })
        }
    }

    /// Draw `source_rect` of the picture into `dest` of target `slot`, leaving
    /// the rest of the target as it is (only `target` is written).
    fn draw(&self, video: &ID3D11VideoContext, slot: usize, source_rect: RECT, dest: RECT, target: RECT, mirror: (bool, bool)) -> windows::core::Result<()> {
        unsafe {
            if let Ok(video1) = video.cast::<ID3D11VideoContext1>() {
                video1.VideoProcessorSetStreamMirror(&self.processor, 0, mirror.0 || mirror.1, mirror.0, mirror.1);
            }
            video.VideoProcessorSetOutputTargetRect(&self.processor, true, Some(&target));
            video.VideoProcessorSetStreamSourceRect(&self.processor, 0, true, Some(&source_rect));
            video.VideoProcessorSetStreamDestRect(&self.processor, 0, true, Some(&dest));
        }
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
        let result = unsafe { video.VideoProcessorBlt(&self.processor, &self.outputs[slot], 0, &streams) };
        unsafe { std::mem::ManuallyDrop::drop(&mut streams[0].pInputSurface) };
        result
    }
}

impl Converter {
    /// `overlay`: the webcam's picture and where it goes, drawn over `source`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        gpu: &Gpu,
        source: &Arc<Latest>,
        overlay: Option<(&Arc<Latest>, crate::webcam::SharedPlacement)>,
        targets: &[ID3D11Texture2D],
        yuv: bool,
        width: u32,
        height: u32,
        fps: u32,
    ) -> Result<Self> {
        let video: ID3D11VideoContext = gpu.context.cast()?;
        let base = Layer::new(gpu, &video, source, targets, yuv, width, height, fps)?;
        let overlay = match overlay {
            Some((cam, placement)) => Some((Layer::new(gpu, &video, cam, targets, yuv, width, height, fps)?, placement)),
            None => None,
        };
        Ok(Self { gpu: gpu.clone(), width, height, video, base, overlay })
    }

    /// Convert the latest captured image (and the webcam over it) into target `slot`.
    pub(crate) fn convert(&self, slot: usize) -> Result<()> {
        let (cw, ch) = self.base.source.content();
        let source = RECT { left: 0, top: 0, right: cw as i32, bottom: ch as i32 };
        let (x, y, w, h) = fit(cw, ch, self.width, self.height);
        let dest = RECT { left: x as i32, top: y as i32, right: (x + w) as i32, bottom: (y + h) as i32 };
        let full = RECT { left: 0, top: 0, right: self.width as i32, bottom: self.height as i32 };
        let result = self.base.draw(&self.video, slot, source, dest, full, (false, false)).context("couldn't convert a screen frame");
        if result.is_ok() {
            if let Some((layer, placement)) = &self.overlay {
                if layer.source.has_frame() {
                    let p = *placement.lock().unwrap();
                    if let Some((source, dest)) = overlay_rects(p, layer.source.content(), self.width, self.height) {
                        if let Err(e) = layer.draw(&self.video, slot, source, dest, dest, (p.flip_h, p.flip_v)) {
                            eprintln!("couldn't draw the webcam: {e}");
                        }
                    }
                }
            }
        }
        unsafe { self.gpu.context.Flush() };
        result
    }
}

/// Where the webcam's picture comes from and goes, in pixels, for a frame of
/// `width`×`height`: its crop, and its box clipped to the frame (cutting the
/// same share off the picture — from the other side when it's flipped, as the
/// mirror is applied after). None if nothing of it is in the frame.
fn overlay_rects(p: crate::webcam::Placement, (cw, ch): (u32, u32), width: u32, height: u32) -> Option<(RECT, RECT)> {
    let (fw, fh) = (width as f32, height as f32);
    // One axis: the visible part of the box [d0, d1] within [0, size], and the
    // matching part of the cropped picture [s0, s1].
    let axis = |d0: f32, d1: f32, size: f32, s0: f32, s1: f32, flip: bool| -> Option<(f32, f32, f32, f32)> {
        if d1 - d0 < 1.0 || s1 - s0 < 1.0 {
            return None;
        }
        let (v0, v1) = (d0.max(0.0), d1.min(size));
        if v1 - v0 < 1.0 {
            return None;
        }
        let (t0, t1) = ((v0 - d0) / (d1 - d0), (v1 - d0) / (d1 - d0));
        let span = s1 - s0;
        let (a, b) = if flip { (s1 - t1 * span, s1 - t0 * span) } else { (s0 + t0 * span, s0 + t1 * span) };
        Some((v0, v1, a, b))
    };
    // Never stretched: the camera's picture fills the box with its own shape.
    let [cl, ct, cr, cb] = p.fill_crop((cw, ch), (width, height));
    let (dx0, dx1, sx0, sx1) = axis(p.x * fw, (p.x + p.w) * fw, fw, cl * cw as f32, (1.0 - cr) * cw as f32, p.flip_h)?;
    let (dy0, dy1, sy0, sy1) = axis(p.y * fh, (p.y + p.h) * fh, fh, ct * ch as f32, (1.0 - cb) * ch as f32, p.flip_v)?;
    let r = |a: f32, b: f32, c: f32, d: f32| RECT { left: a.round() as i32, top: b.round() as i32, right: c.round() as i32, bottom: d.round() as i32 };
    let (source, dest) = (r(sx0, sy0, sx1, sy1), r(dx0, dy0, dx1, dy1));
    (dest.right - dest.left >= 2 && dest.bottom - dest.top >= 2 && source.right > source.left && source.bottom > source.top).then_some((source, dest))
}

/// Makes the preview picture of what's captured (see `crate::preview`), at the
/// recording's own size. The GPU scales and converts into a texture and copies
/// it to a CPU-readable one; a reader thread copies that out and publishes it,
/// so whoever calls [`Previewer::submit`] (the pacer) never waits. Two frames
/// can be on their way at once, and only while the UI keeps taking them, so
/// nothing is copied for nothing.
pub(crate) struct Previewer {
    converter: Converter,
    target: ID3D11Texture2D,
    slots: Vec<(ID3D11Texture2D, Arc<AtomicBool>)>,
    gpu: Gpu,
    tx: Option<std::sync::mpsc::Sender<usize>>,
    reader: Option<std::thread::JoinHandle<()>>,
}
unsafe impl Send for Previewer {}

impl Previewer {
    /// Frames on their way to the UI at once.
    const SLOTS: usize = 2;

    /// `generation` from `crate::preview::new_producer`, taken when the capture
    /// was asked for (so a slow start can't take over from a later one).
    pub(crate) fn new(
        gpu: &Gpu,
        latest: &Arc<Latest>,
        overlay: Option<(&Arc<Latest>, crate::webcam::SharedPlacement)>,
        width: u32,
        height: u32,
        fps: u32,
        generation: u64,
    ) -> Result<Self> {
        // RGBA straight from the GPU is what the UI takes; drivers that can't
        // write it get BGRA, swapped on the reader thread.
        let make = |format| -> Result<(ID3D11Texture2D, Vec<ID3D11Texture2D>, Converter)> {
            let target = gpu.texture(width, height, format, D3D11_BIND_RENDER_TARGET.0 as u32, D3D11_USAGE_DEFAULT, 0)?;
            let staging = (0..Self::SLOTS)
                .map(|_| gpu.texture(width, height, format, 0, D3D11_USAGE_STAGING, D3D11_CPU_ACCESS_READ.0 as u32))
                .collect::<Result<Vec<_>>>()?;
            let converter = Converter::new(gpu, latest, overlay.clone(), std::slice::from_ref(&target), false, width, height, fps)?;
            Ok((target, staging, converter))
        };
        let (rgba, (target, staging, converter)) = match make(DXGI_FORMAT_R8G8B8A8_UNORM) {
            Ok(t) => (true, t),
            Err(_) => (false, make(DXGI_FORMAT_B8G8R8A8_UNORM)?),
        };
        let slots: Vec<(ID3D11Texture2D, Arc<AtomicBool>)> = staging.into_iter().map(|t| (t, Arc::new(AtomicBool::new(false)))).collect();

        let (tx, rx) = std::sync::mpsc::channel::<usize>();
        let reader = {
            let gpu = gpu.clone();
            let latest = latest.clone();
            let slots = Shared(slots.clone());
            std::thread::spawn(move || {
                let slots = slots;
                let (w, h) = (width as usize, height as usize);
                let size = w * h * 4;
                // Frames recently handed out; one the UI has let go of is reused,
                // since a fresh 10+ MB buffer per frame is slow.
                let mut recent: std::collections::VecDeque<Arc<crate::preview::PreviewFrame>> = Default::default();
                while let Ok(slot) = rx.recv() {
                    let (staging, busy) = &slots.0[slot];
                    let reusable = recent.iter().position(|f| Arc::strong_count(f) == 1);
                    let mut pixels = reusable
                        .and_then(|i| recent.remove(i))
                        .and_then(|f| Arc::try_unwrap(f).ok())
                        .map(|f| f.rgba)
                        .filter(|v| v.len() == size)
                        .unwrap_or_else(|| vec![0u8; size]);
                    let read = unsafe {
                        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                        // Waits for the GPU to finish this frame — here, not on the pacer.
                        gpu.context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).map(|()| {
                            let src = mapped.pData as *const u8;
                            let pitch = mapped.RowPitch as usize;
                            if pitch == w * 4 {
                                pixels.copy_from_slice(std::slice::from_raw_parts(src, size));
                            } else {
                                for row in 0..h {
                                    let line = std::slice::from_raw_parts(src.add(row * pitch), w * 4);
                                    pixels[row * w * 4..(row + 1) * w * 4].copy_from_slice(line);
                                }
                            }
                            gpu.context.Unmap(staging, 0);
                        })
                    };
                    busy.store(false, Ordering::Release);
                    if read.is_err() {
                        continue;
                    }
                    if !rgba {
                        for px in pixels.chunks_exact_mut(4) {
                            px.swap(0, 2);
                        }
                    }
                    recent.push_back(crate::preview::publish(generation, width, height, pixels, latest.waiting(), latest.app()));
                    while recent.len() > 3 {
                        recent.pop_front();
                    }
                }
                crate::preview::clear(generation);
            })
        };
        Ok(Self { converter, target, slots, gpu: gpu.clone(), tx: Some(tx), reader: Some(reader) })
    }

    /// Start a preview frame from the latest picture, if the preview wants one
    /// and a slot is free. Never waits.
    pub(crate) fn submit(&self) -> Result<()> {
        if !crate::preview::wants_frame() {
            return Ok(());
        }
        let Some(slot) = self.slots.iter().position(|(_, busy)| !busy.swap(true, Ordering::AcqRel)) else {
            return Ok(()); // both still on their way
        };
        let started = self.converter.convert(0).map(|()| unsafe { self.gpu.context.CopyResource(&self.slots[slot].0, &self.target) });
        if started.is_err() || self.tx.as_ref().is_none_or(|tx| tx.send(slot).is_err()) {
            self.slots[slot].1.store(false, Ordering::Release);
        }
        started
    }
}

impl Drop for Previewer {
    fn drop(&mut self) {
        drop(self.tx.take());
        if let Some(r) = self.reader.take() {
            let _ = r.join();
        }
    }
}

/// The largest rectangle with `w`×`h`'s shape that fits in `into_w`×`into_h`,
/// centered: (x, y, width, height). Even values, as NV12 needs.
fn fit(w: u32, h: u32, into_w: u32, into_h: u32) -> (u32, u32, u32, u32) {
    if w == 0 || h == 0 {
        return (0, 0, into_w, into_h);
    }
    let (fw, fh) = if w as u64 * into_h as u64 > h as u64 * into_w as u64 {
        (into_w, (h as u64 * into_w as u64 / w as u64) as u32)
    } else {
        ((w as u64 * into_h as u64 / h as u64) as u32, into_h)
    };
    let (fw, fh) = (fw & !1, fh & !1);
    (((into_w - fw) / 2) & !1, ((into_h - fh) / 2) & !1, fw, fh)
}

#[cfg(test)]
mod tests {
    use super::fit;

    #[test]
    fn fit_letterboxes_and_pillarboxes() {
        assert_eq!(fit(2560, 1440, 1920, 1080), (0, 0, 1920, 1080));
        // A 4:3 window in a 16:9 frame: bars left and right.
        assert_eq!(fit(1600, 1200, 1920, 1080), (240, 0, 1440, 1080));
        // An ultrawide window: bars top and bottom.
        assert_eq!(fit(3440, 1440, 1920, 1080), (0, 138, 1920, 802));
        assert_eq!(fit(0, 0, 1920, 1080), (0, 0, 1920, 1080));
    }
}

#[cfg(test)]
mod overlay_tests {
    use super::overlay_rects;
    use crate::webcam::Placement;

    #[test]
    fn webcam_box_in_frame() {
        let p = Placement { x: 0.75, y: 0.5, w: 0.25, h: 0.25, crop: [0.0; 4], flip_h: false, flip_v: false, turns: 0 };
        let (s, d) = overlay_rects(p, (1280, 720), 1920, 1080).unwrap();
        assert_eq!((s.left, s.top, s.right, s.bottom), (0, 0, 1280, 720));
        assert_eq!((d.left, d.top, d.right, d.bottom), (1440, 540, 1920, 810));
    }

    #[test]
    fn crop_and_clip_take_the_same_share() {
        // Half off the right edge, with the left quarter of the camera cropped.
        let p = Placement { x: 0.875, y: 0.0, w: 0.25, h: 0.25, crop: [0.25, 0.0, 0.0, 0.0], flip_h: false, flip_v: false, turns: 0 };
        let (s, d) = overlay_rects(p, (1280, 720), 1920, 1080).unwrap();
        assert_eq!((d.left, d.right), (1680, 1920));
        // Visible camera is 320..1280; half of it shows.
        assert_eq!((s.left, s.right), (320, 800));
        // Never stretched: the 960x720 left after the crop is 4:3, the box
        // 16:9, so rows are trimmed to 540, centred.
        assert_eq!((s.top, s.bottom), (90, 630));
        assert!(overlay_rects(Placement { x: 1.2, ..p }, (1280, 720), 1920, 1080).is_none());
        // Flipped, the frame's right edge cuts the camera's left side instead:
        // the visible half is the crop's left half, 320..800 → mirrored → 800..1280.
        let (s, _) = overlay_rects(Placement { flip_h: true, ..p }, (1280, 720), 1920, 1080).unwrap();
        assert_eq!((s.left, s.right), (800, 1280));
    }
}
