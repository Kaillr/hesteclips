//! macOS pieces beyond the recorder's core (`crate::sck`).

pub(crate) mod camera;
pub mod decode;
pub(crate) mod gpu;
pub(crate) mod preview;
pub(crate) mod video;
pub(crate) mod windows;

/// For `examples/windowed_apps`: what one window-list query costs.
pub fn bench_windows() {
    let t = std::time::Instant::now();
    let mut n = 0;
    for _ in 0..100 {
        n = windows::windows(false).len();
    }
    println!("{n} windows, {:?} per query", t.elapsed() / 100);
}

/// For `examples/vt_limits`: whether the hardware H.264 encoder opens at this size.
pub fn probe_encoder(width: usize, height: usize) -> String {
    crate::sck::probe_encoder(width, height)
}

/// A display's size in pixels, by id (`None`: the main display), else the
/// main display's.
pub(crate) fn display_pixels(id: Option<&str>) -> Option<(u32, u32)> {
    use objc2_core_graphics::{CGDisplayCopyDisplayMode, CGDisplayMode, CGGetActiveDisplayList, CGMainDisplayID};
    let mut ids = [0u32; 16];
    let mut n = 0u32;
    if unsafe { CGGetActiveDisplayList(16, ids.as_mut_ptr(), &mut n) } != objc2_core_graphics::CGError::Success {
        return None;
    }
    let ids = &ids[..n as usize];
    let display = id.and_then(|s| s.parse::<u32>().ok()).filter(|d| ids.contains(d)).unwrap_or_else(|| CGMainDisplayID());
    let mode = CGDisplayCopyDisplayMode(display);
    let (w, h) = (CGDisplayMode::pixel_width(mode.as_deref()), CGDisplayMode::pixel_height(mode.as_deref()));
    (w > 0 && h > 0).then_some((w as u32, h as u32))
}
