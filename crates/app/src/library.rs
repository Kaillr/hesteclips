//! The clip library: a thumbnail grid grouped by day, newest first.
//!
//! Click a card to play it; hover for a Share button; right-click for everything
//! else. While a capture is running a placeholder card stands in for the clip, so
//! a half-written file never shows up as if it were finished.

use std::path::PathBuf;
use std::time::Duration;

use egui::{Align2, Color32, CornerRadius, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2};

use crate::{App, RecState, clips, share, thumbs};

const MIN_CARD_WIDTH: f32 = 220.0;
const GAP: f32 = 14.0;
const CAPTION_HEIGHT: f32 = 42.0;
const RADIUS: u8 = 8;
/// How long a freshly saved clip stays highlighted.
const NEW_HIGHLIGHT: Duration = Duration::from_secs(8);

pub(crate) const REC_RED: Color32 = Color32::from_rgb(235, 72, 72);
pub(crate) const ACCENT: Color32 = Color32::from_rgb(90, 150, 255);

/// The "Rename clip" dialog.
pub(crate) struct Rename {
    path: PathBuf,
    name: String,
    error: Option<String>,
    focused: bool,
}

enum Action {
    /// Drag the file out of the window (to another app / the desktop).
    DragOut(PathBuf, Option<PathBuf>),
    Copy(PathBuf),
    /// System share sheet at a window position.
    ShareSheet(PathBuf, Pos2),
    Rename(PathBuf),
    Open(PathBuf),
    Edit(PathBuf),
    Reveal(PathBuf),
    Share(PathBuf),
    Trash(PathBuf),
}

enum Card<'a> {
    /// Recording or saving a replay clip.
    Placeholder,
    /// A "Save as new clip" render that has no file yet.
    NewRender(usize),
    Clip(&'a clips::Clip),
}

