//! Do decoders give their hardware sessions back? Opens and drops N decoders,
//! then records a second: `cargo run --release -p capture --example vt_leak -- <clip> [n]`.
#[cfg(target_os = "macos")]
fn main() {
    let clip = std::path::PathBuf::from(std::env::args().nth(1).expect("clip"));
    let n: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(64);
    for i in 0..n {
        match capture::decode::Decoder::open(&clip, capture::decode::Output::Screen) {
            Ok(mut d) => {
                let _ = d.frame(30);
            }
            Err(e) => {
                println!("decoder {i}: {e:#}");
                break;
            }
        }
    }
    println!("opened and dropped {n} decoders");
    // Held open at once, then a recording.
    let held: Vec<_> = (0..n).map_while(|i| match capture::decode::Decoder::open(&clip, capture::decode::Output::Screen) {
        Ok(mut d) => { let _ = d.frame(30); Some(d) }
        Err(e) => { println!("holding decoder {i}: {e:#}"); None }
    }).collect();
    println!("holding {} decoders", held.len());
    let live = capture::mixer::LiveAudio::new();
    let mut rec = capture::default_recorder(live);
    let s = capture::EncodeSettings {
        output_dir: std::env::temp_dir().join("hc"), container_ext: "mp4".into(), fps: 60, video_bitrate_kbps: 20000,
        target_height: None, keyframe_interval_secs: 2, use_hardware: true, replay_seconds: 5,
        video: capture::VideoSource::Screen { id: String::new() }, away_screen: None, webcam: None, sources: Vec::new(),
    };
    println!("record: {:?}", rec.start(capture::Mode::ReplayBuffer, &s).map(|_| "ok"));
    let _ = rec.stop();
    drop(held);
}
#[cfg(not(target_os = "macos"))]
fn main() {}
