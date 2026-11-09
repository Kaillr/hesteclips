//! Browser sources on macOS: a web page (alerts, a chat overlay, a widget)
//! drawn by WebKit (Safari's engine, part of macOS) in a borderless window
//! of our own off every display, which ScreenCaptureKit records as any
//! window (`super::layers`). Its background is transparent where the page's
//! is, so it's blended over what's under it.
//!
//! WebKit stops drawing a page whose window it takes to be out of sight
//! (off every display, or covered): its animations and timers froze,
//! measured, 3 pictures in 3 s instead of 30. So the view's occlusion
//! detection is off (`_setWindowOcclusionDetectionEnabled:`, WebKit's own
//! setting, though not public API: where a macOS doesn't have it, the page
//! only draws while its window would show), as Windows turns off
//! Chromium's for the same reason. Videos and sounds in a page play without
//! a click.
//!
//! AppKit and WebKit run on the main thread: the window and its page are
//! made there (the app's event loop runs it), and closed there when the
//! page is dropped, neither waited for.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use dispatch2::{DispatchQueue, MainThreadBound};
use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly, msg_send, sel};
use objc2_app_kit::{NSBackingStoreType, NSColor, NSScreen, NSWindow, NSWindowCollectionBehavior, NSWindowStyleMask};
use objc2_foundation::{NSNumber, NSPoint, NSRect, NSSize, NSString, NSURL, NSURLRequest};
use objc2_web_kit::{WKAudiovisualMediaTypes, WKUserScript, WKUserScriptInjectionTime, WKWebView, WKWebViewConfiguration};

/// A page's window and its view.
type Views = (Retained<NSWindow>, Retained<WKWebView>);

/// A web page in a window of its own. Closed when dropped.
pub(crate) struct Page {
    pub url: String,
    /// Its size, in points: its picture is captured as many pixels.
    pub size: (u32, u32),
    /// Its window's number, once it's up (0 until then).
    window: AtomicU32,
    /// The window and its view, on the main thread.
    views: Mutex<Option<MainThreadBound<Views>>>,
}

impl Page {
    /// Show `url` in a page `width`×`height` big, `css` added to it: made on
    /// the main thread, not waited for.
    pub(crate) fn open(url: String, width: u32, height: u32, css: String) -> Result<Arc<Page>> {
        NSURL::URLWithString(&NSString::from_str(&url)).with_context(|| format!("{url} isn't a web address"))?;
        let page = Arc::new(Page { url, size: (width.max(16), height.max(16)), window: AtomicU32::new(0), views: Mutex::new(None) });
        let made = page.clone();
        DispatchQueue::main().exec_async(move || {
            let Some(mtm) = MainThreadMarker::new() else { return };
            match made.make(mtm, &css) {
                Ok(views) => {
                    made.window.store(views.0.windowNumber() as u32, Ordering::Release);
                    *made.views.lock().unwrap() = Some(MainThreadBound::new(views, mtm));
                }
                Err(e) => eprintln!("web page {}: {e:#}", made.url),
            }
        });
        Ok(page)
    }

    /// Its window's number, once the page is up.
    pub(crate) fn window(&self) -> Option<u32> {
        let n = self.window.load(Ordering::Acquire);
        (n != 0).then_some(n)
    }

    /// The window, off every display, and its page loading.
    fn make(&self, mtm: MainThreadMarker, css: &str) -> Result<Views> {
        let (w, h) = (self.size.0 as f64, self.size.1 as f64);
        // Left of every display: drawn (and captured), never seen.
        let screens = NSScreen::screens(mtm);
        let left = screens.iter().map(|s| s.frame().origin.x).fold(0.0, f64::min);
        let bottom = screens.iter().map(|s| s.frame().origin.y).fold(0.0, f64::min);
        let frame = NSRect::new(NSPoint::new(left - w - 100.0, bottom), NSSize::new(w, h));
        let window = unsafe { NSWindow::initWithContentRect_styleMask_backing_defer(NSWindow::alloc(mtm), frame, NSWindowStyleMask::Borderless, NSBackingStoreType::Buffered, false) };
        unsafe { window.setReleasedWhenClosed(false) };
        window.setOpaque(false);
        window.setBackgroundColor(Some(&NSColor::clearColor()));
        window.setHasShadow(false);
        window.setIgnoresMouseEvents(true);
        window.setExcludedFromWindowsMenu(true);
        // Not among the app's windows in Mission Control, nor cycled to.
        window.setCollectionBehavior(NSWindowCollectionBehavior::Transient | NSWindowCollectionBehavior::IgnoresCycle | NSWindowCollectionBehavior::Stationary);
        let config = unsafe { WKWebViewConfiguration::new(mtm) };
        unsafe { config.setMediaTypesRequiringUserActionForPlayback(WKAudiovisualMediaTypes::None) };
        if let Some(script) = crate::layers::style_script(css) {
            let script = unsafe { WKUserScript::initWithSource_injectionTime_forMainFrameOnly(WKUserScript::alloc(mtm), &NSString::from_str(&script), WKUserScriptInjectionTime::AtDocumentStart, true) };
            unsafe { config.userContentController().addUserScript(&script) };
        }
        let view = unsafe { WKWebView::initWithFrame_configuration(WKWebView::alloc(mtm), NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w, h)), &config) };
        // Transparent where the page is: blended over what's under it.
        let _: () = unsafe { msg_send![&view, setValue: &*NSNumber::new_bool(false), forKey: &*NSString::from_str("drawsBackground")] };
        // Drawn while out of sight (see the top).
        let can: bool = unsafe { msg_send![&view, respondsToSelector: sel!(_setWindowOcclusionDetectionEnabled:)] };
        if can {
            let _: () = unsafe { msg_send![&view, _setWindowOcclusionDetectionEnabled: false] };
        } else {
            eprintln!("web page {}: it may stop moving while its window's out of sight (this macOS can't keep it drawing)", self.url);
        }
        window.setContentView(Some(&view));
        window.orderFrontRegardless();
        let address = NSURL::URLWithString(&NSString::from_str(&self.url)).context("not a web address")?;
        unsafe { view.loadRequest(&NSURLRequest::requestWithURL(&address)) };
        Ok((window, view))
    }
}

impl Drop for Page {
    /// Its window closed, on the main thread (not waited for).
    fn drop(&mut self) {
        let Some(views) = self.views.lock().unwrap().take() else { return };
        DispatchQueue::main().exec_async(move || {
            if let Some(mtm) = MainThreadMarker::new() {
                let (window, view) = views.get(mtm);
                unsafe { view.stopLoading() };
                window.orderOut(None);
                window.close();
            }
            drop(views);
        });
    }
}
