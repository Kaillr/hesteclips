//! Light noise reduction: the sound that stays the same (a fan, hum, hiss) is
//! learned and taken out, and what comes and goes is kept: a voice, and also
//! a keyboard, which RNNoise (the strong one, [`crate::mixer::Denoise`])
//! takes out for not being a voice.
//!
//! The sound is cut into 20 ms windows, 10 ms apart, and each window's
//! pitches are turned down by how much of them is noise. A pitch's noise is
//! the quietest it has been over the last 1.5 s or so (minimum statistics): a
//! fan is never quieter than itself, while a voice or a key tap leaves gaps
//! in that time. How far a pitch is turned down follows a smoothed estimate
//! of what's left once the noise is out (the decision-directed Wiener gain),
//! so the noise doesn't break up into warbling tones, and never further than
//! [`FLOOR`]: what's left of the noise sounds like it, only quieter.
//!
//! The sound comes out a frame late, as RNNoise's does, so the two line up
//! and switching between them doesn't shift it.

use std::collections::VecDeque;
use std::sync::Arc;

use realfft::num_complex::Complex32;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};

/// Samples per step: 10 ms at 48 kHz, RNNoise's frame.
pub(crate) const FRAME: usize = 480;
/// Each window is two steps: the one before and this one.
const WINDOW: usize = 2 * FRAME;
const BINS: usize = WINDOW / 2 + 1;
/// How much of a pitch's power carries over from the step before (about
/// 30 ms): enough to steady a noise's level, little enough that it falls
/// back between key taps.
const SMOOTH: f32 = 0.7;
/// Steps per stretch whose quietest level is kept, and the stretches looked
/// back over: 6 × 250 ms.
const STRETCH: u32 = 25;
const STRETCHES: usize = 6;
/// A noise's quietest level over that time is well below its average: this
/// brings it back up. Measured on hiss (see the tests).
const BIAS: f32 = 3.0;
/// How much of the last step's cleaned level the estimate of what's left
/// keeps (the rest is this step's): high, so the noise's ups and downs don't
/// come through as warbling tones.
const CARRY: f32 = 0.98;
/// The furthest a pitch is turned down: to 15%, about −16 dB.
const FLOOR: f32 = 0.15;
/// A sound this far over the noise (in power, beyond it) is let through at
/// once: the smoothed estimate would only catch up a step later, when a key
/// tap is mostly over (taps kept 58%). A noise's own ups and downs reach it
/// at one pitch in about 3000 (e^-8), too rare to warble.
const ONSET: f32 = 8.0;

pub(crate) struct Steady {
    fft: Arc<dyn RealToComplex<f32>>,
    ifft: Arc<dyn ComplexToReal<f32>>,
    /// A sine bump (the square root of a Hann window), applied going in and
    /// coming out: overlapping halves then add up to the sound unchanged.
    window: Vec<f32>,
    /// The last frame as it came, then this one.
    input: Vec<f32>,
    /// The second half of the last window, cleaned: added to this one's first.
    tail: Vec<f32>,
    /// Per pitch: its power, smoothed.
    power: Vec<f32>,
    /// Per pitch: the quietest `power` in this stretch, and in the ones before.
    quietest: Vec<f32>,
    earlier: VecDeque<Vec<f32>>,
    steps: u32,
    /// Per pitch: its power once cleaned, last step.
    clean: Vec<f32>,
    started: bool,
    time: Vec<f32>,
    spectrum: Vec<Complex32>,
    scratch: Vec<Complex32>,
}

impl Steady {
    pub(crate) fn new() -> Self {
        let mut planner = RealFftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(WINDOW);
        let ifft = planner.plan_fft_inverse(WINDOW);
        let scratch = vec![Complex32::default(); fft.get_scratch_len().max(ifft.get_scratch_len())];
        Self {
            window: (0..WINDOW).map(|n| (std::f32::consts::PI * n as f32 / WINDOW as f32).sin()).collect(),
            input: vec![0.0; WINDOW],
            tail: vec![0.0; FRAME],
            power: vec![0.0; BINS],
            quietest: vec![f32::INFINITY; BINS],
            earlier: VecDeque::with_capacity(STRETCHES),
            steps: 0,
            clean: vec![0.0; BINS],
            started: false,
            time: fft.make_input_vec(),
            spectrum: fft.make_output_vec(),
            scratch,
            fft,
            ifft,
        }
    }

