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

    /// The source frame saved frame `k` shows, as ffmpeg's `fps` filter
    /// picks it: each source frame's time rounded to the nearest new frame's
    /// (halves up), the last of those kept: the last to start before the
    /// middle of frame `k`'s time. (Taken at the start of its time instead,
    /// 144 fps saved at 60 showed frames 0, 2, 4, 7 where ffmpeg's showed 1,
    /// 3, 5, 8; at half the rate they're the same.) At the source's own
    /// rate, each frame itself.
    pub(crate) fn shown(&self, job: &Job, k: u64) -> u64 {
        self.before(job, (k as f64 + 0.5) / job.out_fps.max(1) as f64)
    }

    /// The source frame blending's look `m` sees: the last to start before
    /// the look's time (the middle of its step at the blend rate), as
    /// ffmpeg's `fps` filter makes the blend rate's frames.
    pub(crate) fn seen(&self, job: &Job, m: i64) -> u64 {
        let rate = job.blend.map_or(job.out_fps, |b| b.0).max(1) as f64;
        self.before(job, (m as f64 + 0.5) / rate)
    }

    /// The last source frame to start before `t` seconds of the saved video
    /// (at a join, the part before's last). Before a part's start, the frame
    /// showing there; past the end, the last.
    fn before(&self, job: &Job, t: f64) -> u64 {
        let t = t.min(self.seconds);
        let mut before = 0.0;
        let (mut a, mut at) = (job.parts[0].0, job.parts[0].0);
        for &(pa, pb) in job.parts {
            let len = (pb - pa).max(0.0);
            (a, at) = (pa, pa + (t - before).max(0.0));
            if t <= before + len + 1e-10 {
                break;
            }
            before += len;
        }
        // In frames; one starting at `t`, as far as the arithmetic can tell,
        // isn't before it.
        let last = (at * job.fps - 1e-6).ceil() - 1.0;
        let first = (a * job.fps + 1e-6).floor();
        last.max(first).max(0.0) as u64
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
            None => (0..self.frames).for_each(|k| push(self.shown(job, k))),
            Some(s) => {
                let last = self.frames as i64 - 1;
                let mut m = s.first_look(0);
                while s.split(m).0 <= last {
                    push(self.seen(job, m));
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

    fn job(parts: &[(f64, f64)], fps: f64, out_fps: u32, blend: Option<(u32, BlendShape)>) -> Job<'_> {
        Job { source: Path::new(""), fps, parts, crop: None, size: (2, 2), out_fps, blend, kbps: 1, fit: false }
    }

    /// At fewer frames a second, the frames ffmpeg's exports show: read from
    /// them, made from clips whose frames carry their numbers
    /// (`scripts/frame-numbers.py`), 1 s to 9 s of each.
    #[test]
    fn frames_shown_are_ffmpegs() {
        let shown = |fps: f64, out: u32, parts: &[(f64, f64)]| {
            let j = job(parts, fps, out, None);
            let t = Timeline::new(&j);
            (0..8).map(|k| t.shown(&j, k)).collect::<Vec<_>>()
        };
        assert_eq!(shown(144.0, 60, &[(1.0, 9.0)]), [145, 147, 149, 152, 154, 157, 159, 161]);
        assert_eq!(shown(120.0, 30, &[(1.0, 9.0)]), [121, 125, 129, 133, 137, 141, 145, 149]);
        assert_eq!(shown(120.0, 60, &[(1.0, 9.0)]), [120, 122, 124, 126, 128, 130, 132, 134]);
        // 540 fps at 60 from 2 s.
        assert_eq!(shown(540.0, 60, &[(2.0, 6.0)])[..3], [1084, 1093, 1102]);
        // The source's own rate: each frame.
        assert_eq!(shown(60.0, 60, &[(1.0, 9.0)])[..4], [60, 61, 62, 63]);
    }

    /// A frame whose middle is right at a join shows the part before's last
    /// frame (the part after's first starts there, not before).
    #[test]
    fn a_join_at_a_frames_middle() {
        // 60 fps saved at 40: frame 2's middle at 2.5 / 40 = 0.0625 s, the
        // join. The part before's frames start at 0, 1/60, 2/60, 3/60.
        let parts = [(0.0, 0.0625), (10.0, 11.0)];
        let j = job(&parts, 60.0, 40, None);
        let t = Timeline::new(&j);
        assert_eq!(t.shown(&j, 2), 3);
        // Frame 3's middle, 0.025 s into the part after: its second frame.
        assert_eq!(t.shown(&j, 3), 601);
    }

    /// Blending looks: at the source's rate each sees its own frame; at a
    /// lower blend rate, the frames ffmpeg's `fps` filter makes it from
    /// (every other one from the first, at half). Looks before the start see
    /// the first frame.
    #[test]
    fn looks_are_ffmpegs() {
        let parts = [(1.0, 3.0)];
        let j = job(&parts, 540.0, 60, Some((540, BlendShape::Even)));
        let t = Timeline::new(&j);
        assert_eq!((0..4).map(|m| t.seen(&j, m)).collect::<Vec<_>>(), [540, 541, 542, 543]);
        let j = job(&parts, 240.0, 60, Some((120, BlendShape::Even)));
        let t = Timeline::new(&j);
        assert_eq!((0..4).map(|m| t.seen(&j, m)).collect::<Vec<_>>(), [240, 242, 244, 246]);
        assert_eq!(t.seen(&j, -2), 240);
    }

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
