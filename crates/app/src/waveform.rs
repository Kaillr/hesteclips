//! Audio waveforms for the timelines, drawn the way editors do: the signed
//! peaks (min/max) as a lighter outline, the RMS (how loud it actually sounds)
//! solid inside it. Heights are on a dB scale like the meters (down to
//! [`FLOOR_DB`]), so quiet tracks are still readable, and the RMS layer keeps
//! the shape of the sound visible even where the peaks are always high: a
//! shot's spike, a voice's syllables, a quiet patch.
//!
//! The track is summarised once into a pyramid of min/max/RMS buckets (16
//! samples at the finest, 4× coarser per level). Drawing picks the level that
//! fits the zoom, so every pixel column summarises exactly its own slice of
//! time: no stair steps when zoomed in, no work proportional to the clip's
//! length when zoomed out. Columns are joined into one filled mesh, so edges
//! are smooth instead of a row of 1 px bars.

use egui::{Color32, Painter, Pos2, Rect, Shape, Stroke, epaint::Mesh};

/// Bottom of the height scale, dBFS.
const FLOOR_DB: f32 = -54.0;

/// Amplitude to height (0..=1), sign kept: dB above [`FLOOR_DB`].
fn scale(v: f32) -> f32 {
    let db = 20.0 * v.abs().max(1e-9).log10();
    ((db - FLOOR_DB) / -FLOOR_DB).clamp(0.0, 1.0).copysign(v)
}

/// Samples per bucket at the finest level.
const BASE: usize = 16;
/// Each level's buckets cover this many of the level below.
const FACTOR: usize = 4;

#[derive(Clone, Copy)]
struct Bucket {
    min: f32,
    max: f32,
    /// Mean square, for RMS.
    ms: f32,
}

impl Bucket {
    const EMPTY: Bucket = Bucket { min: f32::MAX, max: f32::MIN, ms: 0.0 };

    fn merge(buckets: &[Bucket]) -> Bucket {
        let mut b = Bucket::EMPTY;
        for x in buckets {
            b.min = b.min.min(x.min);
            b.max = b.max.max(x.max);
            b.ms += x.ms;
        }
        b.ms /= buckets.len().max(1) as f32;
        b
    }
}

pub struct Waveform {
    /// Sample frames per second.
    rate: f64,
    /// `levels[0]` is the finest.
    levels: Vec<Vec<Bucket>>,
}

impl Waveform {
    /// From interleaved stereo at `rate` Hz (channels are combined: peaks from
    /// either side, loudness from both).
    pub fn new(stereo: &[f32], rate: u32) -> Self {
        let base: Vec<Bucket> = stereo
            .chunks(BASE * 2)
            .map(|c| {
                let mut b = Bucket::EMPTY;
                for s in c {
                    b.min = b.min.min(*s);
                    b.max = b.max.max(*s);
                    b.ms += s * s;
                }
                b.ms /= c.len().max(1) as f32;
                b
            })
            .collect();
        let mut levels = vec![base];
        while levels.last().is_some_and(|l| l.len() > 1) {
            let next = levels.last().unwrap().chunks(FACTOR).map(Bucket::merge).collect();
            levels.push(next);
        }
        Self { rate: rate as f64, levels }
    }

    pub fn is_empty(&self) -> bool {
        self.levels[0].is_empty()
    }

    /// Summary of `t0..t1` seconds, from the coarsest level that still has a
    /// few buckets in the range.
    fn range(&self, t0: f64, t1: f64) -> Option<Bucket> {
        let (s0, s1) = (t0.max(0.0) * self.rate, t1.max(0.0) * self.rate);
        let mut size = BASE as f64;
        let mut level = 0;
        while level + 1 < self.levels.len() && (s1 - s0) / (size * FACTOR as f64) >= 2.0 {
            size *= FACTOR as f64;
            level += 1;
        }
        let l = &self.levels[level];
        let a = (s0 / size).floor() as usize;
        let b = ((s1 / size).ceil() as usize).max(a + 1).min(l.len());
        (a < b).then(|| Bucket::merge(&l[a..b]))
    }

