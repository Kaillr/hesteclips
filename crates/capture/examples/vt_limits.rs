//! Which sizes VideoToolbox's hardware H.264 encoder takes:
//! `cargo run --release -p capture --example vt_limits`.
#[cfg(target_os = "macos")]
fn main() {
    for (w, h) in [(3600, 2338), (4096, 2304), (4096, 4096), (4098, 2304), (5120, 2160), (6720, 2836), (8192, 4320)] {
        println!("{w}x{h}: {}", capture::mac::probe_encoder(w, h));
    }
}
#[cfg(not(target_os = "macos"))]
fn main() {}
