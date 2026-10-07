//! Getting a clip out of HesteClips and into wherever it's going.
//!
//! - **Drag**: drag a card out of the window onto Discord, a chat, a browser
//!   upload box, the desktop, an editor — a real OS file drag, so every app that
//!   accepts dropped files accepts a clip. (macOS and Windows: on Linux the
//!   window system has no drag we can start from an eframe window.)
//! - **Copy**: put the file on the clipboard, then paste it anywhere (⌘V).
//! - **Share sheet**: AirDrop, Messages, Mail, Notes, … on macOS; Nearby
//!   Share, Mail and share-capable apps on Windows.
//! - **MP3**: just the clip's sound, saved wherever you pick.
//!
//! All of these hand over the clip's file in the library, which is always the
//! clip as it looks now (an edit replaces it), so what you share is what you see.

use std::path::{Path, PathBuf};

#[cfg(any(target_os = "macos", target_os = "windows"))]
use raw_window_handle::HasWindowHandle;

/// The clip's sound as an MP3 at `out`: its mix (track 1, what the clip sounds
/// like), at the clip's own sample rate and 320 kbps, MP3's best — as close
/// to the clip's own (already compressed) sound as MP3 gets.
pub fn save_mp3(clip: &Path, out: &Path) -> Result<(), String> {
    let info = media::probe(clip).map_err(|e| e.to_string())?;
    if info.audio.is_empty() {
        return Err("this clip has no sound".into());
    }
    let title = clip.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let done = media::ffmpeg()
        .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(clip)
        .args(["-map", "0:a:0", "-map_metadata", "-1", "-metadata"])
        .arg(format!("title={title}"))
        .args(["-c:a", "libmp3lame", "-b:a", "320k"])
        .arg(out)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("ffmpeg: {e}"))?;
    if done.status.success() {
        return Ok(());
    }
    let _ = std::fs::remove_file(out);
    let why = String::from_utf8_lossy(&done.stderr);
    Err(why.lines().last().unwrap_or("ffmpeg failed").to_owned())
}

/// Whether clips can be dragged out of the window into other apps.
pub const CAN_DRAG_OUT: bool = cfg!(any(target_os = "macos", target_os = "windows"));

