//! A window that draws like a game, to test the game capture hook on: red top
//! half, blue bottom half (so a picture upside down shows), a white bar
//! moving across (so frames differ), with Direct3D 11 or (`GL=1`) OpenGL,
//! the way osu! and Geometry Dash draw. Closes after `SECS` seconds (10).
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

    extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
    }

    let secs: f64 = std::env::var("SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(10.0);
    let gl = std::env::var("GL").is_ok_and(|v| v == "1");
    // STATIC=1: the same picture every frame (still presented each frame).
    let still = std::env::var("STATIC").is_ok_and(|v| v == "1");
    let (w, h) = (800, 600);
    let hwnd = unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = WNDCLASSW { lpfnWndProc: Some(proc), hInstance: instance.into(), lpszClassName: w!("hook_target"), ..Default::default() };
        RegisterClassW(&class);
        let mut r = RECT { left: 0, top: 0, right: w, bottom: h };
        AdjustWindowRect(&mut r, WS_OVERLAPPEDWINDOW, false)?;
        CreateWindowExW(
            Default::default(),
            w!("hook_target"),
            if gl { w!("hook target (OpenGL)") } else { w!("hook target (Direct3D 11)") },
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            100,
            100,
            r.right - r.left,
            r.bottom - r.top,
            None,
            None,
            Some(instance.into()),
            None,
        )?
    };

    let start = std::time::Instant::now();
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
    while start.elapsed().as_secs_f64() < secs {
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            if !IsWindow(Some(hwnd)).as_bool() {
                break;
            }
        }
        draw(if still { 0.0 } else { start.elapsed().as_secs_f32() });
    }
    Ok(())
}

#[cfg(not(windows))]
fn main() {}
