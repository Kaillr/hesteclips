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
