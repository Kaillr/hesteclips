//! The timeline's filmstrip, shared by the editor and the viewer.
//!
//! Every picture shows the frame in the middle of the stretch it covers, so
//! the middle of a thumbnail is exactly where its frame is: put the playhead
//! there and the preview shows the same picture. Its full height always
//! shows (only the sides are trimmed, by at most a fifth).
//!
//! On Windows the pictures are exact frames from the GPU's video decoder, in
//! stretches of a fixed number of frames laid end to end from the clip's
//! start (the last one to its end), as many frames as fit a thumbnail at
//! this zoom: they stay put while panning. A picture not decoded yet shows
//! the nearest one that is, then its own (once, a few ms later).
//!
//! Elsewhere (or if the decoder can't open the clip) the pictures are the
//! clip's keyframes, from `media::keyframe_strip`: the ones to show are
//! decided before any is decoded, so each appears once.

use std::path::Path;
use std::sync::mpsc::{self, Receiver};

use egui::{Color32, Pos2, Rect, Vec2};
use media::ClipInfo;

/// Thumbnail height in pixels (lanes are ~56 pt; 2× for high-DPI screens).
const HEIGHT: u32 = 112;
/// At most this many keyframe thumbnails, however long the clip.
const MAX_THUMBS: usize = 160;

pub enum Filmstrip {
    #[cfg(windows)]
    Exact(exact::Exact),
    Keys(Keys),
}

impl Filmstrip {
    pub fn build(ctx: &egui::Context, source: &Path, info: &ClipInfo) -> Self {
        #[cfg(windows)]
        if std::env::var_os("HESTECLIPS_NO_HW_DECODE").is_none() {
            return Self::Exact(exact::Exact::open(ctx, source, info));
        }
        let _ = info;
        Self::Keys(Keys::build(ctx, source))
    }

    /// Paint into `lane`, which shows `from..to` seconds of a clip `dur` long.
    pub fn paint(&mut self, ui: &egui::Ui, lane: Rect, from: f64, to: f64, dur: f64) {
        // The decoder couldn't open the clip: keyframes instead.
        #[cfg(windows)]
        if let Self::Exact(e) = self {
            if e.failed() {
                *self = Self::Keys(Keys::build(ui.ctx(), &e.source));
            }
        }
        match self {
            #[cfg(windows)]
            Self::Exact(e) => e.paint(ui, lane, from, to, dur),
            Self::Keys(k) => k.paint(ui, lane, from, to, dur),
        }
    }

    /// The decoded thumbnail nearest `t`, if any (a rough hover preview).
    pub fn near(&self, t: f64) -> Option<&egui::TextureHandle> {
        match self {
            #[cfg(windows)]
            Self::Exact(e) => e.near(t),
            Self::Keys(k) => k.near(t),
        }
    }
}

