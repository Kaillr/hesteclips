//! Which frames a frame-blending look counts toward, and how much, and where
//! the looks sit against the pictures they take: the same on Windows, macOS
//! and Linux.
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// How much of the way the looks move toward halfway between the pictures
/// (the game's, or the screen's), on each picture. A guess: slow enough that
/// the noise in pictures' times averages out (a few hundred milliseconds),
/// quick enough to follow a game or a screen that drifts.
const PHASE_GAIN: f64 = 0.03;
/// How far the looks may move from where the frames' own times put them
/// (in steps) before they go a whole step back the other way: as far is the
/// same lock, one look over (a look repeated, or one passed over: a game a
/// little off the blend rate gets a picture seen twice, or one missed, now
/// and then, as it must). Past half a step each way, with some way to go
/// before going back: a lock that sits right at half a step (a game at three
/// pictures a frame, its pictures where they fell) went back and forth over
/// it with every other picture, measured on Windows 144 times in under a
/// second, each time a frame repeated or passed over. Now it has a quarter
/// step either side of it before it goes back. The cost: a frame's picture
/// may be up to three quarters of a step off its time instead of half (4 ms
/// more at 60 fps).
const PHASE_LIMIT: f64 = 0.75;

/// Where the looks sit (a pacer's `phase`: look `m` is at `t0 + phase +
/// (m + 0.5) * step`) once a picture has come up `since` seconds after the
/// looks' start, steps `step` seconds apart: moved a little of the way
/// ([`PHASE_GAIN`]) toward having the pictures on whole steps, the looks
/// halfway between two. The `first` picture the capture timed puts them
/// there at once (started anywhere, they could take seconds to get there, a
/// frame repeated or passed over every few meanwhile: measured on Windows, a
/// 240 fps game recorded at 240 did so for its first 4.5 s). A whole step
/// back the other way past [`PHASE_LIMIT`].
pub(crate) fn follow_picture(phase: f64, step: f64, since: f64, first: bool) -> f64 {
    if first {
        let off = since.rem_euclid(step);
        return if off > step / 2.0 { off - step } else { off };
    }
    let steps = (since - phase) / step;
    let phase = phase + (steps - steps.round()) * step * PHASE_GAIN;
    if phase > step * PHASE_LIMIT {
        phase - step
    } else if phase < -step * PHASE_LIMIT {
        phase + step
    } else {
        phase
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

    /// Numbers from -1 to 1 that look random, the same each run.
    fn jitter(seed: &mut u64) -> f64 {
        *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (*seed >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0
    }

    /// The looks' phase over a game's pictures, each `apart` seconds after the
    /// one before plus up to `noise` either way, starting `offset` into the
    /// first step, with looks `step` apart: how many times it went a whole
    /// step back (a look repeated or passed over), and which way each went.
    fn wraps(apart: f64, noise: f64, offset: f64, step: f64, pictures: usize, follow: impl Fn(f64, f64, f64, bool) -> f64) -> Vec<i32> {
        let mut seed = 7;
        let mut phase = 0.0;
        let mut out = Vec::new();
        for n in 0..pictures {
            let since = offset + n as f64 * apart + noise * jitter(&mut seed);
            let next = follow(phase, step, since, n == 0);
            if n > 0 && (next - phase).abs() > step / 2.0 {
                out.push(if next < phase { -1 } else { 1 });
            }
            phase = next;
        }
        out
    }

    /// A game at exactly three pictures a frame, its pictures falling where
    /// the lock is half a step off (as one measured did): it holds, where
    /// going back at half a step went back and forth all the time.
    #[test]
    fn a_lock_at_half_a_step_holds() {
        let step = 1.0 / 60.0;
        // Twenty seconds at 180 pictures a second, a third of a millisecond
        // of noise in their times.
        let (apart, noise, pictures) = (step / 3.0, 0.0003, 3600);
        // The rule this replaced: back at half a step.
        let old = |phase: f64, step: f64, since: f64, _first: bool| {
            let steps = (since - phase) / step;
            let phase = phase + (steps - steps.round()) * step * PHASE_GAIN;
            if phase > step / 2.0 {
                phase - step
            } else if phase < -step / 2.0 {
                phase + step
            } else {
                phase
            }
        };
        // The first picture sets the lock exactly half a step off; the old
        // rule is shown it there too.
        let old_from_half = |phase: f64, step: f64, since: f64, first: bool| old(if first { step / 2.0 } else { phase }, step, since, first);
        let before = wraps(apart, noise, step / 2.0, step, pictures, old_from_half);
        assert!(before.len() > 20, "the old rule should go back and forth here: {}", before.len());
        let now = wraps(apart, noise, step / 2.0, step, pictures, follow_picture);
        assert!(now.is_empty(), "went back {} times", now.len());
    }

    /// A game a little off the frame rate (three pictures a frame, a tenth
    /// of a percent faster): its pictures drift past the looks, a step every
    /// 3000 pictures, and the looks go a step back once for each, the same
    /// way every time, never back and forth.
    #[test]
    fn a_drifting_game_goes_back_once_a_step() {
        let step = 1.0 / 60.0;
        let apart = step / 3.0 * (1.0 - 1e-3);
        let got = wraps(apart, 0.0003, 0.003, step, 12_000, follow_picture);
        assert!((3..=5).contains(&got.len()), "{} times: {got:?}", got.len());
        assert!(got.windows(2).all(|w| w[0] == w[1]), "both ways: {got:?}");
    }

    /// The first picture puts the looks halfway between pictures at once.
    #[test]
    fn the_first_picture_sets_the_lock() {
        let step = 1.0 / 240.0;
        for since in [0.0, 0.001, 0.0039, 12.3456, 600.0002] {
            let phase = follow_picture(0.0, step, since, true);
            let steps = (since - phase) / step;
            assert!((steps - steps.round()).abs() < 1e-6, "{since}: off by {} steps", steps - steps.round());
            assert!(phase.abs() <= step / 2.0 + 1e-12, "{since}: phase {phase}");
        }
    }
}