/// Start a native drag of `files` out of the window. The drag image is the
/// first clip's thumbnail (a JPEG on disk) when we have one, as a little stack
/// when there are several.
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub fn start_drag(frame: &eframe::Frame, files: Vec<PathBuf>, preview: Option<PathBuf>) -> Result<(), String> {
    let image = drag::Image::Raw(drag_image(preview.as_deref(), files.len()));
    drag::start_drag(
        frame,
        drag::DragItem::Files(files),
        image,
        |_, _| {},
        drag::Options::default(),
    )
    .map_err(|e| e.to_string())
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn start_drag(_frame: &eframe::Frame, _files: Vec<PathBuf>, _preview: Option<PathBuf>) -> Result<(), String> {
    Err("dragging clips out isn't available here — use Copy clip".into())
}

/// Put `file` on the clipboard as a file (not its path as text), ready to paste
/// into chats, Finder, mail…
pub fn copy_file(file: &Path) -> Result<(), String> {
    use clipboard_rs::Clipboard;
    // macOS wants a file URL; Windows takes a plain path; on Linux it becomes
    // a `text/uri-list` entry, which must be a URL with odd characters
    // (spaces in a renamed clip) escaped.
    #[cfg(target_os = "linux")]
    let entry = crate::clips::file_uri(&std::path::absolute(file).map_err(|e| e.to_string())?);
    #[cfg(not(target_os = "linux"))]
    let entry = if cfg!(target_os = "macos") {
        format!("file://{}", file.display())
    } else {
        file.display().to_string()
    };
    // On Linux (X11) the clipboard holds no data itself: whoever copied
    // serves it until something else is copied. One context does that for
    // every copy.
    #[cfg(target_os = "linux")]
    {
        static CTX: std::sync::Mutex<Option<clipboard_rs::ClipboardContext>> = std::sync::Mutex::new(None);
        let mut ctx = CTX.lock().unwrap();
        if ctx.is_none() {
            *ctx = Some(clipboard_rs::ClipboardContext::new().map_err(|e| e.to_string())?);
        }
        return ctx.as_ref().unwrap().set_files(vec![entry]).map_err(|e| e.to_string());
    }
    #[cfg(not(target_os = "linux"))]
    {
        let ctx = clipboard_rs::ClipboardContext::new().map_err(|e| e.to_string())?;
        ctx.set_files(vec![entry]).map_err(|e| e.to_string())
    }
}

/// Whether this platform has a system share sheet.
pub const HAS_SHARE_SHEET: bool = cfg!(any(target_os = "macos", target_os = "windows"));

/// The share sheet's menu entry, naming what it offers on this platform.
pub const SHARE_SHEET_LABEL: &str =
    if cfg!(target_os = "macos") { "AirDrop, Messages, Mail…" } else { "Share (Nearby Share, Mail…)" };

/// Open the macOS share sheet for `file`, anchored at `at` (window points from the
/// top-left, as egui reports them).
#[cfg(target_os = "macos")]
pub fn share_sheet(frame: &eframe::Frame, file: &Path, at: egui::Pos2) -> Result<(), String> {
    use objc2::AnyThread;
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2_app_kit::{NSSharingServicePicker, NSView};
    use objc2_foundation::{NSArray, NSPoint, NSRect, NSRectEdge, NSSize, NSString, NSURL};
    use raw_window_handle::RawWindowHandle;

    let handle = frame.window_handle().map_err(|e| e.to_string())?;
    let RawWindowHandle::AppKit(h) = handle.as_raw() else {
        return Err("not a macOS window".into());
    };
    // SAFETY: eframe's AppKit handle points at the live content NSView, and egui
    // calls us on the main thread.
    let view: &NSView = unsafe { h.ns_view.cast::<NSView>().as_ref() };
    let url = NSURL::fileURLWithPath(&NSString::from_str(&file.to_string_lossy()));
    let items: Retained<NSArray<AnyObject>> = NSArray::from_retained_slice(&[Retained::into_super(Retained::into_super(url))]);
    // SAFETY: NSURL is one of the item types the picker accepts.
    let picker = unsafe { NSSharingServicePicker::initWithItems(NSSharingServicePicker::alloc(), &items) };
    // egui's y grows downward; AppKit's grows upward unless the view is flipped.
    let y = if view.isFlipped() { at.y as f64 } else { view.bounds().size.height - at.y as f64 };
    let rect = NSRect::new(NSPoint::new(at.x as f64, y), NSSize::new(1.0, 1.0));
    picker.showRelativeToRect_ofView_preferredEdge(rect, view, NSRectEdge::NSMinYEdge);
    Ok(())
}

/// Open the Windows Share UI for `file`. It's anchored by Windows, not at `at`.
#[cfg(target_os = "windows")]
pub fn share_sheet(frame: &eframe::Frame, file: &Path, _at: egui::Pos2) -> Result<(), String> {
    use std::cell::Cell;

    use raw_window_handle::RawWindowHandle;
    use windows::ApplicationModel::DataTransfer::{DataRequestedEventArgs, DataTransferManager};
    use windows::Foundation::TypedEventHandler;
    use windows::Storage::{IStorageItem, StorageFile};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Shell::IDataTransferManagerInterop;
    use windows::core::{HSTRING, Interface};

    thread_local! {
        /// The window's current DataRequested handler, replaced per share.
        static HANDLER: Cell<Option<i64>> = const { Cell::new(None) };
    }

    let handle = frame.window_handle().map_err(|e| e.to_string())?;
    let RawWindowHandle::Win32(h) = handle.as_raw() else {
        return Err("not a Windows window".into());
    };
    let hwnd = HWND(h.hwnd.get() as *mut _);
    let err = |e: windows::core::Error| e.message();
    // Resolved up front: the request handler runs on this (UI) thread and
    // mustn't block on file I/O.
    let path = std::path::absolute(file).map_err(|e| e.to_string())?;
    let file: StorageFile =
        StorageFile::GetFileFromPathAsync(&HSTRING::from(path.as_os_str())).and_then(|op| op.join()).map_err(err)?;
    let file = windows::core::AgileReference::new(&file).map_err(err)?;
    let title = HSTRING::from(path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());

    let interop = windows::core::factory::<DataTransferManager, IDataTransferManagerInterop>().map_err(err)?;
    // SAFETY: `hwnd` is our live top-level window, and egui calls us on its thread.
    let manager: DataTransferManager = unsafe { interop.GetForWindow(hwnd) }.map_err(err)?;
    if let Some(token) = HANDLER.take() {
        let _ = manager.RemoveDataRequested(token);
    }
    let handler = TypedEventHandler::<DataTransferManager, DataRequestedEventArgs>::new(move |_, args| {
        let Some(args) = args.as_ref() else { return Ok(()) };
        let data = args.Request()?.Data()?;
        data.Properties()?.SetTitle(&title)?;
        let item: IStorageItem = file.resolve()?.cast()?;
        data.SetStorageItemsReadOnly(&windows_collections::IIterable::<IStorageItem>::from(vec![Some(item)]))
    });
    HANDLER.set(Some(manager.DataRequested(&handler).map_err(err)?));
    unsafe { interop.ShowShareUIForWindow(hwnd) }.map_err(err)
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn share_sheet(_frame: &eframe::Frame, _file: &Path, _at: egui::Pos2) -> Result<(), String> {
    Err("no share sheet on this platform".into())
}

/// The picture that follows the cursor: the clip's thumbnail at a small size
/// (the cached one is 480 px, far too big to drag around), or a plain tile if the
/// thumbnail isn't ready yet.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn drag_image(preview: Option<&Path>, count: usize) -> Vec<u8> {
    let mut img = preview
        .and_then(|p| image::open(p).ok())
        .map(|i| i.resize(192, 108, image::imageops::FilterType::Triangle))
        .unwrap_or_else(|| image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(160, 90, image::Rgba([40, 40, 44, 230]))));
    // Several clips: cards peeking out behind it, up to two.
    let behind = count.saturating_sub(1).min(2) as u32;
    if behind > 0 {
        const STEP: u32 = 7;
        let (w, h) = (img.width(), img.height());
        let mut pile = image::RgbaImage::new(w + STEP * behind, h + STEP * behind);
        for k in (1..=behind).rev() {
            let shade = 70 - 15 * k as u8;
            let card = image::RgbaImage::from_pixel(w, h, image::Rgba([shade, shade, shade + 4, 235]));
            image::imageops::overlay(&mut pile, &card, (STEP * k) as i64, (STEP * k) as i64);
        }
        image::imageops::overlay(&mut pile, &img.to_rgba8(), 0, 0);
        img = image::DynamicImage::ImageRgba8(pile);
    }
    let mut png = Vec::new();
    let _ = img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png);
    png
}

