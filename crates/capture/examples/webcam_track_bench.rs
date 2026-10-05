//! Numbers for the "webcam on its own track" plan.
//!
//! Save-time render (decode + composite + encode, as a save would):
//! `cargo run --release -p capture --example webcam_track_bench -- render <screen.mp4> [camera.mp4]`
//! (`FRAMES=n` stops early.)
//!
//! Two encoders while capturing the main display (needs Screen Recording and
//! camera permission):
//! `cargo run --release -p capture --example webcam_track_bench -- live [secs]`
//! runs screen only, webcam composited (today) and webcam separate (the plan).
//! `CAMERA=<part of a name>` picks the camera; `MODES=Screen,Separate` a subset;
//! `HEIGHT=1440` records at that height instead of the display's own.
#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    use capture::mac::bench::{LiveMode, RenderMode, live, save_render};
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dir = std::env::temp_dir().join("hesteclips-bench");
    std::fs::create_dir_all(&dir)?;
    match args.first().map(String::as_str) {
        Some("render") => {
            let screen = std::path::PathBuf::from(&args[1]);
            let camera = args.get(2).map(std::path::PathBuf::from);
            let frames = std::env::var("FRAMES").ok().and_then(|f| f.parse().ok());
            let out = dir.join("render.mp4");
            if std::env::var("ONLY_FULL").is_ok() {
                save_render(&screen, camera.as_deref(), &out, RenderMode::Full, 20000, false, frames)?;
                return Ok(());
            }
            save_render(&screen, None, &out, RenderMode::DecodeOnly, 20000, false, frames)?;
            save_render(&screen, camera.as_deref(), &out, RenderMode::Compose, 20000, false, frames)?;
            save_render(&screen, camera.as_deref(), &out, RenderMode::Full, 20000, false, frames)?;
            save_render(&screen, camera.as_deref(), &out, RenderMode::Full, 20000, true, frames)?;
            println!("last output: {}", out.display());
        }
        Some("live") => {
            let secs: f64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(20.0);
            let want = std::env::var("CAMERA").unwrap_or_else(|_| "MacBook".into());
            let cams = capture::webcam::list_cameras();
            let cam = cams.iter().find(|c| c.name.contains(&want)).or(cams.first()).expect("no camera");
            println!("camera: {}", cam.name);
            let modes = std::env::var("MODES").unwrap_or_else(|_| "Screen,Composited,Separate".into());
            for m in modes.split(',') {
                let mode = match m {
                    "Screen" => LiveMode::Screen,
                    "Composited" => LiveMode::Composited,
                    _ => LiveMode::Separate,
                };
                live(mode, secs, &cam.id, &dir, 20000, 6000, std::env::var("HEIGHT").ok().and_then(|h| h.parse().ok()))?;
            }
        }
        _ => eprintln!("usage: webcam_track_bench render <screen.mp4> [camera.mp4] | live [secs]"),
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn main() {}
