//! The timeline's filmstrip: thumbnails of the clip's keyframes, shared by the
//! editor and the viewer.
//!
//! Which keyframes become thumbnails is decided before any is decoded (see
//! `media::keyframe_strip`), so each picture appears once, when it arrives,
//! never as a stand-in swapped out later (that flickered for seconds).
//!
//! Every picture is pinned to the moment it shows: it starts at its keyframe's
//! time and repeats until the next one drawn. Zooming and panning move the
//! pictures with the timeline instead of re-dealing them into cells, so the
//! eye can follow them.

use std::path::Path;
use std::sync::mpsc::{self, Receiver};

use egui::{Color32, Pos2, Rect, Vec2};

/// Thumbnail height in pixels (lanes are ~56 pt; 2× for high-DPI screens).
const HEIGHT: u32 = 112;
/// At most this many thumbnails, however long the clip.
const MAX_THUMBS: usize = 160;

enum Msg {
    Keys(Vec<f64>),
    Frame(f64, media::Frame),
}

pub struct Filmstrip {
    rx: Receiver<Msg>,
    /// The keyframes that become thumbnails, sorted; empty until known.
    keys: Vec<f64>,
    /// Their textures, by index into `keys`, as they arrive.
    thumbs: Vec<Option<egui::TextureHandle>>,
    /// Width / height of a thumbnail, once one has arrived.
    aspect: Option<f32>,
}

impl Filmstrip {
    pub fn build(ctx: &egui::Context, source: &Path) -> Self {
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

    /// Paint into `lane`, which shows `from..to` seconds of a clip `dur` long.
    pub fn paint(&mut self, ui: &egui::Ui, lane: Rect, from: f64, to: f64, dur: f64) {
        self.receive(ui.ctx());
        let Some(aspect) = self.aspect else { return };
        let span = (to - from).max(1e-6);
        let x_of = |t: f64| lane.left() + ((t - from) / span) as f32 * lane.width();
        let w = lane.height() * aspect;
        // Pictures closer together than one thumbnail's width: keep the first,
        // skip the rest. Decided in clip time, so panning never changes which.
        let min_gap = w as f64 / lane.width() as f64 * span;
        let mut shown: Vec<usize> = Vec::new();
        for (i, k) in self.keys.iter().enumerate() {
            if shown.last().is_none_or(|&j| k - self.keys[j] >= min_gap * 0.999) {
                shown.push(i);
            }
        }
        let painter = ui.painter_at(lane);
        for (n, &i) in shown.iter().enumerate() {
            let start = x_of(self.keys[i]);
            let end = shown.get(n + 1).map_or(x_of(dur), |&j| x_of(self.keys[j]));
            if end < lane.left() || start > lane.right() {
                continue;
            }
            let Some(tex) = self.thumbs[i].as_ref() else { continue };
            // Repeat it until the next picture, cutting the last copy short.
            let mut x = start;
            while x < end.min(lane.right()) {
                let cw = w.min(end - x);
                if x + cw >= lane.left() {
                    let cell = Rect::from_x_y_ranges(x..=x + cw, lane.y_range());
                    let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(cw / w, 1.0));
                    painter.image(tex.id(), cell, uv, Color32::WHITE);
                }
                x += w;
            }
        }
    }

    /// The thumbnail nearest `t`, if it has arrived (a rough hover preview).
    pub fn near(&self, t: f64) -> Option<&egui::TextureHandle> {
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
