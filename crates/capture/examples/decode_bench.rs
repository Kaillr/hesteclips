//! Time in-process hardware decoding and check its frames:
//! `cargo run --release -p capture --example decode_bench -- <clip> [width]`.
//! `DUMP=<file>` writes frame 0's RGBA, to compare with ffmpeg.
#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use std::time::Instant;
    let path = std::path::PathBuf::from(std::env::args().nth(1).expect("clip path"));
    let width: u32 = std::env::args().nth(2).and_then(|w| w.parse().ok()).unwrap_or(1280);

    // DIRECT=1: jumps through the decoder fed directly against the file
    // reader's, alternating so other load hits both alike. Keyframes (every
    // KEY_EVERY frames, default 120) and halfway between.
    if std::env::var_os("DIRECT").is_some() {
        let mut direct = capture::win::decode::Decoder::open_direct(&path, width, None)?;
        let mut reader = capture::win::decode::Decoder::open_file_reader(&path, width, None)?;
        // Numbered alike, as the app does (one rate, from ffprobe).
        direct.set_fps(reader.fps);
        println!("{} vs {}", direct.way(), reader.way());
        let gop: u64 = std::env::var("KEY_EVERY").ok().and_then(|n| n.parse().ok()).unwrap_or(120);
        let frames = std::env::var("FRAMES").ok().and_then(|n| n.parse().ok()).unwrap_or(4000u64);
        for (label, extra) in [("keyframes", 0), ("halfway between keyframes", gop / 2)] {
            let (mut a, mut b) = (Vec::new(), Vec::new());
            for round in 0..4u64 {
                let targets: Vec<u64> = (0..10).map(|j| ((round * 10 + j) * 7 % (frames / gop).max(1)) * gop + extra).collect();
                for (dec, out) in [(&mut direct, &mut a), (&mut reader, &mut b)] {
                    for &t in &targets {
                        let s = Instant::now();
                        dec.frame(t)?;
                        out.push(s.elapsed().as_secs_f64() * 1000.0);
                    }
                }
            }
            let med = |v: &mut Vec<f64>| {
                v.sort_by(f64::total_cmp);
                format!("median {:.1} ms (10-90%: {:.1}-{:.1})", v[v.len() / 2], v[v.len() / 10], v[v.len() * 9 / 10])
            };
            println!("{label}: direct {}, file reader {}", med(&mut a), med(&mut b));
        }
        let s = Instant::now();
        let mut n = 0;
        direct.frame(0)?;
        while n < 600 && direct.next()?.is_some() {
            n += 1;
        }
        let dt = s.elapsed().as_secs_f64();
        let s = Instant::now();
        let mut m = 0;
        reader.frame(0)?;
        while m < 600 && reader.next()?.is_some() {
            m += 1;
        }
        println!("in order: direct {:.0} fps, file reader {:.0} fps", n as f64 / dt, m as f64 / s.elapsed().as_secs_f64());
        return Ok(());
    }
    // VERIFY=n: n frames (spread over the clip, in a jumbled order, plus a run
    // in order) decoded both ways must be the same frame, pixel for pixel.
    if let Some(n) = std::env::var("VERIFY").ok().and_then(|n| n.parse::<u64>().ok()) {
        let mut direct = capture::win::decode::Decoder::open_direct(&path, width, None)?;
        let mut reader = capture::win::decode::Decoder::open_file_reader(&path, width, None)?;
        // Numbered alike, as the app does (one rate, from ffprobe).
        direct.set_fps(reader.fps);
        let total = std::env::var("FRAMES").ok().and_then(|f| f.parse().ok()).unwrap_or(3000u64);
        let mut bad = 0;
        let mut check = |what: &str, a: Option<capture::win::decode::Picture>, b: Option<capture::win::decode::Picture>| {
            match (a, b) {
                (Some(a), Some(b)) if a.index == b.index && a.rgba == b.rgba => {}
                (None, None) => {}
                (Some(a), Some(b)) => {
                    let diff = a.rgba.iter().zip(&b.rgba).filter(|(x, y)| x != y).count();
                    println!("{what}: direct frame {} vs file reader frame {}, {diff} bytes differ", a.index, b.index);
                    bad += 1;
                }
                (a, b) => {
                    println!("{what}: direct {:?} vs file reader {:?}", a.map(|p| p.index), b.map(|p| p.index));
                    bad += 1;
                }
            }
        };
        for k in 0..n {
            let i = (k * 7919) % total;
            check(&format!("frame {i}"), direct.frame(i)?, reader.frame(i)?);
        }
        let start = total / 3;
        check(&format!("frame {start}"), direct.frame(start)?, reader.frame(start)?);
        for _ in 0..200 {
            check("next", direct.next()?, reader.next()?);
        }
        println!("{} ({} vs {}): {} checks, {bad} differences", path.display(), direct.way(), reader.way(), n + 201);
        return Ok(());
    }
    // PAR=n: n decoders jumping to different keyframes at once (as a scrub
    // through unbuilt frames does), for total jumps per second.
    if let Some(n) = std::env::var("PAR").ok().and_then(|n| n.parse::<usize>().ok()) {
        const JUMPS: usize = 40;
        let mut decoders = Vec::new();
        for _ in 0..n {
            decoders.push(capture::win::decode::Decoder::open(&path, width, None)?);
        }
        let t = Instant::now();
        let threads: Vec<_> = decoders
            .into_iter()
            .enumerate()
            .map(|(k, mut d)| {
                std::thread::spawn(move || {
                    // Keyframes far apart, never the same for two decoders.
                    for j in 0..JUMPS {
                        let target = ((j * n + k) * 240 % 17_000) as u64;
                        d.frame(target).unwrap();
                    }
                })
            })
            .collect();
        for th in threads {
            th.join().unwrap();
        }
        let s = t.elapsed().as_secs_f64();
        println!("{n} decoders: {} jumps in {s:.2} s = {:.0} jumps/s ({:.0} ms each)", n * JUMPS, (n * JUMPS) as f64 / s, s * 1000.0 / JUMPS as f64);
        return Ok(());
    }

    let t = Instant::now();
    let mut d = capture::win::decode::Decoder::open(&path, width, None)?;
    println!("open {}x{} at {} fps: {:?}", d.width, d.height, d.fps, t.elapsed());

    // INDICES=n: the first n frames' numbers, to check none repeat or skip.
    if let Some(n) = std::env::var("INDICES").ok().and_then(|n| n.parse::<usize>().ok()) {
        let got: Vec<u64> = (0..n).filter_map(|_| d.next().ok().flatten().map(|p| p.index)).collect();
        let repeats = got.windows(2).filter(|w| w[1] == w[0]).count();
        let skips = got.windows(2).filter(|w| w[1] > w[0] + 1).count();
        println!("{n} frames: {repeats} repeated numbers, {skips} skipped; first: {:?}", &got[..got.len().min(16)]);
        return Ok(());
    }
    // FULL=1: decode every frame (as the scrub proxy does) and report.
    if std::env::var_os("FULL").is_some() {
        let t = Instant::now();
        let mut n = 0u64;
        let mut copied = 0usize;
        while let Some(p) = d.next()? {
            n += 1;
            copied += p.rgba.len();
        }
        println!("all {n} frames: {:?} ({:.0} fps), {} MB copied back", t.elapsed(), n as f64 / t.elapsed().as_secs_f64(), copied / 1_000_000);
        return Ok(());
    }
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
        let mut fresh = capture::win::decode::Decoder::open(&path, width, None)?;
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
    // SEEKS=a,b,c: time reaching each of these frames, in that order.
    if let Some(list) = std::env::var("SEEKS").ok() {
        for target in list.split(',').filter_map(|s| s.trim().parse::<u64>().ok()) {
            let t = Instant::now();
            let p = d.frame(target)?;
            println!("frame {target}: got {:?} in {:?}", p.map(|p| p.index), t.elapsed());
        }
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