impl App {
    pub(crate) fn library(&mut self, ui: &mut egui::Ui, frame: &eframe::Frame) {
        self.permission_banner(ui);

        let placeholder = self.rec_state == RecState::Recording || self.saving;
        let new_renders: Vec<usize> = (0..self.renders.len()).filter(|&i| self.renders[i].as_new).collect();
        if self.clips.is_empty() && !placeholder && new_renders.is_empty() {
            self.empty_state(ui);
            return;
        }

        // Group into days. The placeholder always belongs to today. Cards borrow a
        // snapshot so drawing them can still use `&mut self` (thumbnail cache).
        let clips = self.clips.clone();
        let mut groups: Vec<(chrono::NaiveDate, Vec<Card>)> = Vec::new();
        let mut live: Vec<Card> = new_renders.into_iter().map(Card::NewRender).collect();
        if placeholder {
            live.insert(0, Card::Placeholder);
        }
        if !live.is_empty() {
            groups.push((chrono::Local::now().date_naive(), live));
        }
        for clip in &clips {
            let day = clip.day();
            match groups.last_mut() {
                Some((d, cards)) if *d == day => cards.push(Card::Clip(clip)),
                _ => groups.push((day, vec![Card::Clip(clip)])),
            }
        }

        let mut action = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            let avail = ui.available_width();
            let cols = (((avail + GAP) / (MIN_CARD_WIDTH + GAP)).floor() as usize).max(1);
            let card_w = (avail - GAP * (cols - 1) as f32) / cols as f32;

            for (day, cards) in &groups {
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(clips::day_label(*day)).strong().size(15.0));
                    let n = cards.iter().filter(|c| matches!(c, Card::Clip(_))).count();
                    if n > 0 {
                        ui.weak(if n == 1 { "1 clip".to_owned() } else { format!("{n} clips") });
                    }
                });
                ui.add_space(6.0);
                for row in cards.chunks(cols) {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = GAP;
                        for card in row {
                            match card {
                                Card::Placeholder => self.placeholder_card(ui, card_w),
                                Card::NewRender(i) => self.new_render_card(ui, *i, card_w),
                                Card::Clip(clip) => {
                                    if let Some(a) = self.clip_card(ui, clip, card_w) {
                                        action = Some(a);
                                    }
                                }
                            }
                        }
                    });
                    ui.add_space(GAP);
                }
            }
        });

        match action {
            Some(Action::Open(p)) => {
                if let Err(e) = clips::open_in_default_app(&p) {
                    self.toast_error(format!("Couldn't open the clip: {e}"));
                }
            }
            Some(Action::Reveal(p)) => {
                if let Err(e) = clips::reveal_in_file_manager(&p) {
                    self.toast_error(format!("Couldn't show the file: {e}"));
                }
            }
            Some(Action::Share(p)) => self.open_share_dialog(p),
            Some(Action::DragOut(p, preview)) => {
                // Hand the gesture to the OS; egui must forget its own drag or the
                // card would stay "grabbed" after the drop.
                ui.ctx().stop_dragging();
                if let Err(e) = share::start_drag(frame, &p, preview) {
                    self.toast_error(format!("Couldn't start the drag: {e}"));
                }
            }
            Some(Action::Copy(p)) => match share::copy_file(&p) {
                Ok(()) => self.toast("Copied — paste it into any app (⌘V)".replace("⌘V", if cfg!(target_os = "macos") { "⌘V" } else { "Ctrl+V" })),
                Err(e) => self.toast_error(format!("Couldn't copy the clip: {e}")),
            },
            Some(Action::ShareSheet(p, at)) => {
                if let Err(e) = share::share_sheet(frame, &p, at) {
                    self.toast_error(format!("Couldn't open sharing: {e}"));
                }
            }
            Some(Action::Edit(p)) => self.open_editor(p),
            Some(Action::Rename(p)) => self.rename_clip(p),
            Some(Action::Trash(p)) => match clips::move_to_trash(&p) {
                Ok(()) => {
                    media::revert(&p); // its edit sidecar + render go with it
                    self.toast(format!("Moved {} to the Trash", crate::file_name(&p)));
                    self.refresh_clips();
                }
                Err(e) => self.toast_error(format!("Couldn't move to the Trash: {e}")),
            },
            None => {}
        }
    }

    fn empty_state(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.28);
            ui.label(egui::RichText::new("🎬").size(44.0));
            ui.add_space(8.0);
            ui.label(egui::RichText::new("No clips yet").size(20.0).strong());
            ui.add_space(4.0);
            let hint = match self.rec_state {
                RecState::Buffering => format!(
                    "The replay buffer is running. Press {} or Save clip to keep the last {} seconds.",
                    crate::hotkey_label("F10"),
                    self.settings.replay_seconds
                ),
                RecState::Recording => "Recording… stop it to see your clip here.".to_owned(),
                RecState::Idle => format!(
                    "Start the replay buffer and press {} whenever something worth keeping happens.",
                    crate::hotkey_label("F10")
                ),
            };
            ui.weak(hint);
        });
    }

    /// Placeholder for a clip that doesn't exist yet: a pulsing dot, a label and,
    /// when known, a progress bar. Shared by recording, saving and rendering.
    fn progress_card(ui: &mut egui::Ui, w: f32, title: &str, caption: &str, color: Color32, progress: Option<f32>) {
        let (rect, _) = ui.allocate_exact_size(Vec2::new(w, w * 9.0 / 16.0 + CAPTION_HEIGHT), Sense::hover());
        let thumb = Rect::from_min_size(rect.min, Vec2::new(w, w * 9.0 / 16.0));
        let p = ui.painter();
        let v = ui.visuals();
        p.rect_filled(thumb, RADIUS, v.extreme_bg_color);
        p.rect_stroke(thumb, RADIUS, Stroke::new(1.5, color), StrokeKind::Inside);
        pulse(p, thumb.center() - Vec2::new(0.0, 14.0), color, ui.input(|i| i.time));
        p.text(thumb.center() + Vec2::new(0.0, 14.0), Align2::CENTER_CENTER, title, FontId::proportional(15.0), v.text_color());
        if let Some(f) = progress {
            progress_bar(p, thumb, f, color);
        }
        p.text(Pos2::new(rect.left() + 2.0, thumb.bottom() + 8.0), Align2::LEFT_TOP, caption, FontId::proportional(14.0), v.weak_text_color());
        ui.ctx().request_repaint();
    }

    fn new_render_card(&mut self, ui: &mut egui::Ui, i: usize, w: f32) {
        let job = &self.renders[i];
        let f = job.progress();
        // Show the source clip's preview under the progress, so it's clear what's being made.
        let source = self.clips.iter().find(|c| c.path == job.source).cloned();
        if let Some(src) = source {
            let name = job.dest.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let (rect, _) = ui.allocate_exact_size(Vec2::new(w, w * 9.0 / 16.0 + CAPTION_HEIGHT), Sense::hover());
            let thumb_rect = Rect::from_min_size(rect.min, Vec2::new(w, w * 9.0 / 16.0));
            let ctx = ui.ctx().clone();
            if let Some(tex) = self.thumbs.get(&ctx, &src).and_then(|t| t.texture.clone()) {
                let uv = crop_uv(tex.size_vec2(), thumb_rect.size());
                egui::Image::from_texture((tex.id(), thumb_rect.size())).uv(uv).corner_radius(RADIUS).paint_at(ui, thumb_rect);
            }
            let p = ui.painter();
            p.rect_filled(thumb_rect, RADIUS, Color32::from_black_alpha(160));
            p.rect_stroke(thumb_rect, RADIUS, Stroke::new(1.5, ACCENT), StrokeKind::Inside);
            pulse(p, thumb_rect.center() - Vec2::new(0.0, 12.0), ACCENT, ui.input(|i| i.time));
            p.text(thumb_rect.center() + Vec2::new(0.0, 14.0), Align2::CENTER_CENTER, format!("Saving new clip  {:.0}%", f * 100.0), FontId::proportional(14.0), Color32::WHITE);
            progress_bar(p, thumb_rect, f, ACCENT);
            let v = ui.visuals();
            p.text(Pos2::new(rect.left() + 2.0, thumb_rect.bottom() + 6.0), Align2::LEFT_TOP, &name, FontId::proportional(14.0), v.strong_text_color());
            ui.ctx().request_repaint();
            return;
        }
        let name = job.dest.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        Self::progress_card(ui, w, &format!("Saving  {:.0}%", f * 100.0), &name, ACCENT, Some(f));
    }

    fn placeholder_card(&mut self, ui: &mut egui::Ui, w: f32) {
        if self.rec_state == RecState::Recording {
            let secs = self.rec_started.map_or(0, |t| t.elapsed().as_secs());
            let title = format!("Recording  {}", thumbs::format_duration(Duration::from_secs(secs)));
            Self::progress_card(ui, w, &title, "In progress", REC_RED, None);
        } else {
            Self::progress_card(ui, w, "Saving clip…", "In progress", ACCENT, None);
        }
    }

    fn clip_card(&mut self, ui: &mut egui::Ui, clip: &clips::Clip, w: f32) -> Option<Action> {
        let thumb_h = w * 9.0 / 16.0;
        // click_and_drag: a click plays, a drag pulls the file out of the window.
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, thumb_h + CAPTION_HEIGHT), Sense::click_and_drag());
        let thumb_rect = Rect::from_min_size(rect.min, Vec2::new(w, thumb_h));
        let hovered = ui.rect_contains_pointer(rect);
        let is_new = self
            .last_saved
            .as_ref()
            .is_some_and(|(p, at)| *p == clip.path && at.elapsed() < NEW_HIGHLIGHT);

        let ctx = ui.ctx().clone();
        // An edited clip shows its edit: new first frame, new length.
        let shown = clip.display();
        let thumb = self.thumbs.get(&ctx, &shown);
        let v = ui.visuals().clone();
        let p = ui.painter();

        // --- Thumbnail (cropped to 16:9 so every card lines up) ---
        match thumb.and_then(|t| t.texture.as_ref()) {
            Some(tex) => {
                let uv = crop_uv(tex.size_vec2(), thumb_rect.size());
                egui::Image::from_texture((tex.id(), thumb_rect.size()))
                    .uv(uv)
                    .corner_radius(RADIUS)
                    .paint_at(ui, thumb_rect);
            }
            None => {
                p.rect_filled(thumb_rect, RADIUS, v.extreme_bg_color);
                let icon = if thumb.is_some() { "🎬" } else { "…" };
                p.text(thumb_rect.center(), Align2::CENTER_CENTER, icon, FontId::proportional(26.0), v.weak_text_color());
            }
        }

        if let Some(d) = thumb.and_then(|t| t.duration) {
            badge(p, thumb_rect.right_bottom() - Vec2::new(6.0, 6.0), Align2::RIGHT_BOTTOM, &thumbs::format_duration(d), Color32::from_black_alpha(190));
        }
        // An edit of this clip is rendering: dim it, pulse, and show progress.
        let render = self.renders.iter().find(|j| !j.as_new && j.source == clip.path).map(|j| j.progress());
        if let Some(f) = render {
            p.rect_filled(thumb_rect, RADIUS, Color32::from_black_alpha(150));
            pulse(p, thumb_rect.center() - Vec2::new(0.0, 12.0), ACCENT, ui.input(|i| i.time));
            p.text(thumb_rect.center() + Vec2::new(0.0, 14.0), Align2::CENTER_CENTER, format!("Saving edit  {:.0}%", f * 100.0), FontId::proportional(14.0), Color32::WHITE);
            progress_bar(p, thumb_rect, f, ACCENT);
            ui.ctx().request_repaint();
        } else if is_new {
            badge(p, thumb_rect.left_top() + Vec2::new(6.0, 6.0), Align2::LEFT_TOP, "NEW", ACCENT);
        }

        if hovered && render.is_none() {
            p.rect_filled(thumb_rect, RADIUS, Color32::from_black_alpha(70));
            let c = thumb_rect.center();
            p.circle_filled(c, 22.0, Color32::from_black_alpha(150));
            p.add(egui::Shape::convex_polygon(
                vec![c + Vec2::new(-6.0, -10.0), c + Vec2::new(11.0, 0.0), c + Vec2::new(-6.0, 10.0)],
                Color32::WHITE,
                Stroke::NONE,
            ));
        }
        let border = if is_new {
            Stroke::new(2.0, ACCENT)
        } else if hovered {
            Stroke::new(1.0, v.widgets.hovered.bg_stroke.color)
        } else {
            Stroke::new(1.0, v.widgets.noninteractive.bg_stroke.color)
        };
        p.rect_stroke(thumb_rect, RADIUS, border, StrokeKind::Inside);

        // --- Caption ---
        // Edited clips say so in words, in the accent colour, right where you read
        // the clip's details — not a dark chip lost on top of a busy thumbnail.
        let edited = clip.has_edit && clip.rendered.is_some() && render.is_none();
        let mut text_x = rect.left() + 2.0;
        let title_y = thumb_rect.bottom() + 6.0;
        if edited {
            text_x += edited_pill(p, Pos2::new(text_x, title_y + 1.0)) + 6.0;
        }
        let text_w = rect.right() - text_x - 2.0;
        let title = p.layout_job(single_line(&clip.title(), FontId::proportional(14.0), v.strong_text_color(), text_w));
        p.galley(Pos2::new(text_x, title_y), title, v.text_color());
        let ext = clip.path.extension().map(|e| e.to_string_lossy().to_uppercase()).unwrap_or_default();
        // "trimmed from 0:52" only when the length actually changed (an audio-only
        // edit keeps it). Probing the original is cached like any thumbnail.
        let trimmed_from = if edited {
            let _ = self.thumbs.get(&ctx, clip);
            match (self.thumbs.duration_of(clip), self.thumbs.duration_of(&shown)) {
                (Some(orig), Some(now)) if orig - now > 0.5 => Some(thumbs::format_duration(Duration::from_secs_f64(orig))),
                _ => None,
            }
        } else {
            None
        };
        let detail = match trimmed_from {
            Some(orig) => format!("{}  ·  {ext}  ·  trimmed from {orig}", shown.human_size()),
            None => format!("{}  ·  {ext}", shown.human_size()),
        };
        let meta = p.layout_job(single_line(&detail, FontId::proportional(12.0), v.weak_text_color(), w - 4.0));
        p.galley(Pos2::new(rect.left() + 2.0, thumb_rect.bottom() + 24.0), meta, v.weak_text_color());

        let mut action = None;

        // Hover shortcuts for the most common follow-ups.
        let share_rect = Rect::from_min_size(thumb_rect.right_top() + Vec2::new(-84.0, 6.0), Vec2::new(78.0, 26.0));
        let edit_rect = Rect::from_min_size(share_rect.left_top() - Vec2::new(70.0, 0.0), Vec2::new(64.0, 26.0));
        let share_id = ui.id().with(("share_menu", &clip.path));
        let menu_open = egui::Popup::is_id_open(ui.ctx(), share_id);
        if (hovered || menu_open) && render.is_none() {
            let overlay_button = |ui: &mut egui::Ui, rect: Rect, text: &str, fill: Color32| {
                // A detached child Ui: `ui.put` would count as another item in the row
                // and push the next card one gap to the right while hovered.
                ui.new_child(egui::UiBuilder::new().max_rect(rect)).add_sized(
                    rect.size(),
                    egui::Button::new(egui::RichText::new(text).color(Color32::WHITE))
                        .fill(fill)
                        .corner_radius(CornerRadius::same(6)),
                )
            };
            if overlay_button(ui, edit_rect, "✂ Edit", Color32::from_black_alpha(170)).clicked() {
                action = Some(Action::Edit(clip.path.clone()));
            }
            let share_btn = overlay_button(ui, share_rect, "📤 Share", ACCENT);
            egui::Popup::menu(&share_btn).id(share_id).show(|ui| {
                if let Some(a) = share_menu(ui, clip, share_btn.rect.left_bottom()) {
                    action = Some(a);
                }
            });
        }

        let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(format!(
            "{}\nClick to play · drag into any app to share",
            clip.name
        ));
        // Once the pointer has moved a little with the button held, it's a drag:
        // hand it to the OS so the clip can be dropped into Discord, Finder, a
        // browser… (egui alone can't drag outside its own window).
        if resp.drag_started() && action.is_none() {
            let preview = thumbs::cached_jpeg(&shown);
            action = Some(Action::DragOut(clip.playable().to_path_buf(), preview));
        }
        if resp.clicked() && action.is_none() {
            action = Some(Action::Open(clip.playable().to_path_buf()));
        }
        resp.context_menu(|ui| {
            ui.set_min_width(190.0);
            if ui.button("▶  Play").clicked() {
                action = Some(Action::Open(clip.playable().to_path_buf()));
            }
            if ui.button("✂  Edit…").clicked() {
                action = Some(Action::Edit(clip.path.clone()));
            }
            if ui.button("✏  Rename…").clicked() {
                action = Some(Action::Rename(clip.path.clone()));
            }
            ui.separator();
            if let Some(a) = share_menu(ui, clip, ui.ctx().pointer_latest_pos().unwrap_or_default()) {
                action = Some(a);
            }
            ui.separator();
            if ui.button(egui::RichText::new("🗑  Move to Trash").color(v.error_fg_color)).clicked() {
                action = Some(Action::Trash(clip.path.clone()));
            }
        });
        action
    }
}

