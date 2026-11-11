//! A window that draws like a game, to test the game capture hook on: red top
//! half, blue bottom half (so a picture upside down shows), a white bar
//! moving across (so frames differ), with Direct3D 11 or (`GL=1`) OpenGL,
//! the way osu! and Geometry Dash draw. Closes after `SECS` seconds (10).
//! `FPS=n`: exactly n pictures a second, the bar 4 px further in each (to
//! check frame blending: each recorded frame should show the bar's
//! positions in its time, evenly). `NOVSYNC=1` (OpenGL): faster than the
//! display's refresh rate. `SIZE=WxH`: draw that big (800×600).
//! `FULLSCREEN=1`: a borderless window exactly covering the main display, as
//! a game in fullscreen is (OpenGL's then goes straight to the display, past
//! Windows' compositor, with some drivers: issue #11). `HIDE_CURSOR=1`: no
//! cursor over it, and kept in its middle, as a game being played does;
//! `HIDE_CURSOR=null`: the same, hidden the way GLFW (Minecraft 1.13 on) does,
//! with no cursor image rather than none shown. `TIMES=1`: how far apart its
//! pictures came, at the end.
//!
//!   cargo run -p capture --example hook_target
//!   APP=hook_target.exe HOOK=1 SECS=5 HEIGHT=0 cargo run -p capture --example record
#[cfg(windows)]
fn main() -> windows::core::Result<()> {
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows::Win32::Graphics::Gdi::GetDC;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::*;
    use windows::core::w;

    static NO_CURSOR_IMAGE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        if msg == WM_SETCURSOR && (lp.0 & 0xFFFF) as u32 == HTCLIENT && NO_CURSOR_IMAGE.load(std::sync::atomic::Ordering::Relaxed) {
            unsafe { SetCursor(None) };
            return LRESULT(1);
        }
        unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
    }

    let secs: f64 = std::env::var("SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(10.0);
    let gl = std::env::var("GL").is_ok_and(|v| v == "1");
    // STATIC=1: the same picture every frame (still presented each frame).
    let still = std::env::var("STATIC").is_ok_and(|v| v == "1");
    let fullscreen = std::env::var("FULLSCREEN").is_ok_and(|v| v == "1");
    let hide_cursor = std::env::var("HIDE_CURSOR").ok().filter(|v| v == "1" || v == "null");
    NO_CURSOR_IMAGE.store(hide_cursor.as_deref() == Some("null"), std::sync::atomic::Ordering::Relaxed);
    // Fullscreen, the display's real pixels, however it's scaled.
    if fullscreen {
        let _ = unsafe { windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    }
    let display = unsafe {
        use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromPoint};
        let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(MonitorFromPoint(Default::default(), MONITOR_DEFAULTTOPRIMARY), &mut info);
        info.rcMonitor
    };
    let (w, h) = if fullscreen {
        (display.right - display.left, display.bottom - display.top)
    } else {
        // SIZE=WxH: the window's drawing size (800×600).
        std::env::var("SIZE").ok().and_then(|s| s.split_once('x').and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?)))).unwrap_or((800, 600))
    };
    let hwnd = unsafe {
        let instance = GetModuleHandleW(None)?;
        // The arrow over it, as a game shows (none would leave Windows'
        // animated busy one, a new frame for Windows' capture each turn).
        let class = WNDCLASSW { lpfnWndProc: Some(proc), hInstance: instance.into(), lpszClassName: w!("hook_target"), hCursor: LoadCursorW(None, IDC_ARROW)?, ..Default::default() };
        RegisterClassW(&class);
        let (style, x, y, r) = if fullscreen {
            (WS_POPUP | WS_VISIBLE, display.left, display.top, RECT { left: 0, top: 0, right: w, bottom: h })
        } else {
            let mut r = RECT { left: 0, top: 0, right: w, bottom: h };
            AdjustWindowRect(&mut r, WS_OVERLAPPEDWINDOW, false)?;
            (WS_OVERLAPPEDWINDOW | WS_VISIBLE, 100, 100, r)
        };
        CreateWindowExW(
            Default::default(),
            w!("hook_target"),
            if gl { w!("hook target (OpenGL)") } else { w!("hook target (Direct3D 11)") },
            style,
            x,
            y,
            r.right - r.left,
            r.bottom - r.top,
            None,
            None,
            Some(instance.into()),
            None,
        )?
    };
    if hide_cursor.as_deref() == Some("1") {
        unsafe { ShowCursor(false) };
    }
    let hide_cursor = hide_cursor.is_some();

    let start = std::time::Instant::now();
    let cap: Option<f64> = std::env::var("FPS").ok().and_then(|s| s.parse().ok());
    let mut shown = 0u64;
    let mut late = 0u64;
    let mut draw: Box<dyn FnMut(f32)> = if gl {
        use windows::Win32::Graphics::Gdi::HDC;
        use windows::Win32::Graphics::OpenGL::*;
        let dc: HDC = unsafe { GetDC(Some(hwnd)) };
        unsafe {
            let pfd = PIXELFORMATDESCRIPTOR {
                nSize: std::mem::size_of::<PIXELFORMATDESCRIPTOR>() as u16,
                nVersion: 1,
                dwFlags: PFD_DRAW_TO_WINDOW | PFD_SUPPORT_OPENGL | PFD_DOUBLEBUFFER,
                iPixelType: PFD_TYPE_RGBA,
                cColorBits: 32,
                cDepthBits: 24,
                ..Default::default()
            };
            let format = ChoosePixelFormat(dc, &pfd);
            SetPixelFormat(dc, format, &pfd)?;
            let ctx = wglCreateContext(dc)?;
            wglMakeCurrent(dc, ctx)?;
            // NOVSYNC=1: not held to the display's refresh, like a game with
            // vsync off (else `FPS` above the refresh rate isn't reached).
            if std::env::var("NOVSYNC").is_ok_and(|v| v == "1") {
                if let Some(f) = wglGetProcAddress(windows::core::s!("wglSwapIntervalEXT")) {
                    let swap_interval: extern "system" fn(i32) -> i32 = std::mem::transmute(f);
                    swap_interval(0);
                }
            }
        }
        Box::new(move |t| unsafe {
            glViewport(0, 0, w, h);
            glClearColor(0.0, 0.0, 1.0, 1.0);
            glClear(GL_COLOR_BUFFER_BIT);
            glEnable(GL_SCISSOR_TEST);
            // OpenGL counts rows from the bottom: this is the top half.
            glScissor(0, h / 2, w, h / 2);
            glClearColor(1.0, 0.0, 0.0, 1.0);
            glClear(GL_COLOR_BUFFER_BIT);
            glScissor(((t * 200.0) as i32) % w, 0, 40, h);
            glClearColor(1.0, 1.0, 1.0, 1.0);
            glClear(GL_COLOR_BUFFER_BIT);
            glDisable(GL_SCISSOR_TEST);
            let _ = SwapBuffers(dc);
        })
    } else {
        use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
        use windows::Win32::Graphics::Direct3D11::*;
        use windows::Win32::Graphics::Dxgi::Common::*;
        use windows::Win32::Graphics::Dxgi::*;
        use windows::core::Interface;
        let desc = DXGI_SWAP_CHAIN_DESC {
            BufferDesc: DXGI_MODE_DESC { Width: w as u32, Height: h as u32, Format: DXGI_FORMAT_B8G8R8A8_UNORM, ..Default::default() },
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            OutputWindow: hwnd,
            Windowed: true.into(),
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            Flags: 0,
        };
        let (mut swap, mut device, mut context) = (None, None, None);
        unsafe {
            D3D11CreateDeviceAndSwapChain(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                Default::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&desc),
                Some(&mut swap),
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
        }
        let (swap, device, context): (IDXGISwapChain, ID3D11Device, ID3D11DeviceContext) = (swap.unwrap(), device.unwrap(), context.unwrap());
        let context1: ID3D11DeviceContext1 = context.cast()?;
        let back: ID3D11Texture2D = unsafe { swap.GetBuffer(0)? };
        let mut rtv = None;
        unsafe { device.CreateRenderTargetView(&back, None, Some(&mut rtv))? };
        let rtv = rtv.unwrap();
        let view: ID3D11View = rtv.cast()?;
        Box::new(move |t| unsafe {
            context.ClearRenderTargetView(&rtv, &[0.0, 0.0, 1.0, 1.0]);
            context1.ClearView(&view, &[1.0, 0.0, 0.0, 1.0], Some(&[RECT { left: 0, top: 0, right: w, bottom: h / 2 }]));
            let x = ((t * 200.0) as i32) % w;
            context1.ClearView(&view, &[1.0, 1.0, 1.0, 1.0], Some(&[RECT { left: x, top: 0, right: x + 40, bottom: h }]));
            let _ = swap.Present(1, Default::default());
        })
    };

    let mut msg = MSG::default();
    // TIMES=1: how far apart the pictures came (each drawn as soon as the
    // last was taken): a hitch shows as a long gap.
    let mut gaps = std::env::var("TIMES").is_ok_and(|v| v == "1").then(Vec::<f64>::new);
    let mut last_drawn = 0.0;
    while start.elapsed().as_secs_f64() < secs {
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            if !IsWindow(Some(hwnd)).as_bool() {
                break;
            }
            // Back to the middle, as a game turning the mouse into camera
            // movement keeps it.
            if hide_cursor && GetForegroundWindow() == hwnd {
                let _ = SetCursorPos(display.left + w / 2, display.top + h / 2);
            }
        }
        let t = match cap {
            // The bar moves 200 px a second of `t`: 4 px a picture.
            Some(fps) => {
                let due = shown as f64 / fps;
                while start.elapsed().as_secs_f64() < due {
                    std::hint::spin_loop();
                }
                shown += 1;
                shown as f32 / 50.0
            }
            None => start.elapsed().as_secs_f32(),
        };
        draw(if still { 0.0 } else { t });
        if let Some(gaps) = gaps.as_mut() {
            let now = start.elapsed().as_secs_f64();
            gaps.push(now - last_drawn);
            last_drawn = now;
        }
        // How many pictures were shown late: more than half a picture after
        // their time (then the recording rightly shows them in a later frame).
        if let Some(fps) = cap {
            let due = (shown - 1) as f64 / fps;
            if start.elapsed().as_secs_f64() - due > 0.5 / fps {
                late += 1;
            }
        }
    }
    if let Some(fps) = cap {
        println!("{shown} pictures at {fps} fps, {late} shown more than half a picture late");
    }
    if let Some(mut gaps) = gaps.filter(|g| g.len() > 1) {
        gaps.remove(0);
        gaps.sort_by(f64::total_cmp);
        let at = |q: f64| gaps[((gaps.len() - 1) as f64 * q) as usize] * 1000.0;
        let long = gaps.iter().filter(|&&g| g * 1000.0 > 1.5 * at(0.5)).count();
        println!("{} pictures, ms apart: median {:.2}, 99% {:.2}, 99.9% {:.2}, longest {:.2}; {long} over 1.5× the median", gaps.len() + 1, at(0.5), at(0.99), at(0.999), at(1.0));
    }
    Ok(())
}

#[cfg(not(windows))]
fn main() {}
