//! Time VideoToolbox decoding (`capture::mac::decode`) and check its frames:
//! `cargo run --release -p capture --example mac_decode_bench -- <clip> [width]`
//! (a width: RGBA at that width; none: NV12 for the screen). `FPS=` numbers
//! frames at that rate (default 60).
//! `VERIFY=n`: n frames reached by jumping must match the same frames reached
//! by decoding in order from the start (and ffmpeg's, roughly).
#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    use capture::mac::decode::{Decoder, Output};
    use std::time::Instant;
    let path = std::path::PathBuf::from(std::env::args().nth(1).expect("clip path"));
    // A width: RGBA at that width (as thumbnails take them); none: NV12 for the screen.
    let width: Option<u32> = std::env::args().nth(2).and_then(|w| w.parse().ok());
    let output = width.map_or(Output::Screen, Output::Rgba);
    let fps: f64 = std::env::var("FPS").ok().and_then(|f| f.parse().ok()).unwrap_or(60.0);
    let open = |output: Output| -> anyhow::Result<Decoder> {
        let mut d = Decoder::open(&path, output)?;
        d.set_fps(fps);
        Ok(d)
    };
    let med = |v: &mut Vec<f64>| {
        v.sort_by(f64::total_cmp);
        format!("median {:.1} ms (10-90%: {:.1}-{:.1})", v[v.len() / 2], v[v.len() / 10], v[v.len() * 9 / 10])
    };

    if let Some(n) = std::env::var("VERIFY").ok().and_then(|n| n.parse::<u64>().ok()) {
        // Every frame in order, kept as RGBA by number; then jumps must match.
        let mut seq = Decoder::open(&path, Output::Rgba(width.unwrap_or(u32::MAX)))?;
        seq.set_fps(fps);
        let mut all = std::collections::BTreeMap::new();
        while let Some(p) = seq.next()? {
            all.insert(p.index, p.rgba);
        }
        let last = *all.keys().last().unwrap();
        let mut jump = Decoder::open(&path, Output::Rgba(width.unwrap_or(u32::MAX)))?;
        jump.set_fps(fps);
        let mut bad = 0;
        for k in 0..n {
            let i = (k * 7919) % (last + 1);
            let p = jump.frame(i)?.expect("frame");
            let want = all.range(..=i).next_back().unwrap();
            if p.index != *want.0 || &p.rgba != want.1 {
                println!("frame {i}: got {} want {}", p.index, want.0);
                bad += 1;
            }
        }
        // Numbers: none repeated, gaps only where frames were dropped.
        let keys: Vec<u64> = all.keys().copied().collect();
        let skips = keys.windows(2).filter(|w| w[1] > w[0] + 1).count();
        println!("{} frames in order ({skips} gaps), {n} jumps checked, {bad} differences", keys.len());
        if let Some(out) = std::env::var_os("DUMP") {
            let i: u64 = std::env::var("DUMP_FRAME").ok().and_then(|s| s.parse().ok()).unwrap_or(0);
            std::fs::write(out, all.range(..=i).next_back().unwrap().1)?;
            println!("dumped frame {i} ({}x{})", jump.width, jump.height);
        }
        return Ok(());
    }

    let t = Instant::now();
    let mut d = open(output)?;
    println!("open {}x{} (source {:?}) in {:.1} ms, {output:?}", d.width, d.height, d.source_size(), t.elapsed().as_secs_f64() * 1000.0);
    let t = Instant::now();
    let first = d.frame(0)?.expect("frame");
    println!("first frame {} in {:.1} ms", first.index, t.elapsed().as_secs_f64() * 1000.0);
    drop(first);

    // Hold frames as playback does (a dozen ahead, plus the shown and
    // retired ones): the decoder's buffers must not run out.
    let hold: usize = std::env::var("HOLD").ok().and_then(|n| n.parse().ok()).unwrap_or(20);
    let mut held = std::collections::VecDeque::new();
    let t = Instant::now();
    let mut n = 0;
    while n < 600 {
        let Some(p) = d.next()? else { break };
        held.push_back(p);
        if held.len() > hold {
            held.pop_front();
        }
        n += 1;
    }
    drop(held);
    let s = t.elapsed().as_secs_f64();
    println!("{n} frames in order: {:.0} fps ({:.2} ms/frame)", n as f64 / s, s * 1000.0 / n as f64);

    let keys = d.keyframes();
    let last = *keys.last().unwrap();
    // Long gaps only (the first second has a few close together).
    let gaps: Vec<(u64, u64)> = keys.windows(2).map(|w| (w[0], w[1])).filter(|(a, b)| b - a >= 60).collect();
    println!("{} keyframes, typical gap {} frames", keys.len(), gaps.first().map_or(0, |g| g.1 - g.0));
    for (label, at) in [("keyframes", 0.0), ("halfway between keyframes", 0.5), ("just before a keyframe", 1.0)] {
        let mut v = Vec::new();
        for j in 0..40usize {
            let (a, b) = gaps[j * 7 % gaps.len()];
            let target = if at >= 1.0 { b - 1 } else { a + ((b - a) as f64 * at) as u64 };
            let s = Instant::now();
            d.frame(target)?;
            v.push(s.elapsed().as_secs_f64() * 1000.0);
        }
        println!("jumps to {label}: {}", med(&mut v));
    }
    let _ = last;
    // Stepping back one frame at a time (paused, pressing ←).
    let mut v = Vec::new();
    for i in (gaps[3].0 + 30..gaps[3].0 + 60).rev() {
        let s = Instant::now();
        d.frame(i)?;
        v.push(s.elapsed().as_secs_f64() * 1000.0);
    }
    println!("stepping back: {}", med(&mut v));
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn main() {}