    /// Draw `(from, span)` seconds into `lane`, mirrored around its middle, with
    /// `gain_at(t)` applied and each column coloured by `color(peak, rms)`.
    pub fn paint(&self, p: &Painter, lane: Rect, (from, span): (f64, f64), gain_at: impl Fn(f64) -> f32, color: impl Fn(f32, f32) -> Color32) {
        if self.is_empty() || span <= 0.0 {
            return;
        }
        let mid = lane.center().y;
        let half = lane.height() / 2.0 - 2.0;
        // One column per physical pixel.
        let step = 1.0 / p.pixels_per_point();
        let cols = (lane.width() / step).ceil() as usize;
        let per_col = span / cols as f64;
        let mut peaks = Mesh::default();
        let mut body = Mesh::default();
        let mut clipped = Vec::new();
        for c in 0..=cols {
            let x = lane.left() + c as f32 * step;
            let t0 = from + c as f64 * per_col;
            let (lo, hi, rms, peak) = match self.range(t0, t0 + per_col) {
                Some(b) => {
                    let g = gain_at(t0 + per_col / 2.0);
                    let (lo, hi) = (b.min * g, b.max * g);
                    let peak = lo.abs().max(hi.abs());
                    (lo, hi, b.ms.sqrt() * g, peak)
                }
                None => (0.0, 0.0, 0.0, 0.0),
            };
            if peak >= 0.999 {
                clipped.push(x);
            }
            let col = color(peak, rms);
            // At least a hairline, so silence still reads as a track.
            let y = |v: f32| mid - scale(v) * half;
            let (top, bottom) = (y(hi.max(0.0)).min(mid - 0.5), y(lo.min(0.0)).max(mid + 0.5));
            let r = (scale(rms) * half).min(mid - top).min(bottom - mid);
            push_column(&mut peaks, x, top, bottom, col.gamma_multiply(0.55));
            push_column(&mut body, x, mid - r, mid + r, col);
        }
        p.add(Shape::mesh(peaks));
        p.add(Shape::mesh(body));
        // Clipping: a red tick at the top and bottom where it happens.
        for x in clipped {
            p.vline(x, lane.top()..=lane.top() + 3.0, Stroke::new(step.max(1.0), crate::library::REC_RED));
            p.vline(x, lane.bottom() - 3.0..=lane.bottom(), Stroke::new(step.max(1.0), crate::library::REC_RED));
        }
    }
}

/// Add a column (top and bottom vertex) and join it to the previous one.
fn push_column(mesh: &mut Mesh, x: f32, top: f32, bottom: f32, color: Color32) {
    let i = mesh.vertices.len() as u32;
    mesh.colored_vertex(Pos2::new(x, top), color);
    mesh.colored_vertex(Pos2::new(x, bottom), color);
    if i >= 2 {
        mesh.add_triangle(i - 2, i - 1, i);
        mesh.add_triangle(i - 1, i + 1, i);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_summarise_exactly() {
        // 1 s at 1 kHz: silence, then a 0.5 tone burst between 0.5 and 0.6 s.
        let rate = 1000;
        let mut stereo = vec![0.0f32; rate * 2];
        for i in 500..600 {
            let v = if i % 2 == 0 { 0.5 } else { -0.5 };
            stereo[i * 2] = v;
            stereo[i * 2 + 1] = v;
        }
        let w = Waveform::new(&stereo, rate as u32);
        let quiet = w.range(0.0, 0.4).unwrap();
        assert_eq!((quiet.min, quiet.max), (0.0, 0.0));
        let loud = w.range(0.52, 0.58).unwrap();
        assert_eq!((loud.min, loud.max), (-0.5, 0.5));
        assert!((loud.ms.sqrt() - 0.5).abs() < 1e-4);
        // The whole second, from a coarse level, still sees the burst.
        let all = w.range(0.0, 1.0).unwrap();
        assert_eq!(all.max, 0.5);
    }
}
