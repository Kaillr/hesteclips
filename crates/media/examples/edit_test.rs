//! `cargo run -p media --example edit_test -- <clip>`: probe, decode, frame, render.
fn main() -> anyhow::Result<()> {
    let src = std::path::PathBuf::from(std::env::args().nth(1).expect("clip path"));
    let tmp = std::env::temp_dir().join("hc_edit_test").join(src.file_name().unwrap());
    std::fs::create_dir_all(tmp.parent().unwrap())?;
    std::fs::copy(&src, &tmp)?;
    let info = media::probe(&tmp)?;
    println!("{info:?}\nsources: {:?}", info.source_tracks().iter().map(|a| a.label()).collect::<Vec<_>>());
    let t = std::time::Instant::now();
    let pcm = media::decode_audio(&tmp, info.source_tracks()[0].index)?;
    println!("decoded {} samples ({:.1}s) in {:?}", pcm.len(), pcm.len() as f64 / 2.0 / media::PREVIEW_RATE as f64, t.elapsed());
    let t = std::time::Instant::now();
    let f = media::frame_at(&tmp, info.duration / 2.0, 640)?;
    println!("frame {}x{} in {:?}", f.width, f.height, t.elapsed());
    let mut edit = media::Edit::new(&info);
    edit.start = info.snap(1.0 + 7.0 / info.fps);
    edit.end = info.snap(3.0);
    edit.tracks[0].gain = media::from_db(-6.0);
    media::save_edit(&tmp, &edit)?;
    let t = std::time::Instant::now();
    let out = media::render(&tmp, &info, &edit)?;
    println!("rendered in {:?} -> current={:?}", t.elapsed(), media::rendered_if_current(&tmp).is_some());
    let o = media::probe(&out)?;
    println!("out: dur={:.4} (want {:.4}) frames≈{:.1} audio={:?}", o.duration, edit.duration(), o.duration * o.fps, o.audio.iter().map(|a| a.label()).collect::<Vec<_>>());
    std::fs::remove_dir_all(tmp.parent().unwrap())?;
    Ok(())
}
