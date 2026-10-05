//! Render a clip with export settings and report what came out:
//! `cargo run --release -p media --example render_output -- <clip> <out> [height] [fps] [kbps] [max_mb] [mix_only]`
//! (`-` skips a setting; `START=`/`END=` trim, in seconds.)
fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let opt = |i: usize| a.get(i).filter(|s| *s != "-").and_then(|s| s.parse::<u32>().ok());
    let src = std::path::PathBuf::from(&a[0]);
    let info = media::probe(&src)?;
    let mut edit = media::Edit::new(&info);
    if let Some(s) = std::env::var("START").ok().and_then(|s| s.parse().ok()) {
        edit.start = s;
    }
    if let Some(e) = std::env::var("END").ok().and_then(|s| s.parse().ok()) {
        edit.end = e;
    }
    edit.output = media::Output { height: opt(2), fps: opt(3), video_kbps: opt(4), max_mb: opt(5), mix_only: a.get(6).is_some_and(|s| s == "1") };
    let est = edit.output.estimate_bytes(&info, &edit);
    let t = std::time::Instant::now();
    media::render_to(&src, &info, &edit, std::path::Path::new(&a[1]), None)?;
    let size = std::fs::metadata(&a[1])?.len();
    let out = media::probe(std::path::Path::new(&a[1]))?;
    println!(
        "{:.1} s: {}x{} {:.2} fps {:.2} s, {} audio, {:.1} MB (estimate {:.1} MB)",
        t.elapsed().as_secs_f64(),
        out.width,
        out.height,
        out.fps,
        out.duration,
        out.audio.len(),
        size as f64 / 1048576.0,
        est.unwrap_or(0) as f64 / 1048576.0
    );
    Ok(())
}
