//! Getting a clip out of hesteclips and into wherever it's going.
//!
//! - **Drag**: drag a card out of the window onto Discord, a chat, a browser
//!   upload box, the desktop, an editor — a real OS file drag, so every app that
//!   accepts dropped files accepts a clip.
//! - **Copy**: put the file on the clipboard, then paste it anywhere (⌘V).
//! - **Share sheet** (macOS): AirDrop, Messages, Mail, Notes, …
//!
//! All of these hand over the clip's *playable* file: the rendered edit when
//! there is one, so what you share is what you see.

use std::path::{Path, PathBuf};

use raw_window_handle::HasWindowHandle;

/// Start a native drag of `file` out of the window. The drag image is the clip's
/// thumbnail (a JPEG on disk) when we have one.
pub fn start_drag(frame: &eframe::Frame, file: &Path, preview: Option<PathBuf>) -> Result<(), String> {
    let image = drag::Image::Raw(drag_image(preview.as_deref()));
    drag::start_drag(
        frame,
        drag::DragItem::Files(vec![file.to_path_buf()]),
        image,
        |_, _| {},
        drag::Options::default(),
    )
    .map_err(|e| e.to_string())
}

/// Put `file` on the clipboard as a file (not its path as text), ready to paste
/// into chats, Finder, mail…
pub fn copy_file(file: &Path) -> Result<(), String> {
    use clipboard_rs::Clipboard;
    let ctx = clipboard_rs::ClipboardContext::new().map_err(|e| e.to_string())?;
    // macOS wants a file URL; Windows/Linux take a plain path.
    let entry = if cfg!(target_os = "macos") {
        format!("file://{}", file.display())
    } else {
        file.display().to_string()
    };
    ctx.set_files(vec![entry]).map_err(|e| e.to_string())
}

/// Whether this platform has a system share sheet.
pub const HAS_SHARE_SHEET: bool = cfg!(target_os = "macos");

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

#[cfg(not(target_os = "macos"))]
pub fn share_sheet(_frame: &eframe::Frame, _file: &Path, _at: egui::Pos2) -> Result<(), String> {
    Err("no share sheet on this platform".into())
}

/// The picture that follows the cursor: the clip's thumbnail at a small size
/// (the cached one is 480 px, far too big to drag around), or a plain tile if the
/// thumbnail isn't ready yet.
fn drag_image(preview: Option<&Path>) -> Vec<u8> {
    let img = preview
        .and_then(|p| image::open(p).ok())
        .map(|i| i.resize(192, 108, image::imageops::FilterType::Triangle))
        .unwrap_or_else(|| image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(160, 90, image::Rgba([40, 40, 44, 230]))));
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
}
