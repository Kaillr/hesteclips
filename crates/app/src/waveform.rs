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
    /// Samples not yet a whole finest bucket (while it's still being fed).
    pending: Vec<f32>,
}

impl Waveform {
    /// From interleaved stereo at `rate` Hz (channels are combined: peaks from
    /// either side, loudness from both).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn new(stereo: &[f32], rate: u32) -> Self {
        let mut w = Self::empty(rate);
        w.extend(stereo);
        w.finish();
        w
    }

    /// Nothing yet: fed with [`Self::extend`] as the sound is decoded.
    pub fn empty(rate: u32) -> Self {
        Self { rate: rate as f64, levels: vec![Vec::new()], pending: Vec::new() }
    }

    /// The next samples (interleaved stereo, after what came before). Only the
    /// newest bucket of each level is redone, so feeding it as it decodes
    /// costs the same as building it at once.
    pub fn extend(&mut self, stereo: &[f32]) {
        self.pending.extend_from_slice(stereo);
        let whole = self.pending.len() / (BASE * 2) * (BASE * 2);
        if whole == 0 {
            return;
        }
        let new = media::sample_peaks(&self.pending[..whole], BASE * 2).into_iter().map(|[min, max, ms]| Bucket { min, max, ms });
        self.levels[0].extend(new);
        self.pending.drain(..whole);
        self.rebuild_tail();
    }

    /// The end of the sound: its last, partial bucket too.
    pub fn finish(&mut self) {
        if !self.pending.is_empty() {
            let rest = std::mem::take(&mut self.pending);
            let new = media::sample_peaks(&rest, BASE * 2).into_iter().map(|[min, max, ms]| Bucket { min, max, ms });
            self.levels[0].extend(new);
            self.rebuild_tail();
        }
    }

    /// Bring the coarser levels up to date with the finest: each level's last
    /// bucket (it may have been partial) and anything after it is redone.
    fn rebuild_tail(&mut self) {
        let mut l = 0;
        while self.levels[l].len() > 1 {
            if self.levels.len() == l + 1 {
                self.levels.push(Vec::new());
            }
            let keep = self.levels[l + 1].len().saturating_sub(1);
            let new: Vec<Bucket> = self.levels[l][keep * FACTOR..].chunks(FACTOR).map(Bucket::merge).collect();
            self.levels[l + 1].truncate(keep);
            self.levels[l + 1].extend(new);
            l += 1;
        }
        self.levels.truncate(l + 1);
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
            push_column(&mut peaks, x, top, bottom, col.gamma_multiply(0.55), step);
            push_column(&mut body, x, mid - r, mid + r, col, step);
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
/// Add a column and join it to the previous one: solid from `top` to
/// `bottom`, fading out over `feather` beyond each (anti-aliased edges, so a
/// column's height shows to a fraction of a pixel).
fn push_column(mesh: &mut Mesh, x: f32, top: f32, bottom: f32, color: Color32, feather: f32) {
    let i = mesh.vertices.len() as u32;
    mesh.colored_vertex(Pos2::new(x, top - feather), Color32::TRANSPARENT);
    mesh.colored_vertex(Pos2::new(x, top), color);
    mesh.colored_vertex(Pos2::new(x, bottom), color);
    mesh.colored_vertex(Pos2::new(x, bottom + feather), Color32::TRANSPARENT);
    if i >= 4 {
        // Three bands (fade, solid, fade) between this column and the last.
        for k in 0..3 {
            let (a, b) = (i - 4 + k, i + k);
            mesh.add_triangle(a, a + 1, b);
            mesh.add_triangle(a + 1, b + 1, b);
        }
    }
}

/// A waveform drawn as its sound is decoded: filled in by a thread that
/// follows the decoder, so the graph grows with the sound instead of
/// appearing all at once after it.
pub struct Live {
    wave: std::sync::Mutex<Waveform>,
    done: std::sync::atomic::AtomicBool,
}

impl Live {
    /// Paint what's there (see [`Waveform::paint`]).
    pub fn paint(&self, p: &Painter, lane: Rect, view: (f64, f64), gain_at: impl Fn(f64) -> f32, color: impl Fn(f32, f32) -> Color32) {
        if let Ok(w) = self.wave.lock() {
            w.paint(p, lane, view, gain_at, color);
        }
    }

    /// Fully decoded, and there's no sound at all.
    pub fn is_silent_file(&self) -> bool {
        self.done.load(std::sync::atomic::Ordering::Acquire) && self.wave.lock().is_ok_and(|w| w.is_empty())
    }
}

/// Follow track `track` of `pcm` as it's decoded.
pub fn follow(pcm: std::sync::Arc<media::pcm::Pcm>, track: usize, ctx: &egui::Context) -> std::sync::Arc<Live> {
    use std::sync::atomic::Ordering;
    let live = std::sync::Arc::new(Live { wave: std::sync::Mutex::new(Waveform::empty(media::PREVIEW_RATE)), done: Default::default() });
    let (out, ctx) = (live.clone(), ctx.clone());
    std::thread::Builder::new()
        .name("waveform".into())
        .spawn(move || {
            let mut read = 0;
            loop {
                // Done first: everything decoded by then is in `len`.
                let done = pcm.is_done();
                let Some(t) = pcm.tracks().get(track) else { break };
                let len = t.len();
                if len > read {
                    let chunk = t.copy(read, len);
                    read = len;
                    if let Ok(mut w) = out.wave.lock() {
                        w.extend(&chunk);
                    }
                    ctx.request_repaint();
                }
                if done {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(60));
            }
            if let Ok(mut w) = out.wave.lock() {
                w.finish();
            }
            out.done.store(true, Ordering::Release);
            ctx.request_repaint();
        })
        .expect("spawn waveform thread");
    live
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fed_in_pieces_is_the_same() {
        let stereo: Vec<f32> = (0..2 * 48_000 * 3 + 37).map(|i| ((i as f32) * 0.013).sin() * (i % 7) as f32 / 7.0).collect();
        let whole = Waveform::new(&stereo, 48_000);
        let mut fed = Waveform::empty(48_000);
        // Odd-sized pieces, like the decoder's.
        for piece in stereo.chunks(4097) {
            fed.extend(piece);
        }
        fed.finish();
        assert_eq!(whole.levels.len(), fed.levels.len());
        for (a, b) in whole.levels.iter().zip(&fed.levels) {
            assert_eq!(a.len(), b.len());
            for (x, y) in a.iter().zip(b) {
                assert!((x.min - y.min).abs() < 1e-6 && (x.max - y.max).abs() < 1e-6 && (x.ms - y.ms).abs() < 1e-6);
            }
        }
    }

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
