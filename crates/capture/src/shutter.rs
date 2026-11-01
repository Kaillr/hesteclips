//! Which frames a frame-blending look counts toward, and how much: the same
//! on Windows, macOS and Linux.
//!
//! Look `m` is at `m + 0.5` steps (a step is one over the blend rate), and
//! frame `k`'s own time is steps `k * looks` to `(k + 1) * looks`.

/// How a blended frame weighs the looks in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BlendShape {
    /// The looks in the frame's own time, each the same (a shutter open for
    /// the whole frame): the frame shows just what was on screen then, a
    /// moving thing as a trail from where it was at the frame's start to
    /// where it was at its end, and the next frame's trail starts there. The
    /// trail's ends are sharp, and when the eye follows the motion they're
    /// seen jumping from frame to frame.
    #[default]
    Even,
    /// The looks from the middle of the frame before to the middle of the
    /// one after, by cos² of how far each is from the frame's middle (a Hann
    /// window two frames wide): a look counts toward the two frames whose
    /// middles it's between, more toward the nearer, and its two weights add
    /// up to one. A trail is as long as Even's at half its brightness, but
    /// fades out at its ends into the frames on either side, so frames hand
    /// over smoothly. Motion quicker than the frame rate can show (a cursor
    /// circling fast) comes through as steps far less: what changes faster
    /// than the frame rate gets through Even at up to 22%, here under 3%.
    Soft,
}

/// Where the looks go, for `looks` a frame.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Shutter {
    looks: i64,
    soft: bool,
}

impl Shutter {
    pub(crate) fn new(looks: u32, shape: BlendShape) -> Self {
        // One look a frame is no blending: each frame is the picture up at
        // its time.
        Self { looks: looks.max(1) as i64, soft: shape == BlendShape::Soft && looks > 1 }
    }

    /// Two frames' worth of looks at once (soft), or one.
    pub(crate) fn frames_open(&self) -> usize {
        if self.soft { 2 } else { 1 }
    }

    /// The first frame look `m` counts toward, and its weight in that frame
    /// and in the one after.
    pub(crate) fn split(&self, m: i64) -> (i64, [f32; 2]) {
        if !self.soft {
            return (m.div_euclid(self.looks), [1.0, 0.0]);
        }
        // Twice how far the look is past frame 0's middle, in steps: frame
        // `k`'s middle is at `(k + 0.5) * looks`.
        let twice = 2 * m + 1 - self.looks;
        let k = twice.div_euclid(2 * self.looks);
        // How far past frame `k`'s middle, in frames (0 to 1).
        let past = twice.rem_euclid(2 * self.looks) as f64 / (2 * self.looks) as f64;
        let near = (std::f64::consts::FRAC_PI_2 * past).cos().powi(2) as f32;
        (k, [near, 1.0 - near])
    }

    /// The first look that counts toward frame `k`.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub(crate) fn first_look(&self, k: i64) -> i64 {
        self.start(if self.soft { k - 1 } else { k })
    }

    /// The last look that counts toward frame `k`: once it's taken, the
    /// frame is done.
    pub(crate) fn last_look(&self, k: i64) -> i64 {
        self.start(k + 1) - 1
    }

    /// The first look whose first frame is `k` or later.
    fn start(&self, k: i64) -> i64 {
        k * self.looks + if self.soft { self.looks / 2 } else { 0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each frame's weight in each look, over looks `from..to`.
    fn weights(s: &Shutter, k: i64, from: i64, to: i64) -> Vec<(i64, f32)> {
        (from..to)
            .filter_map(|m| {
                let (first, w) = s.split(m);
                match k - first {
                    0 => Some((m, w[0])),
                    1 => Some((m, w[1])),
                    _ => None,
                }
            })
            .filter(|(_, w)| *w > 0.0)
            .collect()
    }

    #[test]
    fn even_is_each_frames_own_looks() {
        let s = Shutter::new(4, BlendShape::Even);
        assert_eq!(weights(&s, 2, 0, 40), (8..12).map(|m| (m, 1.0)).collect::<Vec<_>>());
        assert_eq!((s.first_look(2), s.last_look(2)), (8, 11));
    }

    /// One look a frame isn't blended, soft or not.
    #[test]
    fn one_look_is_even() {
        let s = Shutter::new(1, BlendShape::Soft);
        assert_eq!(s.split(5), (5, [1.0, 0.0]));
        assert_eq!(s.frames_open(), 1);
    }

    /// Every look counts in all as much as in Even; each frame's looks add
    /// up to as much as Even's, centred where Even's are (so the picture
    /// stays in time with the sound); and they lie within the frame's first
    /// and last look.
    #[test]
    fn soft_weighs_every_moment_the_same() {
        for looks in [2, 3, 4, 5, 8, 16, 32] {
            let s = Shutter::new(looks, BlendShape::Soft);
            for m in -40..200 {
                let (_, w) = s.split(m);
                assert!((w[0] + w[1] - 1.0).abs() < 1e-6, "{looks} looks, look {m}: {w:?}");
            }
            for k in 0..4 {
                let w = weights(&s, k, -200, 400);
                let sum: f32 = w.iter().map(|(_, w)| w).sum();
                assert!((sum - looks as f32).abs() < 1e-4, "{looks} looks, frame {k}: weighs {sum}");
                let centre = w.iter().map(|(m, w)| (*m as f32 + 0.5) * w).sum::<f32>() / sum;
                let even = (k as f32 + 0.5) * looks as f32;
                assert!((centre - even).abs() < 1e-3, "{looks} looks, frame {k}: centred at {centre}, not {even}");
                let (first, last) = (w.first().unwrap().0, w.last().unwrap().0);
                assert!(first >= s.first_look(k) && last <= s.last_look(k), "{looks} looks, frame {k}: looks {first}..={last}, not within {}..={}", s.first_look(k), s.last_look(k));
                assert_eq!(s.last_look(k) - s.first_look(k) + 1, 2 * looks as i64);
            }
        }
    }

    /// Strongest in the middle of the frame's own time, fading toward the
    /// middles of the frames either side.
    #[test]
    fn soft_fades_out_from_the_middle() {
        let s = Shutter::new(4, BlendShape::Soft);
        let w: Vec<f32> = weights(&s, 3, 0, 40).into_iter().map(|(_, w)| w).collect();
        assert_eq!(w.len(), 8);
        for i in 0..4 {
            assert!(w[i] < w[i + 1] || i == 3, "{w:?}");
            assert!((w[i] - w[7 - i]).abs() < 1e-6, "{w:?}");
        }
        assert!(w[0] < 0.05 && w[3] > 0.95, "{w:?}");
    }
}
