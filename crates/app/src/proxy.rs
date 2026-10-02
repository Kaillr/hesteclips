//! Scrub proxy: every frame of the clip as a small JPEG, kept in memory.
//!
//! Seeking the real file costs ~0.2 s a frame, far too slow to follow a drag. One
//! ffmpeg pass (~2 s for a minute of 60 fps footage) decodes every frame at a
//! small size; after that any frame is a JPEG decode away, so scrubbing updates
//! the picture on every mouse move. Frames arrive in order while it builds, so the
//! start of the clip is scrubbable almost immediately.

use std::io::Read;
use std::path::Path;
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};

/// Proxy frame width; enough for a crisp preview while dragging.
const WIDTH: u32 = 640;

pub struct Proxy {
    frames: Arc<Mutex<Vec<Vec<u8>>>>,
    child: Option<Child>,
}

impl Proxy {
    pub fn build(ctx: &egui::Context, source: &Path, fps: f64) -> Self {
        let frames = Arc::new(Mutex::new(Vec::new()));
        let child = media::ffmpeg()
            .args(["-hide_banner", "-loglevel", "error", "-i"])
            .arg(source)
            // fps= pins one output frame per source frame index, even for VFR input.
            .args(["-an", "-vf", &format!("fps={fps},scale={WIDTH}:-2:flags=bilinear")])
            .args(["-c:v", "mjpeg", "-q:v", "6", "-f", "image2pipe", "-"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok();
        let mut child = child;
        if let Some(stdout) = child.as_mut().and_then(|c| c.stdout.take()) {
            let (frames, ctx) = (frames.clone(), ctx.clone());
            std::thread::spawn(move || split_jpegs(stdout, &frames, &ctx));
        }
        Self { frames, child }
    }

    /// How many frames are ready (they arrive in order).
    pub fn ready(&self) -> usize {
        self.frames.lock().map_or(0, |f| f.len())
    }

    /// Decode proxy frame `idx`, if it's been built yet.
    pub fn frame(&self, idx: u64) -> Option<egui::ColorImage> {
        let jpeg = self.frames.lock().ok()?.get(idx as usize)?.clone();
        let img = image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg).ok()?.to_rgba8();
        Some(egui::ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw()))
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// Split an MJPEG byte stream into individual JPEGs (each SOI 0xFFD8 … EOI 0xFFD9).
///
/// ffmpeg's mjpeg encoder byte-stuffs 0xFF inside entropy-coded data, so the first
/// EOI after a frame's start ends that frame.
fn split_jpegs(mut r: impl Read, frames: &Mutex<Vec<Vec<u8>>>, ctx: &egui::Context) {
    let mut buf: Vec<u8> = Vec::with_capacity(1 << 20);
    let mut chunk = vec![0u8; 1 << 16];
    // Bytes before `scanned` have been searched for an EOI already.
    let mut scanned = 0usize;
    let mut last_repaint = std::time::Instant::now();
    loop {
        let n = match r.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        buf.extend_from_slice(&chunk[..n]);
        // Search from one byte back so an EOI split across reads is still found;
        // never inside the 2-byte SOI at the start of the frame.
        let mut from = scanned.saturating_sub(1).max(2);
        while from + 1 < buf.len() {
            match buf[from..].windows(2).position(|w| w == [0xFF, 0xD9]) {
                Some(off) => {
                    let end = from + off + 2;
                    if let Ok(mut f) = frames.lock() {
                        f.push(buf[..end].to_vec());
                    }
                    buf.drain(..end);
                    from = 2;
                }
                None => break,
            }
        }
        scanned = buf.len();
        if last_repaint.elapsed().as_millis() > 100 {
            ctx.request_repaint();
            last_repaint = std::time::Instant::now();
        }
    }
    ctx.request_repaint();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frames split correctly no matter how reads chop the stream.
    #[test]
    fn splits_across_read_boundaries() {
        let frame = |b: u8| vec![0xFF, 0xD8, b, b, 0xFF, 0x00, b, 0xFF, 0xD9];
        let stream: Vec<u8> = (1..=5).flat_map(frame).collect();
        for chunk in 1..stream.len() {
            struct Chunked(Vec<u8>, usize);
            impl Read for Chunked {
                fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
                    let n = self.1.min(self.0.len()).min(out.len());
                    out[..n].copy_from_slice(&self.0[..n]);
                    self.0.drain(..n);
                    Ok(n)
                }
            }
            let frames = Mutex::new(Vec::new());
            split_jpegs(Chunked(stream.clone(), chunk), &frames, &egui::Context::default());
            let got = frames.into_inner().unwrap();
            assert_eq!(got, (1..=5).map(frame).collect::<Vec<_>>(), "chunk size {chunk}");
        }
    }
}
