//! How much of the graphics card's video decoder another app keeps while
//! scrubbing's decoders are busy (a video in a browser, while you scrub).
//! The decoder takes turns between everything decoding on it, whatever their
//! priority: three of scrubbing's left one other app a fifth of it, one left
//! it about half.
//!
//! `ROLE=load`: `DECODERS` decoders (3, as the scrub engine has), each
//! decoding keyframe to keyframe as fast as it can, saying each second how
//! busy other apps keep the decoder (as the scrub engine reads it).
//! Otherwise the other app: one decoder, as fast as it can, starting
//! `DELAY` seconds in. Each prints the frames it decoded a second over
//! `SECS` (8).
//!
//!   cargo run -p capture --example decode_share -- <clip>
#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use std::time::{Duration, Instant};

    use capture::win::decode::Decoder;

    let path = std::path::PathBuf::from(std::env::args().nth(1).expect("clip path"));
    let env = |k: &str| std::env::var(k).ok().and_then(|s| s.parse::<f64>().ok());
    let secs = env("SECS").unwrap_or(8.0);
    let load = std::env::var("ROLE").is_ok_and(|r| r == "load");
    std::thread::sleep(Duration::from_secs_f64(env("DELAY").unwrap_or(0.0)));
    let luid = Decoder::default_luid().expect("no graphics card");
    let workers = if load { env("DECODERS").unwrap_or(3.0) as usize } else { 1 };
    if load {
        // Made before the other app starts decoding: it has to show up anyway.
        let mut others = capture::win::gpu::GpuUse::video_decode().expect("no GPU counters");
        std::thread::spawn(move || {
            for s in 1..=secs as u32 {
                std::thread::sleep(Duration::from_secs(1));
                println!("  {s} s: other apps keep the video decoder {:.0}% busy", others.others_now(std::process::id()));
            }
        });
    }
    let threads: Vec<_> = (0..workers)
        .map(|w| {
            let path = path.clone();
            std::thread::spawn(move || -> anyhow::Result<u64> {
                let mut d = if load { Decoder::open_direct_pooled(&path, luid, 256 << 20)? } else { Decoder::open_direct(&path, u32::MAX, Some(luid))? };
                let keys = d.keyframes();
                anyhow::ensure!(keys.len() > 2, "too few keyframes");
                let stretches = keys.len() - 1;
                let until = Instant::now() + Duration::from_secs_f64(secs);
                let (mut n, mut g) = (0u64, w * stretches / workers);
                while Instant::now() < until {
                    let s = g % stretches;
                    let (key, last) = (keys[s], keys[s + 1] - 1);
                    d.frame(key)?;
                    n += 1;
                    let mut at = key;
                    while at < last && Instant::now() < until {
                        match d.next()? {
                            Some(p) => {
                                at = p.index;
                                n += 1;
                            }
                            None => break,
                        }
                    }
                    d.flush();
                    g += 1;
                }
                Ok(n)
            })
        })
        .collect();
    let mut total = 0;
    for t in threads {
        total += t.join().expect("a decoder panicked")?;
    }
    let who = if load { format!("scrubbing's {workers} decoders") } else { "the other app".to_owned() };
    println!("{who}: {:.0} fps", total as f64 / secs);
    Ok(())
}

#[cfg(not(windows))]
fn main() {}
