//! macOS pieces beyond the recorder's core (`crate::sck`).

pub mod bench;
pub(crate) mod blend;
pub(crate) mod browser;
pub(crate) mod camera;
pub mod decode;
pub(crate) mod export;
pub(crate) mod gpu;
pub(crate) mod layers;
pub(crate) mod preview;
pub(crate) mod shot;
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

/// A display's current mode, by id (`None`: the main display), else the
/// main display's.
fn display_mode(id: Option<&str>) -> Option<objc2_core_foundation::CFRetained<objc2_core_graphics::CGDisplayMode>> {
    use objc2_core_graphics::{CGDisplayCopyDisplayMode, CGGetActiveDisplayList, CGMainDisplayID};
    let mut ids = [0u32; 16];
    let mut n = 0u32;
    if unsafe { CGGetActiveDisplayList(16, ids.as_mut_ptr(), &mut n) } != objc2_core_graphics::CGError::Success {
        return None;
    }
    let ids = &ids[..n as usize];
    let display = id.and_then(|s| s.parse::<u32>().ok()).filter(|d| ids.contains(d)).unwrap_or_else(|| CGMainDisplayID());
    CGDisplayCopyDisplayMode(display)
}

/// Where display `id` is on the desktop, in points (left, top, width,
/// height; the main display's top-left corner is 0, 0, as AppKit places
/// windows by it). `None` if it isn't connected.
pub(crate) fn display_bounds(id: &str) -> Option<[i32; 4]> {
    use objc2_core_graphics::{CGDisplayBounds, CGGetActiveDisplayList};
    let want: u32 = id.parse().ok()?;
    let mut ids = [0u32; 16];
    let mut n = 0u32;
    if unsafe { CGGetActiveDisplayList(16, ids.as_mut_ptr(), &mut n) } != objc2_core_graphics::CGError::Success || !ids[..n as usize].contains(&want) {
        return None;
    }
    let r = CGDisplayBounds(want);
    let (w, h) = (r.size.width.round() as i32, r.size.height.round() as i32);
    (w > 0 && h > 0).then(|| [r.origin.x.round() as i32, r.origin.y.round() as i32, w, h])
}

/// A display's size in pixels, by id (`None`: the main display), else the
/// main display's.
pub(crate) fn display_pixels(id: Option<&str>) -> Option<(u32, u32)> {
    use objc2_core_graphics::CGDisplayMode;
    let mode = display_mode(id);
    let (w, h) = (CGDisplayMode::pixel_width(mode.as_deref()), CGDisplayMode::pixel_height(mode.as_deref()));
    (w > 0 && h > 0).then_some((w as u32, h as u32))
}

/// How many times a second a display shows a new picture, by id (`None`:
/// the main display). Nothing on it changes faster, so ScreenCaptureKit
/// never delivers more different pictures than this, whatever a game draws.
pub(crate) fn display_refresh(id: Option<&str>) -> Option<u32> {
    let hz = objc2_core_graphics::CGDisplayMode::refresh_rate(display_mode(id).as_deref());
    (hz >= 1.0).then(|| hz.round() as u32)
}

#[cfg(test)]
mod tests {
    /// The main display's top-left corner is 0, 0, and it's as many points
    /// as its pixels at its scale (1 or 2); one that isn't connected isn't
    /// anywhere.
    #[test]
    fn where_the_displays_are() {
        let main = objc2_core_graphics::CGMainDisplayID().to_string();
        let b = super::display_bounds(&main).expect("the main display");
        assert_eq!((b[0], b[1]), (0, 0));
        let (pw, ph) = super::display_pixels(Some(&main)).unwrap();
        assert!((pw as i32 == b[2] && ph as i32 == b[3]) || (pw as i32 == 2 * b[2] && ph as i32 == 2 * b[3]), "{b:?} for {pw}×{ph} pixels");
        assert_eq!(super::display_bounds("4000000000"), None);
        assert_eq!(super::display_bounds("not a display"), None);
    }
}