/// Paint pictures over their stretches of time (`start..end`, which have
/// the picture's frame in the middle), center-cropped to fill them.
/// The whole picture's height always shows: a stretch narrower than the
/// picture shows its middle (the sides trimmed), a wider one shows it whole,
/// centered.
fn paint_cells(ui: &egui::Ui, lane: Rect, from: f64, to: f64, cells: &[(f64, f64, Option<&egui::TextureHandle>)]) {
    let span = (to - from).max(1e-6);
    let x_of = |t: f64| lane.left() + ((t - from) / span) as f32 * lane.width();
    let painter = ui.painter_at(lane);
    for &(start, end, tex) in cells {
        let Some(tex) = tex else { continue };
        let cell = Rect::from_x_y_ranges(x_of(start)..=x_of(end), lane.y_range());
        if cell.right() < lane.left() || cell.left() > lane.right() || cell.width() < 0.5 {
            continue;
        }
        let size = tex.size_vec2();
        let natural = lane.height() * size.x / size.y.max(1.0);
        if cell.width() > natural {
            let rect = Rect::from_center_size(cell.center(), egui::vec2(natural, lane.height()));
            painter.image(tex.id(), rect, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
        } else {
            painter.image(tex.id(), cell, crop_uv(size, cell.size()), Color32::WHITE);
        }
    }
}

/// Exact frames on a zoom-dependent grid, from the GPU's video decoder.
#[cfg(windows)]
mod exact {
    use std::collections::BTreeMap;
    use std::sync::mpsc::{Sender, TryRecvError};

    use super::*;
    use capture::win::decode::{Decoder, Picture};

    /// Thumbnails kept, at ~90 KB each.
    const CACHE: usize = 800;

    pub struct Exact {
        pub source: std::path::PathBuf,
        /// The decoder couldn't open the clip.
        failed: std::sync::Arc<std::sync::atomic::AtomicBool>,
        /// Frame numbers wanted next (newest list wins).
        want_tx: Sender<Vec<u64>>,
        got_rx: Receiver<(u64, Picture)>,
        /// Decoded thumbnails by frame number, with when each was last drawn.
        thumbs: BTreeMap<u64, (egui::TextureHandle, u64)>,
        /// The last list asked for: asked again only when what's missing changes.
        last_asked: Vec<u64>,
        fps: f64,
        /// The last frame's number.
        last_frame: u64,
        aspect: f32,
        /// Paint count, for least-recently-drawn eviction.
        tick: u64,
    }

    impl Exact {
        pub fn open(ctx: &egui::Context, source: &Path, info: &ClipInfo) -> Self {
            let aspect = info.width.max(1) as f32 / info.height.max(1) as f32;
            let width = (((HEIGHT as f32 * aspect) / 2.0).round() as u32 * 2).max(2);
            let (want_tx, want_rx) = mpsc::channel::<Vec<u64>>();
            let (got_tx, got_rx) = mpsc::channel();
            let repaint = ctx.clone();
            let failed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let (path, fps, failed_t) = (source.to_path_buf(), info.fps, failed.clone());
            std::thread::spawn(move || {
                lower_priority();
                // Opened here, not on the UI thread: it takes a few hundred ms.
                let mut dec = match Decoder::open(&path, width, None) {
                    Ok(d) => d,
                    Err(e) => {
                        eprintln!("filmstrip: hardware decoder unavailable, using keyframes: {e:#}");
                        failed_t.store(true, std::sync::atomic::Ordering::Relaxed);
                        repaint.request_repaint();
                        return;
                    }
                };
                dec.set_fps(fps);
                // Wait for a list; work through it, switching to a newer one
                // the moment it arrives.
                let Ok(mut list) = want_rx.recv() else { return };
                loop {
                    let mut newer = None;
                    for &i in &list {
                        match want_rx.try_recv() {
                            Ok(l) => {
                                newer = Some(l);
                                break;
                            }
                            Err(TryRecvError::Disconnected) => return,
                            Err(TryRecvError::Empty) => {}
                        }
                        let got = dec.frame(i);
                        match got {
                            Ok(Some(p)) => {
                                if std::env::var_os("HESTECLIPS_DEBUG_VIDEO").is_some() {
                                    eprintln!("{:>8.3} filmstrip: frame {i}", crate::player::uptime());
                                }
                                if got_tx.send((i, p)).is_err() {
                                    return;
                                }
                                repaint.request_repaint();
                            }
                            Ok(None) => {}
                            Err(e) => {
                                eprintln!("filmstrip: {e:#}");
                                return;
                            }
                        }
                    }
                    list = match newer {
                        Some(l) => l,
                        None => match want_rx.recv() {
                            Ok(l) => l,
                            Err(_) => return,
                        },
                    };
                }
            });
            let last_frame = ((info.duration * info.fps).floor() as u64).saturating_sub(1);
            Self { source: source.to_path_buf(), failed, want_tx, got_rx, thumbs: BTreeMap::new(), last_asked: Vec::new(), fps: info.fps.max(1.0), last_frame, aspect, tick: 0 }
        }

        pub fn failed(&self) -> bool {
            self.failed.load(std::sync::atomic::Ordering::Relaxed)
        }

        pub fn paint(&mut self, ui: &egui::Ui, lane: Rect, from: f64, to: f64, dur: f64) {
            self.receive(ui.ctx());
            self.tick += 1;
            let span = (to - from).max(1e-6);
            // Stretches of `step` frames laid end to end from the start, each
            // showing the frame in its middle; the last runs to the clip's end.
            // The step is the widest of 1, 2, 3, 4, 5, 6, 7, 8, 10, 12… frames
            // not wider than a thumbnail, so only the sides are ever trimmed.
            let thumb = (lane.height() * self.aspect) as f64 / lane.width().max(1.0) as f64 * span * self.fps;
            let step = grid_step(thumb);
            let frames = self.last_frame + 1;
            let first = ((from * self.fps) as u64 / step).saturating_sub(1);
            let last = ((to * self.fps) as u64 / step + 1).min(self.last_frame / step);
            let mut cells = Vec::new();
            let mut missing = Vec::new();
            for k in first..=last {
                let (a, b) = (k * step, ((k + 1) * step).min(frames));
                let i = (a + (b - a) / 2).min(self.last_frame);
                if !self.thumbs.contains_key(&i) {
                    missing.push(i);
                }
                cells.push((a, b, i));
            }
            // Always exactly what's missing now: the decoder drops a list the
            // moment a newer one comes (zoomed or panned away).
            if missing != self.last_asked {
                self.last_asked = missing.clone();
                let _ = self.want_tx.send(missing);
            }
            let tick = self.tick;
            for (_, _, i) in &cells {
                if let Some(entry) = self.thumbs.get_mut(i) {
                    entry.1 = tick;
                }
            }
            let drawn: Vec<_> = cells
                .iter()
                .map(|&(a, b, i)| {
                    let end = if b >= frames { dur } else { b as f64 / self.fps };
                    (a as f64 / self.fps, end, self.nearest(i))
                })
                .collect();
            paint_cells(ui, lane, from, to, &drawn);
        }

        pub fn near(&self, t: f64) -> Option<&egui::TextureHandle> {
            self.nearest((t * self.fps) as u64)
        }

        /// The decoded thumbnail closest to frame `i`.
        fn nearest(&self, i: u64) -> Option<&egui::TextureHandle> {
            let below = self.thumbs.range(..=i).next_back();
            let above = self.thumbs.range(i..).next();
            match (below, above) {
                (Some(b), Some(a)) => Some(if i - b.0 <= a.0 - i { &b.1.0 } else { &a.1.0 }),
                (Some(b), None) => Some(&b.1.0),
                (None, Some(a)) => Some(&a.1.0),
                (None, None) => None,
            }
        }

        fn receive(&mut self, ctx: &egui::Context) {
            while let Ok((i, p)) = self.got_rx.try_recv() {
                let img = crate::video::to_image(p);
                let tex = ctx.load_texture(format!("strip{i}"), img, egui::TextureOptions::LINEAR);
                self.thumbs.insert(i, (tex, self.tick));
            }
            // Over the limit: forget the ones drawn longest ago.
            if self.thumbs.len() > CACHE {
                let mut by_age: Vec<(u64, u64)> = self.thumbs.iter().map(|(i, (_, used))| (*used, *i)).collect();
                by_age.sort_unstable();
                for (_, i) in by_age.into_iter().take(self.thumbs.len() - CACHE) {
                    self.thumbs.remove(&i);
                }
            }
        }
    }

    /// The widest step from 1, 2, 3, 4, 5, 6, 7, 8, 10, 12, 14, 16… (frames) that's at
    /// most `frames` (at least 1): neighbours differ by 25% or less, so a
    /// picture's sides are trimmed by at most a fifth.
    pub(super) fn grid_step(frames: f64) -> u64 {
        let mut best = 1u64;
        let mut s = 1u64;
        while (s as f64) <= frames && s < 1 << 40 {
            for c in [s, s * 5 / 4, s * 3 / 2, s * 7 / 4] {
                if c as f64 <= frames {
                    best = best.max(c);
                }
            }
            s *= 2;
        }
        best
    }

    /// The filmstrip's decoding yields to everything else.
    fn lower_priority() {
        use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL};
        unsafe {
            let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
        }
    }
}

