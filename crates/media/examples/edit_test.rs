//! `cargo run -p media --example edit_test -- <clip>`: probe, decode, frame, render.
fn main() -> anyhow::Result<()> {
    let src = std::path::PathBuf::from(std::env::args().nth(1).expect("clip path"));
    let info = media::probe(&src)?;
    println!("{info:?}\nsources: {:?}", info.source_tracks().iter().map(|a| a.label()).collect::<Vec<_>>());
    let t = std::time::Instant::now();
    let pcm = media::decode_audio(&src, info.source_tracks()[0].index)?;
    println!("decoded {} samples ({:.1}s) in {:?}", pcm.len(), pcm.len() as f64 / 2.0 / media::PREVIEW_RATE as f64, t.elapsed());
    let t = std::time::Instant::now();
    let f = media::frame_at(&src, info.duration / 2.0, 640)?;
    println!("frame {}x{} in {:?}", f.width, f.height, t.elapsed());
    let mut edit = media::Edit::new(&info);
    edit.start = info.snap(1.0 + 7.0 / info.fps);
    edit.end = info.snap(3.0);
    edit.tracks[0].gain = media::from_db(-6.0);
    // The second source fades up from silence: exercises the fader-curve expression.
    if let Some(t) = edit.tracks.get_mut(1) {
        t.points = vec![media::VolumePoint { t: 1.5, db: media::SILENT_DB }, media::VolumePoint { t: 2.5, db: 0.0 }];
    }
    let out = std::env::temp_dir().join("hc_edit_test.mp4");
    let t = std::time::Instant::now();
    media::render_to(&src, &info, &edit, &out, Some("test-id"))?;
    let o = media::probe(&out)?;
    println!("rendered in {:?}: dur={:.4} (want {:.4}) frames≈{:.1} id={:?} audio={:?}", t.elapsed(), o.duration, edit.duration(), o.duration * o.fps, o.id, o.audio.iter().map(|a| a.label()).collect::<Vec<_>>());
    if std::env::var_os("KEEP").is_some() {
        println!("kept {}", out.display());
    } else {
        std::fs::remove_file(out)?;
    }
    Ok(())
}
