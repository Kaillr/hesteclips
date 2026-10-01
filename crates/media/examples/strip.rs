//! Time the timeline filmstrip: `cargo run -p media --example strip -- <clip>`.
fn main() -> anyhow::Result<()> {
    let src = std::path::PathBuf::from(std::env::args().nth(1).unwrap());
    let t = std::time::Instant::now();
    let mut n = 0;
    let mut first = None;
    media::keyframe_strip(&src, 112, |time, f| {
        n += 1;
        first.get_or_insert(t.elapsed());
        assert_eq!(f.rgba.len(), (f.width * f.height * 4) as usize);
        let _ = time;
        true
    })?;
    println!("{n} frames; first after {:?}, all after {:?}", first.unwrap_or_default(), t.elapsed());
    Ok(())
}