enum Msg {
    Keys(Vec<f64>),
    Frame(f64, media::Frame),
}

/// Keyframe thumbnails (no in-process decoder).
pub struct Keys {
    rx: Receiver<Msg>,
    /// The keyframes that become thumbnails, sorted; empty until known.
    keys: Vec<f64>,
    /// Their textures, by index into `keys`, as they arrive.
    thumbs: Vec<Option<egui::TextureHandle>>,
    /// Width / height of a thumbnail, once one has arrived.
    aspect: Option<f32>,
}

impl Keys {
    fn build(ctx: &egui::Context, source: &Path) -> Self {
        let (tx, rx) = mpsc::channel();
        let (src, repaint) = (source.to_path_buf(), ctx.clone());
        std::thread::spawn(move || {
            let keys_tx = tx.clone();
            let _ = media::keyframe_strip(
                &src,
                HEIGHT,
                MAX_THUMBS,
                |keys| {
                    let _ = keys_tx.send(Msg::Keys(keys.to_vec()));
                },
                |t, f| {
                    repaint.request_repaint();
                    tx.send(Msg::Frame(t, f)).is_ok() // stops decoding once closed
                },
            );
        });
        Self { rx, keys: Vec::new(), thumbs: Vec::new(), aspect: None }
    }

    fn paint(&mut self, ui: &egui::Ui, lane: Rect, from: f64, to: f64, dur: f64) {
        self.receive(ui.ctx());
        let Some(aspect) = self.aspect else { return };
        let span = (to - from).max(1e-6);
        // Keyframes closer together than one thumbnail's width: keep the first,
        // skip the rest. Decided in clip time, so panning never changes which.
        let min_gap = (lane.height() * aspect) as f64 / lane.width() as f64 * span;
        let mut shown: Vec<(f64, Option<&egui::TextureHandle>)> = Vec::new();
        for (i, k) in self.keys.iter().enumerate() {
            if shown.last().is_none_or(|(t, _)| k - t >= min_gap * 0.999) {
                shown.push((*k, self.thumbs[i].as_ref()));
            }
        }
        // Each from halfway to the one before to halfway to the one after.
        let cells: Vec<_> = (0..shown.len())
            .map(|n| {
                let t = shown[n].0;
                let start = n.checked_sub(1).map_or(0.0, |p| (shown[p].0 + t) / 2.0);
                let end = shown.get(n + 1).map_or(dur, |next| (next.0 + t) / 2.0);
                (start, end, shown[n].1)
            })
            .collect();
        paint_cells(ui, lane, from, to, &cells);
    }

