//! Time a smart render of a trim and check frame count + audio layout.
fn main() -> anyhow::Result<()> {
    let src = std::path::PathBuf::from(std::env::args().nth(1).unwrap());
    let (a, b): (f64, f64) = (std::env::args().nth(2).unwrap().parse()?, std::env::args().nth(3).unwrap().parse()?);
    let info = media::probe(&src)?;
    let mut e = media::Edit::new(&info);
    e.start = info.snap(a); e.end = info.snap(b);
    if let Some(t) = e.tracks.first_mut() { t.points = vec![media::VolumePoint { t: a + 1.0, db: -20.0 }, media::VolumePoint { t: a + 3.0, db: 0.0 }]; }
    let out = std::env::temp_dir().join("smart_out.mp4");
    let t = std::time::Instant::now();
    media::render_with_progress(&src, &info, &e, &out, |p| eprint!("{:.0}% ", p * 100.0))?;
    eprintln!();
    let o = media::probe(&out)?;
    println!("{:.2}s render; out {:.4}s (want {:.4}); {} frames (want {}); audio {:?}",
        t.elapsed().as_secs_f64(), o.duration, e.duration(), (o.duration * o.fps).round(), (e.duration() * info.fps).round(),
        o.audio.iter().map(|a| a.label()).collect::<Vec<_>>());
    Ok(())
}
