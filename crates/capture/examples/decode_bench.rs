//! Time in-process hardware decoding and check its frames:
//! `cargo run --release -p capture --example decode_bench -- <clip> [width]`.
//! `DUMP=<file>` writes frame 0's RGBA, to compare with ffmpeg.
#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use std::time::Instant;
    let path = std::path::PathBuf::from(std::env::args().nth(1).expect("clip path"));
    let width: u32 = std::env::args().nth(2).and_then(|w| w.parse().ok()).unwrap_or(1280);

    let t = Instant::now();
    let mut d = capture::win::decode::Decoder::open(&path, width)?;
    println!("open {}x{} at {} fps: {:?}", d.width, d.height, d.fps, t.elapsed());

    let t = Instant::now();
    let first = d.next()?.expect("a frame");
    println!("frame {} after {:?}, alpha {}", first.index, t.elapsed(), first.rgba[3]);
    if let Some(out) = std::env::var_os("DUMP") {
        std::fs::write(out, &first.rgba)?;
    }

    let t = Instant::now();
    let mut n = 0;
    let mut last = first.index;
    while n < 300 {
        let Some(p) = d.next()? else { break };
        // Numbered by time: never backwards (a file with dropped frames, whose
        // nominal rate is below the real one, can give two frames one number).
        assert!(p.index >= last, "frames in order: {} after {last}", p.index);
        last = p.index;
        n += 1;
    }
    let s = t.elapsed().as_secs_f64();
    println!("{n} frames in order: {:.0} fps ({:.2} ms/frame)", n as f64 / s, s * 1000.0 / n as f64);

    // SEEKDUMP=<index>:<file>: that frame (reached by seeking), to check it's the right picture.
    if let Some(spec) = std::env::var("SEEKDUMP").ok() {
        let (i, out) = spec.split_once(':').expect("index:file");
        let mut fresh = capture::win::decode::Decoder::open(&path, width)?;
        let i: u64 = i.parse()?;
        // NOSEEK=1: get there by decoding every frame from the start.
        let p = if std::env::var_os("NOSEEK").is_some() {
            loop {
                let p = fresh.next()?.expect("frame");
                if p.index >= i {
                    break p;
                }
            }
        } else {
            fresh.frame(i)?.expect("frame")
        };
        std::fs::write(out, &p.rgba)?;
        println!("dumped frame {}", p.index);
        return Ok(());
    }
    // Exact frames: forward nearby, far, backwards.
    for target in [400u64, 430, 1855, 735, 6000, 6001] {
        let t = Instant::now();
        let p = d.frame(target)?;
        println!("frame {target}: got {:?} in {:?}", p.map(|p| p.index), t.elapsed());
    }
    Ok(())
}

#[cfg(not(windows))]
fn main() {}