    fn near(&self, t: f64) -> Option<&egui::TextureHandle> {
        self.nearest(t).and_then(|k| self.thumbs[k].as_ref())
    }

    fn receive(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Keys(keys) => {
                    self.thumbs = vec![None; keys.len()];
                    self.keys = keys;
                }
                Msg::Frame(t, f) => {
                    let Ok(k) = self.keys.binary_search_by(|x| x.total_cmp(&t)) else { continue };
                    let img = egui::ColorImage::from_rgba_unmultiplied([f.width as usize, f.height as usize], &f.rgba);
                    self.aspect = Some(f.width as f32 / f.height.max(1) as f32);
                    self.thumbs[k] = Some(ctx.load_texture(format!("strip{t:.3}"), img, egui::TextureOptions::LINEAR));
                }
            }
        }
    }

    /// Index of the chosen keyframe closest to `t`.
    fn nearest(&self, t: f64) -> Option<usize> {
        let at = self.keys.partition_point(|k| *k < t);
        [at.checked_sub(1), Some(at)]
            .into_iter()
            .flatten()
            .filter(|&j| j < self.keys.len())
            .min_by(|&a, &b| (self.keys[a] - t).abs().total_cmp(&(self.keys[b] - t).abs()))
    }
}

#[cfg(all(test, windows))]
mod tests {
    #[test]
    fn grid_steps_never_wider_than_a_thumbnail() {
        use super::exact::grid_step;
        assert_eq!(grid_step(0.4), 1);
        assert_eq!(grid_step(1.0), 1);
        assert_eq!(grid_step(5.9), 5);
        assert_eq!(grid_step(100.0), 96);
        for f in (3..2000).map(|n| n as f64 * 1.37) {
            let s = grid_step(f) as f64;
            assert!(s <= f && f / s < 1.26, "{f} -> {s}");
        }
    }
}

/// UV rect that center-crops a texture of `tex` size to fill `target`'s aspect.
pub fn crop_uv(tex: Vec2, target: Vec2) -> Rect {
    let (ta, ra) = (tex.x / tex.y, target.x / target.y);
    if ta > ra {
        let w = ra / ta;
        Rect::from_min_max(Pos2::new((1.0 - w) / 2.0, 0.0), Pos2::new((1.0 + w) / 2.0, 1.0))
    } else {
        let h = ta / ra;
        Rect::from_min_max(Pos2::new(0.0, (1.0 - h) / 2.0), Pos2::new(1.0, (1.0 + h) / 2.0))
    }
}