    /// One frame in (`FRAME` samples), the one before it out, cleaned.
    pub(crate) fn process(&mut self, frame: &[f32], out: &mut [f32]) {
        self.input.copy_within(FRAME.., 0);
        self.input[FRAME..].copy_from_slice(frame);
        for ((t, x), w) in self.time.iter_mut().zip(&self.input).zip(&self.window) {
            *t = x * w;
        }
        let _ = self.fft.process_with_scratch(&mut self.time, &mut self.spectrum, &mut self.scratch);
        for k in 0..BINS {
            let p = self.spectrum[k].norm_sqr();
            let s = if self.started { SMOOTH * self.power[k] + (1.0 - SMOOTH) * p } else { p };
            self.power[k] = s;
            self.quietest[k] = self.quietest[k].min(s);
            let quietest = self.earlier.iter().fold(self.quietest[k], |m, e| m.min(e[k]));
            let noise = (quietest * BIAS).max(1e-12);
            // What's left once the noise is out, over the noise: now (what
            // came less the noise) and as it was (last step's cleaned).
            let now = (p / noise - 1.0).max(0.0);
            let left = CARRY * self.clean[k] / noise + (1.0 - CARRY) * now;
            let left = if now > ONSET { left.max(now) } else { left };
            let gain = (left / (1.0 + left)).max(FLOOR);
            self.clean[k] = gain * gain * p;
            self.spectrum[k] *= gain;
        }
        self.started = true;
        // The lowest and highest pitch have no phase: zero, as the inverse wants.
        self.spectrum[0].im = 0.0;
        self.spectrum[BINS - 1].im = 0.0;
        let _ = self.ifft.process_with_scratch(&mut self.spectrum, &mut self.time, &mut self.scratch);
        let scale = 1.0 / WINDOW as f32;
        for i in 0..FRAME {
            out[i] = self.tail[i] + self.time[i] * self.window[i] * scale;
            self.tail[i] = self.time[FRAME + i] * self.window[FRAME + i] * scale;
        }
        self.steps += 1;
        if self.steps == STRETCH {
            self.steps = 0;
            let mut done = if self.earlier.len() == STRETCHES { self.earlier.pop_front().unwrap() } else { vec![0.0; BINS] };
            done.copy_from_slice(&self.quietest);
            self.earlier.push_back(done);
            self.quietest.fill(f32::INFINITY);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hiss(seed: &mut u32) -> f32 {
        *seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        (*seed >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0
    }

    fn run(input: &[f32]) -> Vec<f32> {
        let mut s = Steady::new();
        let mut out = vec![0.0; input.len()];
        for (i, o) in input.chunks_exact(FRAME).zip(out.chunks_exact_mut(FRAME)) {
            s.process(i, o);
        }
        out
    }

    fn rms(s: &[f32]) -> f32 {
        (s.iter().map(|x| x * x).sum::<f32>() / s.len() as f32).sqrt()
    }

    /// Nothing to take out: the sound as it came, a frame late.
    #[test]
    fn quiet_sound_passes_a_frame_late() {
        let mut input = vec![0.0; 48_000];
        for i in (5_000..40_000).step_by(7_000) {
            input[i] = 0.5;
        }
        let out = run(&input);
        for i in (5_000..40_000).step_by(7_000) {
            assert!((out[i + FRAME] - 0.5).abs() < 1e-3, "{i}: {}", out[i + FRAME]);
        }
    }

    /// Hiss throughout is turned down to about `FLOOR`, once it's been
    /// heard for a moment.
    #[test]
    fn hiss_goes() {
        let mut seed = 7;
        let input: Vec<f32> = (0..4 * 48_000).map(|_| hiss(&mut seed) * 0.02).collect();
        let out = run(&input);
        let last = 3 * 48_000..4 * 48_000;
        let ratio = rms(&out[last.clone()]) / rms(&input[last]);
        assert!(ratio < 0.25, "hiss kept {ratio}");
        assert!(ratio > FLOOR * 0.5, "hiss taken out further than the floor: {ratio}");
    }

    /// Key taps over a fan: 15 ms clicks, 8 a second (a stream in osu!),
    /// stay, while the fan between them goes. Not right next to a tap: the
    /// step before it is in a window with it, and the fan there stays, as it
    /// does for a few steps after while the estimate settles back (measured:
    /// over the whole gap, 41% of the fan is left). The tap hides that much
    /// of what's just before and after it.
    #[test]
    fn key_taps_stay() {
        let mut seed = 3;
        let tap = |i: usize| i % 6_000 < 720;
        let fan: Vec<f32> = (0..4 * 48_000).map(|_| hiss(&mut seed) * 0.01).collect();
        let taps: Vec<f32> = (0..4 * 48_000).map(|i| if tap(i) { hiss(&mut seed) * 0.2 * (-((i % 6_000) as f32) / 150.0).exp() } else { 0.0 }).collect();
        let input: Vec<f32> = fan.iter().zip(&taps).map(|(f, t)| f + t).collect();
        let out = run(&input);
        // The last second, each sample against its own time out (a frame late).
        let (mut tap_in, mut tap_out, mut gap_in, mut gap_out) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
        for i in 3 * 48_000..4 * 48_000 - FRAME {
            if tap(i) {
                tap_in += input[i].powi(2);
                tap_out += out[i + FRAME].powi(2);
            } else if (2_400..5_040).contains(&(i % 6_000)) {
                gap_in += input[i].powi(2);
                gap_out += out[i + FRAME].powi(2);
            }
        }
        let kept = (tap_out / tap_in).sqrt();
        assert!(kept > 0.85, "taps kept {kept}");
        let gap = (gap_out / gap_in).sqrt();
        assert!(gap < 0.25, "fan between taps kept {gap}");
    }
}