#[cfg(test)]
mod tests {
    /// Copy puts a real file reference on the clipboard (what Finder/Discord paste).
    #[test]
    #[ignore = "touches the real system clipboard; run with --ignored"]
    fn copy_puts_file_on_clipboard() {
        let f = std::env::temp_dir().join("hc-copy-test.mp4");
        std::fs::write(&f, b"x").unwrap();
        super::copy_file(&f).unwrap();
        let out = std::process::Command::new("osascript")
            .args(["-e", "POSIX path of (the clipboard as «class furl»)"])
            .output()
            .unwrap();
        let got = std::path::PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
        let ok = std::fs::canonicalize(&got).ok() == std::fs::canonicalize(&f).ok();
        std::fs::remove_file(&f).unwrap();
        assert!(ok, "clipboard held {got:?}, expected {f:?}");
    }

    #[test]
    fn mp3_keeps_the_clips_sample_rate_at_320k() {
        let dir = std::env::temp_dir().join(format!("hc-mp3-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("Clip.mp4");
        let made = media::ffmpeg()
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "color=black:s=64x64:d=2", "-f", "lavfi", "-i", "sine=f=440:r=48000:d=2", "-ac", "2", "-shortest"])
            .arg(&clip)
            .status()
            .unwrap();
        assert!(made.success());
        let out = dir.join("Clip.mp3");
        super::save_mp3(&clip, &out).unwrap();
        let probe = media::ffprobe()
            .args(["-v", "error", "-show_entries", "stream=codec_name,sample_rate,bit_rate,channels", "-of", "csv=p=0"])
            .arg(&out)
            .output()
            .unwrap();
        let line = String::from_utf8_lossy(&probe.stdout).trim().to_owned();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(line, "mp3,48000,2,320000", "{line}");
    }
}
