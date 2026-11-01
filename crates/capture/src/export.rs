//! Saving an edited clip's picture with the graphics card, for `media`'s
//! export where the picture is made again: on Linux (`linux::export`) and
//! macOS (`mac::export`); elsewhere, for now, ffmpeg makes it.

use std::path::Path;

use anyhow::Result;

use crate::BlendShape;
use crate::shutter::Shutter;

/// What to make.
pub struct Job<'a> {
    pub source: &'a Path,
    /// The source's frame rate, which its frames are numbered on.
    pub fps: f64,
    /// The parts kept, in the source's seconds, in order.
    pub parts: &'a [(f64, f64)],
    /// The part of the picture kept: left, top, width, height in its pixels.
    pub crop: Option<[u32; 4]>,
    /// The frames' size (even) and rate.
    pub size: (u32, u32),
    pub out_fps: u32,
    /// Frame blending: the rate looked at (a multiple of `out_fps`), and how
    /// the looks in a frame are weighed.
    pub blend: Option<(u32, BlendShape)>,
    pub kbps: u32,
    /// To fit a size: held to `kbps` over short spans (Linux: by x264).
    pub fit: bool,
}

/// Make `job`'s picture (no sound) at `dest`, an MP4. `Ok(false)` where it
/// can't be made here: ffmpeg makes it then.
pub fn render_video(job: &Job, dest: &Path, progress: &dyn Fn(f32)) -> Result<bool> {
    #[cfg(target_os = "linux")]
    {
        crate::linux::export::render_video(job, dest, progress)
    }
    #[cfg(target_os = "macos")]
    {
        crate::mac::export::render_video(job, dest, progress)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (job, dest, progress);
        Ok(false)
    }
}

/// The saved video's time: the parts back to back. Which source frame each
/// saved frame shows (and, blending, each look), as ffmpeg's `fps` and
/// blending filters pick them; nothing in it is any platform's.
#[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
pub(crate) struct Timeline {
    /// Its length in seconds, and how many frames that is.
    seconds: f64,
    pub(crate) frames: u64,
}

#[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
impl Timeline {
    pub(crate) fn new(job: &Job) -> Self {
        let seconds: f64 = job.parts.iter().map(|(a, b)| (b - a).max(0.0)).sum();
        // The frames starting before the end (ffmpeg's count, too).
        let frames = (seconds * job.out_fps as f64 - 1e-9).ceil().max(0.0) as u64;
        Self { seconds, frames }
    }

    /// Frame `k`'s time in the saved video.
    pub(crate) fn frame_time(&self, k: u64, fps: u32) -> f64 {
        k as f64 / fps as f64
    }

    /// The source frame showing at `t` seconds of the saved video (before
    /// its start, the first; past its end, the last).
    pub(crate) fn source_frame(&self, job: &Job, t: f64) -> u64 {
        let t = t.clamp(0.0, (self.seconds - 1e-6).max(0.0));
        let mut before = 0.0;
        let mut at = job.parts[job.parts.len() - 1].1;
        for &(a, b) in job.parts {
            let len = (b - a).max(0.0);
            if t < before + len {
                at = a + (t - before);
                break;
            }
            before += len;
        }
        (at * job.fps + 1e-6).floor().max(0.0) as u64
    }

    /// The source frames wanted, in the order they're used, each once.
    pub(crate) fn source_frames(&self, job: &Job, shutter: Option<&Shutter>) -> Vec<u64> {
        let mut out: Vec<u64> = Vec::new();
        let mut push = |i: u64| {
            if out.last() != Some(&i) {
                out.push(i);
            }
        };
        match shutter {
            None => (0..self.frames).for_each(|k| push(self.source_frame(job, self.frame_time(k, job.out_fps)))),
            Some(s) => {
                let rate = job.blend.map_or(1, |b| b.0) as f64;
                let last = self.frames as i64 - 1;
                let mut m = s.first_look(0);
                while s.split(m).0 <= last {
                    push(self.source_frame(job, (m as f64 + 0.5) / rate));
                    m += 1;
                }
            }
        }
        out
    }
}

/// Lanczos (3 lobes) for `len` samples from `at` made `out`: each output
/// sample's first source sample and `taps` weights (adding up to 1, the
/// kernel widened when shrinking, so nothing aliases). Samples are centred:
/// the same size is the samples themselves. How exports scale, as ffmpeg's
/// did (`scale=…:flags=lanczos`): sharper than averaging the area each pixel
/// covers, which kept 8% less of a detailed picture's finest detail.
#[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
pub(crate) fn lanczos(at: u32, len: u32, out: u32) -> (Vec<i32>, Vec<f32>, u32) {
    let kernel = |x: f64| {
        if x.abs() < 1e-9 {
            1.0
        } else if x.abs() >= 3.0 {
            0.0
        } else {
            let px = std::f64::consts::PI * x;
            3.0 * px.sin() * (px / 3.0).sin() / (px * px)
        }
    };
    let scale = len as f64 / out.max(1) as f64;
    let widen = scale.max(1.0);
    let support = 3.0 * widen;
    let taps = (2.0 * support).ceil() as u32 + 1;
    let (mut firsts, mut weights) = (Vec::with_capacity(out as usize), Vec::with_capacity((out * taps) as usize));
    for i in 0..out {
        let centre = at as f64 + (i as f64 + 0.5) * scale - 0.5;
        let first = (centre - support).floor() as i32 + 1;
        let w: Vec<f64> = (0..taps).map(|t| kernel(((first + t as i32) as f64 - centre) / widen)).collect();
        let total: f64 = w.iter().sum();
        firsts.push(first);
        weights.extend(w.iter().map(|w| (w / total) as f32));
    }
    (firsts, weights, taps)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each output sample's weights add up to one, shrinking or growing.
    #[test]
    fn lanczos_weights_add_up() {
        for (len, out) in [(2560, 1280), (1920, 1280), (1440, 720), (720, 1080), (100, 99)] {
            let (firsts, weights, taps) = lanczos(0, len, out);
            assert_eq!(firsts.len(), out as usize);
            for w in weights.chunks(taps as usize) {
                assert!((w.iter().sum::<f32>() - 1.0).abs() < 1e-5, "{len} to {out}: {}", w.iter().sum::<f32>());
            }
        }
    }

    /// The same size takes each sample as it is (from where it starts: a
    /// crop's samples are its own).
    #[test]
    fn lanczos_same_size_is_the_samples() {
        let (firsts, weights, taps) = lanczos(320, 1920, 1920);
        for (i, (first, w)) in firsts.iter().zip(weights.chunks(taps as usize)).enumerate() {
            let (t, most) = w.iter().enumerate().fold((0, 0.0f32), |m, (t, &v)| if v > m.1 { (t, v) } else { m });
            assert_eq!(*first + t as i32, 320 + i as i32);
            assert!((most - 1.0).abs() < 1e-6 && w.iter().map(|v| v.abs()).sum::<f32>() < 1.0 + 1e-5);
        }
    }
}
