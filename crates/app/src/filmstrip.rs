//! The timeline's filmstrip: thumbnails of the clip's keyframes, shared by the
//! editor and the viewer.
//!
//! Which keyframes become thumbnails is decided before any is decoded (see
//! `media::keyframe_strip`), so each cell knows its picture from the start and
//! appears once, when that picture arrives. Cells never show a nearby stand-in
//! that's swapped out as better ones come in: that flickered for seconds and
//! re-uploaded textures every frame.

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

    /// Paint into `lane`, which shows `from..to` seconds of the clip.
    pub fn paint(&mut self, ui: &egui::Ui, lane: Rect, from: f64, to: f64) {
        self.receive(ui.ctx());
        let Some(aspect) = self.aspect else { return };
        let span = (to - from).max(1e-6);
        // As many uncropped thumbnails as fit; each cell shows the chosen keyframe
        // nearest its middle, or nothing until that one has arrived. Cells sit on
        // a fixed grid in clip time, so panning a zoomed timeline slides them
        // along rather than changing what each one shows.
        let fit = ((lane.width() / (lane.height() * aspect)).ceil() as f64).max(1.0);
        let cell_t = span / fit;
        let x_of = |t: f64| lane.left() + ((t - from) / span) as f32 * lane.width();
        let painter = ui.painter_at(lane);
        let mut k = (from / cell_t).floor();
        while k * cell_t < to {
            let (t0, t1) = (k * cell_t, (k + 1.0) * cell_t);
            if let Some(tex) = self.nearest((t0 + t1) / 2.0).and_then(|i| self.thumbs[i].as_ref()) {
                let cell = Rect::from_x_y_ranges(x_of(t0)..=x_of(t1), lane.y_range());
                painter.image(tex.id(), cell, crop_uv(tex.size_vec2(), cell.size()), Color32::WHITE);
            }
            k += 1.0;
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
