//! Browser sources: a web page (alerts, a chat overlay, a widget) drawn by
//! WebView2, the Edge that's part of Windows, into a window of our own off
//! every display, which Windows' capture records as any window
//! (`layers::CaptureFeed`). Its background is transparent where the page's
//! is, so it's blended over what's under it.
//!
//! The window is a tool window (not on the taskbar, nor in Alt+Tab) that
//! never takes the focus. Chromium stops drawing a window it thinks no one
//! sees: that's turned off (`ARGS`), or the page would freeze.

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Arc, Once};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use webview2_com::Microsoft::Web::WebView2::Win32::{
    COREWEBVIEW2_COLOR, ICoreWebView2Controller, ICoreWebView2Controller2, ICoreWebView2Environment, ICoreWebView2EnvironmentOptions,
};
use webview2_com::{AddScriptToExecuteOnDocumentCreatedCompletedHandler, CoreWebView2EnvironmentOptions, CreateCoreWebView2ControllerCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HRESULT, HSTRING, Interface, PCWSTR, w};

// WebView2's loader, linked in (`WebView2LoaderStatic.lib`, from
// webview2-com-sys, whose build script puts it on the linker's path): the
// app needs no DLL beside it, and starts where WebView2 isn't there.
#[link(name = "WebView2LoaderStatic", kind = "static")]
unsafe extern "system" {
    fn CreateCoreWebView2EnvironmentWithOptions(browser_folder: PCWSTR, user_data_folder: PCWSTR, options: *mut std::ffi::c_void, handler: *mut std::ffi::c_void) -> HRESULT;
}

/// Chromium keeps drawing, and running the page's timers, while its
/// window's off every display; videos and sounds play without a click.
const ARGS: &str = "--disable-features=CalculateNativeWinOcclusion --disable-background-timer-throttling --disable-renderer-backgrounding --disable-backgrounding-occluded-windows --autoplay-policy=no-user-gesture-required";

/// A web page in a window of its own, on a thread of its own (WebView2 runs
/// on its window's thread, which pumps its messages). Closed when dropped.
pub(crate) struct BrowserHost {
    /// Its window, once it's made (0 until then, and if it couldn't be).
    window: Arc<AtomicIsize>,
    stop: Arc<AtomicBool>,
}

