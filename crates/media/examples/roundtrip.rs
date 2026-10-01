//! Simulates the editor's Done path on a copy: save sidecar, render, check state.
fn main() -> anyhow::Result<()> {
    let src = std::path::PathBuf::from(std::env::args().nth(1).unwrap());
    let info = media::probe(&src)?;
    let mut e = media::Edit::new(&info);
    e.start = info.snap(5.0); e.end = info.snap(9.0);
    e.tracks[0].muted = false; e.tracks[0].gain = media::from_db(6.0);
    media::save_edit(&src, &e)?;
    println!("before render: current={}", media::rendered_if_current(&src).is_some());
    media::render(&src, &info, &e)?;
    println!("after render:  current={}", media::rendered_if_current(&src).is_some());
    let back = media::load_edit(&src).unwrap();
    println!("reloaded: start={:.3} end={:.3} gain_db={:.1}", back.start, back.end, media::to_db(back.tracks[0].gain));
    let o = media::probe(&media::rendered_path(&src))?;
    println!("rendered: {:.3}s, tracks={:?}", o.duration, o.audio.iter().map(|a| a.label()).collect::<Vec<_>>());
    // Edit again -> sidecar newer than render -> stale until re-rendered.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    e.end = info.snap(8.0); media::save_edit(&src, &e)?;
    println!("after re-edit: current={}", media::rendered_if_current(&src).is_some());
    media::revert(&src);
    println!("after revert: sidecar={} render={}", media::sidecar_path(&src).exists(), media::rendered_path(&src).exists());
    Ok(())
}
