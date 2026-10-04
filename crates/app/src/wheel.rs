//! Mouse-wheel input that glides: each notch eases out over a few frames and
//! slows down, the same however long the app sat idle before it.
//!
//! egui's own smoothing (`smooth_scroll_delta`, and the zoom it makes of
//! Ctrl+wheel) eases by the time since the last frame. The app redraws only
//! when something changes, so after a moment idle the first frame of a notch
//! saw 80-140 ms pass and applied ~90% of it at once: a jump. Here every frame
//! takes the same share of what's left, at the screen's measured frame time.

use egui::{Vec2, vec2};

/// Time constant of the glide: ~95% of a notch is applied within three.
const EASE: f32 = 0.05;

/// This frame's wheel and trackpad movement, unsmoothed.
#[derive(Default, Clone, Copy)]
pub struct Input {
    /// In points, content direction (as egui's). Shift turns it sideways.
    pub scroll: Vec2,
    /// Ctrl+wheel, in points (positive zooms in).
    pub zoom: f32,
    /// Trackpad pinch: a factor (1 = none).
    pub pinch: f32,
}

pub fn read(ctx: &egui::Context) -> Input {
    let line = ctx.options(|o| o.input_options.line_scroll_speed);
    let page = ctx.content_rect().height();
    ctx.input(|i| {
        let mut out = Input { pinch: 1.0, ..Default::default() };
        for e in &i.raw.events {
            match e {
                egui::Event::MouseWheel { unit, delta, modifiers, .. } => {
                    let d = match unit {
                        egui::MouseWheelUnit::Point => *delta,
                        egui::MouseWheelUnit::Line => *delta * line,
                        egui::MouseWheelUnit::Page => *delta * page,
                    };
                    if modifiers.command {
                        out.zoom += d.x + d.y;
                    } else if modifiers.shift {
                        out.scroll.x += d.x + d.y;
                    } else {
                        out.scroll += d;
                    }
                }
                egui::Event::Zoom(f) => out.pinch *= f,
                _ => {}
            }
        }
        out
    })
}

/// Movement still to apply, eased out frame by frame.
pub struct Glide {
    left: Vec2,
    /// The screen's frame time while gliding.
    frame_dt: f32,
    gliding: bool,
}

impl Default for Glide {
    fn default() -> Self {
        Self { left: Vec2::ZERO, frame_dt: 1.0 / 60.0, gliding: false }
    }
}

impl Glide {
    /// Add `input` and return this frame's share of what's left. Keeps the app
    /// redrawing until the glide is done.
    pub fn step(&mut self, ctx: &egui::Context, input: Vec2) -> Vec2 {
        self.left += input;
        let was_gliding = std::mem::replace(&mut self.gliding, self.left != Vec2::ZERO);
        if self.left == Vec2::ZERO {
            return Vec2::ZERO;
        }
        let since = ctx.input(|i| i.unstable_dt);
        let dt = if was_gliding {
            self.frame_dt = since.min(0.05);
            self.frame_dt
        } else {
            // Sat idle: the time since the last frame says nothing. The glide's
            // frame time, or less if the app has been busy drawing.
            since.min(self.frame_dt)
        };
        let share = 1.0 - (-dt / EASE).exp();
        let mut out = self.left * share;
        for d in 0..2 {
            if self.left[d].abs() < 0.5 {
                out[d] = self.left[d];
            }
        }
        self.left -= out;
        ctx.request_repaint();
        out
    }

    /// One number (e.g. a zoom amount), glided the same way.
    pub fn step1(&mut self, ctx: &egui::Context, input: f32) -> f32 {
        self.step(ctx, vec2(input, 0.0)).x
    }
}
