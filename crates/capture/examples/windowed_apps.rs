//! Games and apps capture's view of the screen: the apps with a window, the
//! app in front, and what the window queries cost.
//! `cargo run --release -p capture --example windowed_apps`
fn main() {
    let t = std::time::Instant::now();
    let apps = capture::list_windowed_apps();
    println!("{} windowed apps in {:?}", apps.len(), t.elapsed());
    for a in &apps {
        println!("  {} — {}", a.name, a.id);
    }
    let t = std::time::Instant::now();
    for _ in 0..100 {
        let _ = capture::foreground_exe();
    }
    println!("front: {:?}, {:?} per call", capture::foreground_exe(), t.elapsed() / 100);
    #[cfg(target_os = "macos")]
    capture::mac::bench_windows();
}