impl BrowserHost {
    /// Show `url` in a page `width`×`height` big, `css` added to it.
    pub(crate) fn start(url: String, width: u32, height: u32, css: String) -> Self {
        let window = Arc::new(AtomicIsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (window2, stop2) = (window.clone(), stop.clone());
        thread::spawn(move || {
            if let Err(e) = run(&url, width.max(16), height.max(16), &css, &window2, &stop2) {
                eprintln!("browser source {url}: {e:#}");
            }
            window2.store(0, Ordering::Release);
        });
        Self { window, stop }
    }

    /// Its window, once the page is up.
    pub(crate) fn window(&self) -> Option<HWND> {
        let h = self.window.load(Ordering::Acquire);
        (h != 0).then_some(HWND(h as *mut _))
    }
}

impl Drop for BrowserHost {
    /// Its thread closes the page and its window.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // Never activated by a click that finds it.
    if msg == WM_MOUSEACTIVATE {
        return LRESULT(MA_NOACTIVATE as isize);
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// The page's window, made, shown off every display, its page loaded (with
/// `css`); then its messages pumped until it's to stop.
fn run(url: &str, width: u32, height: u32, css: &str, out: &AtomicIsize, stop: &AtomicBool) -> Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let instance = GetModuleHandleW(None)?;
        static CLASS: Once = Once::new();
        CLASS.call_once(|| {
            let class = WNDCLASSW { lpfnWndProc: Some(window_proc), hInstance: instance.into(), lpszClassName: w!("HesteClipsBrowserSource"), ..Default::default() };
            RegisterClassW(&class);
        });
        // Left of every display: drawn (and captured), never seen.
        let x = GetSystemMetrics(SM_XVIRTUALSCREEN) - width as i32 - 100;
        let y = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("HesteClipsBrowserSource"),
            w!("HesteClips browser source"),
            WS_POPUP,
            x,
            y,
            width as i32,
            height as i32,
            None,
            None,
            Some(instance.into()),
            None,
        )?;
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        let result = (|| -> Result<ICoreWebView2Controller> {
            let environment = environment()?;
            let (tx, rx) = std::sync::mpsc::channel();
            let env = environment.clone();
            CreateCoreWebView2ControllerCompletedHandler::wait_for_async_operation(
                Box::new(move |handler| env.CreateCoreWebView2Controller(hwnd, &handler).map_err(webview2_com::Error::WindowsError)),
                Box::new(move |error, controller| {
                    error?;
                    tx.send(controller.ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_POINTER))).expect("send over mpsc channel");
                    Ok(())
                }),
            )
            .map_err(|e| anyhow!("{e}"))?;
            let controller = rx.recv().context("WebView2 didn't make its view")??;
            controller.SetBounds(RECT { left: 0, top: 0, right: width as i32, bottom: height as i32 })?;
            controller.SetIsVisible(true)?;
            // Transparent where the page is: blended over what's under it.
            if let Ok(c2) = controller.cast::<ICoreWebView2Controller2>() {
                c2.SetDefaultBackgroundColor(COREWEBVIEW2_COLOR { A: 0, R: 0, G: 0, B: 0 })?;
            }
            let view = controller.CoreWebView2()?;
            if let Some(script) = crate::layers::style_script(css) {
                let v = view.clone();
                AddScriptToExecuteOnDocumentCreatedCompletedHandler::wait_for_async_operation(
                    Box::new(move |handler| v.AddScriptToExecuteOnDocumentCreated(&HSTRING::from(script), &handler).map_err(webview2_com::Error::WindowsError)),
                    Box::new(|error, _id| error),
                )
                .map_err(|e| anyhow!("{e}"))?;
            }
            view.Navigate(&HSTRING::from(url))?;
            Ok(controller)
        })();
        let controller = match result {
            Ok(c) => c,
            Err(e) => {
                let _ = DestroyWindow(hwnd);
                return Err(e);
            }
        };
        out.store(hwnd.0 as isize, Ordering::Release);
        let mut msg = MSG::default();
        while !stop.load(Ordering::Relaxed) {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            thread::sleep(Duration::from_millis(8));
        }
        out.store(0, Ordering::Release);
        let _ = controller.Close();
        let _ = DestroyWindow(hwnd);
        Ok(())
    }
}

/// WebView2's environment, its data (cookies, a page's storage) kept with
/// the app's (`%LOCALAPPDATA%\HesteClips\Browser`), not beside it.
fn environment() -> Result<ICoreWebView2Environment> {
    let data = std::env::var_os("LOCALAPPDATA").map(|d| std::path::PathBuf::from(d).join("HesteClips").join("Browser")).context("no local app data folder")?;
    let _ = std::fs::create_dir_all(&data);
    let data = HSTRING::from(data.as_os_str());
    let options = CoreWebView2EnvironmentOptions::default();
    unsafe { options.set_additional_browser_arguments(ARGS.to_owned()) };
    let options: ICoreWebView2EnvironmentOptions = options.into();
    let (tx, rx) = std::sync::mpsc::channel();
    CreateCoreWebView2EnvironmentCompletedHandler::wait_for_async_operation(
        Box::new(move |handler| unsafe {
            CreateCoreWebView2EnvironmentWithOptions(PCWSTR::null(), PCWSTR(data.as_ptr()), options.as_raw(), handler.as_raw()).ok().map_err(webview2_com::Error::WindowsError)
        }),
        Box::new(move |error, environment| {
            error?;
            tx.send(environment.ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_POINTER))).expect("send over mpsc channel");
            Ok(())
        }),
    )
    .map_err(|e| anyhow!("WebView2 (Edge's web view, part of Windows) couldn't start: {e}"))?;
    Ok(rx.recv().context("WebView2 didn't start")??)
}
