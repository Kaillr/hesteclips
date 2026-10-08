//! How busy a process keeps the graphics card drawing, each second (what the
//! game capture decides "is it drawing?" by).
//!   cargo run -p capture --example gpu_use -- <pid>
#[cfg(windows)]
fn main() {
    let pid: u32 = std::env::args().nth(1).and_then(|p| p.parse().ok()).expect("a process id");
    let mut gpu = capture::win::gpu::GpuUse::new().expect("no GPU counters");
    for _ in 0..5 {
        std::thread::sleep(std::time::Duration::from_secs(1));
        println!("{pid}: {:.1}% of the 3D engine", gpu.percent(pid));
    }
}

#[cfg(not(windows))]
fn main() {}