impl App {
    pub(crate) fn rename_clip(&mut self, path: PathBuf) {
        let name = self.clips.iter().find(|c| c.path == path).map(|c| c.editable_name()).unwrap_or_default();
        self.rename = Some(Rename { path, name, error: None, focused: false });
    }

    /// The "Rename clip" dialog, if open.
    pub(crate) fn rename_dialog(&mut self, ctx: &egui::Context) {
        let Some(r) = &mut self.rename else { return };
        let mut close = false;
        let mut submit = false;
        let modal = egui::Modal::new(egui::Id::new("rename_clip")).show(ctx, |ui| {
            ui.set_width(380.0);
            ui.heading("Rename clip");
            ui.add_space(6.0);
            let out = egui::TextEdit::singleline(&mut r.name).desired_width(f32::INFINITY).show(ui);
            if !r.focused {
                // Open with the whole name selected, ready to type over.
                out.response.request_focus();
                let mut state = out.state.clone();
                state.cursor.set_char_range(Some(egui::text::CCursorRange::two(
                    egui::text::CCursor::new(0),
                    egui::text::CCursor::new(r.name.chars().count()),
                )));
                state.store(ui.ctx(), out.response.id);
                r.focused = true;
            }
            submit = out.response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if let Some(e) = &r.error {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.add(egui::Button::new(egui::RichText::new("Rename").color(Color32::WHITE)).fill(ACCENT)).clicked() {
                    submit = true;
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
        if submit {
            let ext = r.path.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or("mp4".into());
            let dir = r.path.parent().unwrap_or(std::path::Path::new(".")).to_path_buf();
            match clips::path_for_name(&dir, &r.name, &ext, Some(&r.path)) {
                Ok(to) if to == r.path => close = true,
                Ok(to) => match clips::rename(&r.path, &to) {
                    Ok(()) => {
                        if self.last_saved.as_ref().is_some_and(|(p, _)| *p == r.path) {
                            self.last_saved = None;
                        }
                        close = true;
                        self.refresh_clips();
                    }
                    Err(e) => r.error = Some(format!("Couldn't rename: {e}")),
                },
                Err(e) => r.error = Some(e),
            }
        }
        if close || (!submit && modal.should_close()) {
            self.rename = None;
        }
    }
}

/// The ways to get a clip out, shared by the Share button and the right-click
/// menu. Ordered by how often people reach for them.
fn share_menu(ui: &mut egui::Ui, clip: &clips::Clip, anchor: Pos2) -> Option<Action> {
    let file = clip.playable().to_path_buf();
    let mut action = None;
    ui.set_min_width(230.0);
    let paste = if cfg!(target_os = "macos") { "⌘V" } else { "Ctrl+V" };
    if ui.button("📋  Copy clip").on_hover_text(format!("Then paste it into Discord, a chat or a folder ({paste})")).clicked() {
        action = Some(Action::Copy(file.clone()));
    }
    if share::HAS_SHARE_SHEET && ui.button("📤  AirDrop, Messages, Mail…").clicked() {
        action = Some(Action::ShareSheet(file.clone(), anchor));
    }
    if ui.button("☁  Upload to HesteFiles…").clicked() {
        action = Some(Action::Share(file.clone()));
    }
    if ui.button(format!("📂  {}", crate::reveal_label())).clicked() {
        action = Some(Action::Reveal(file));
    }
    ui.separator();
    ui.weak("Tip: drag the clip into any app");
    if action.is_some() {
        ui.close();
    }
    action
}

/// The "Edited" marker in a card's caption: a small accent pill. Returns its width.
fn edited_pill(p: &egui::Painter, at: Pos2) -> f32 {
    let galley = p.layout_no_wrap("✂ Edited".to_owned(), FontId::proportional(11.5), Color32::WHITE);
    let rect = Rect::from_min_size(at, galley.size() + Vec2::new(12.0, 3.0));
    p.rect_filled(rect, 8, ACCENT);
    p.galley(rect.min + Vec2::new(6.0, 1.5), galley, Color32::WHITE);
    rect.width()
}

/// Soft pulsing dot (the same "live" cue as the recording indicator).
fn pulse(p: &egui::Painter, c: Pos2, color: Color32, time: f64) {
    let phase = (time * 2.5).sin() as f32 * 0.5 + 0.5;
    p.circle_filled(c, 9.0 + 5.0 * phase, color.gamma_multiply(0.18 + 0.12 * (1.0 - phase)));
    p.circle_filled(c, 7.0, color);
}

/// Thin progress bar along the bottom of a thumbnail.
fn progress_bar(p: &egui::Painter, thumb: Rect, f: f32, color: Color32) {
    let track = Rect::from_min_max(Pos2::new(thumb.left() + 14.0, thumb.bottom() - 16.0), Pos2::new(thumb.right() - 14.0, thumb.bottom() - 11.0));
    p.rect_filled(track, 3, Color32::from_white_alpha(30));
    let fill = Rect::from_min_max(track.min, Pos2::new(track.left() + track.width() * f.clamp(0.0, 1.0), track.bottom()));
    p.rect_filled(fill, 3, color);
}

/// UV rect that center-crops a texture of `tex` size to fill `target`'s aspect.
fn crop_uv(tex: Vec2, target: Vec2) -> Rect {
    let (ta, ra) = (tex.x / tex.y, target.x / target.y);
    if ta > ra {
        let w = ra / ta;
        Rect::from_min_max(Pos2::new((1.0 - w) / 2.0, 0.0), Pos2::new((1.0 + w) / 2.0, 1.0))
    } else {
        let h = ta / ra;
        Rect::from_min_max(Pos2::new(0.0, (1.0 - h) / 2.0), Pos2::new(1.0, (1.0 + h) / 2.0))
    }
}

fn badge(p: &egui::Painter, pos: Pos2, anchor: Align2, text: &str, fill: Color32) {
    let galley = p.layout_no_wrap(text.to_owned(), FontId::proportional(11.5), Color32::WHITE);
    let rect = anchor.anchor_size(pos, galley.size() + Vec2::new(10.0, 4.0));
    p.rect_filled(rect, 4, fill);
    p.galley(rect.min + Vec2::new(5.0, 2.0), galley, Color32::WHITE);
}

fn single_line(text: &str, font: FontId, color: Color32, max_width: f32) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_owned(), font, color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(max_width);
    job
}
