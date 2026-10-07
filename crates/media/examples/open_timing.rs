//! How long opening a clip in the player takes, and that the one-pass
//! streaming decode gives the same sound as decoding each track alone:
//! `cargo run -p media --example open_timing -- <clip>`
use std::time::Instant;

fn main() {
    let clip = std::path::PathBuf::from(std::env::args().nth(1).expect("clip"));
    let t = Instant::now();
    let info = media::probe(&clip).unwrap();
    println!("probe: {:?}", t.elapsed());

    let indices: Vec<usize> = info.source_tracks().iter().map(|a| a.index).collect();
    let t = Instant::now();
    let (tx, rx) = std::sync::mpsc::channel();
    let pcm = media::pcm::decode_streaming(&clip, &indices, info.duration, move |_, r| {
        let _ = tx.send(r.map_err(|e| e.to_string()));
    });
    while pcm.tracks()[0].len() < 2 * media::PREVIEW_RATE as usize && !pcm.is_done() {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    println!("streaming, {} tracks: first second after {:?}", indices.len(), t.elapsed());
    rx.recv().unwrap().unwrap();
    println!("streaming: all done after {:?}", t.elapsed());

    for (k, &i) in indices.iter().enumerate() {
        let t = Instant::now();
        let old = media::decode_audio(&clip, i).unwrap();
        let new = pcm.tracks()[k].to_vec();
        let n = old.len().min(new.len());
        let worst = old[..n].iter().zip(&new[..n]).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
        println!("track {i}: alone {:?}; {} vs {} samples, largest difference {worst:e}", t.elapsed(), old.len(), new.len());
    }
}
