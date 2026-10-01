//! Render a keyframed volume ramp on a copy and measure levels over time.
use media::VolumePoint;
fn main() -> anyhow::Result<()> {
    let src = std::path::PathBuf::from(std::env::args().nth(1).unwrap());
    let info = media::probe(&src)?;
    let mut e = media::Edit::new(&info);
    e.start = 5.0; e.end = 9.0;
    e.tracks[0].points = vec![VolumePoint { t: 6.0, db: -30.0 }, VolumePoint { t: 8.0, db: 0.0 }];
    for t in [5.0, 6.0, 7.0, 8.0, 9.0] { print!("db_at({t})={:.1}  ", e.tracks[0].db_at(t)); }
    println!();
    let out = src.with_file_name("env_out.mp4");
    media::render_to(&src, &info, &e, &out)?;
    println!("rendered {:.2}s", media::probe(&out)?.duration);
    Ok(())
}
