//! The Sources page: what goes into your clips. Beside the panel of the
//! preset's scenes (`scenes_ui`), a scene's page has its layers, the one in
//! front first (each with an eye to hide it, a grip to drag it in front or
//! behind), and the preview as big as there's room for, its sources placed
//! on it (`canvas_ui`: click one to select it, drag it, crop it). Under
//! both, the preset's sound, the same in every scene: the clip's mix, then
//! each source with its level, volume and mute. Each list's Add puts
//! another source in. A source's settings are in a dialog (`inspector_ui`):
//! double-click it, or its row's settings button, or Settings… in its menu.
//!
//! Deliberately says nothing about "tracks": a source is either part of what the
//! clip sounds like ("In the clip"), kept separately for editing, or both.

use std::time::{Duration, Instant};

use capture::mixer::SourceStatus;
use egui::{Color32, Sense};

use crate::App;
use crate::meter::{self, MeterState};
use crate::settings::{AudioSourceCfg, CaptureApp, CaptureTarget, SourceKind};

/// Fader range in dB.
const FADER_MIN_DB: f32 = -60.0;
const FADER_MAX_DB: f32 = 12.0;
/// How long the "limiting" note stays after the limiter last worked.
const LIMIT_HOLD: Duration = Duration::from_secs(3);
/// Under this page width, a scene's layers and its sound go under its
/// preview (else the layers are beside it, on its left, and the sound under
/// both).
const ONE_COLUMN: f32 = 700.0;
/// The layers' column, beside the preview.
const LAYERS_W: f32 = 260.0;
/// Between the page's columns.
const COLUMN_GAP: f32 = 20.0;
/// The sound under the layers and the preview takes this much of the
/// page's height at most (its rows scroll past it).
const SOUND_SHARE: f32 = 0.4;
/// A row in the video's or the sound's list.
const ROW_H: f32 = 36.0;
/// A sound source's name, in its row, before its fader (less in a row too
/// narrow for both: see `name_width`).
const NAME_W: f32 = 120.0;
/// A sound source's fader at its narrowest before its name gives way.
const MIN_FADER: f32 = 80.0;
/// Two columns from this width on the camera's and the crop's pages.
pub(crate) const TWO_COLUMNS: f32 = 860.0;

/// Meters for one source: what it delivers (before volume) and what goes into
/// the clip (after volume and mute).
#[derive(Default, Clone)]
struct SourceMeters {
    input: MeterState,
    output: MeterState,
}

/// UI state for the Sources page.
#[derive(Default)]
pub(crate) struct SourcesView {
    meters: std::collections::HashMap<String, SourceMeters>,
    master: MeterState,
    /// Limiter gain reduction shown, dB (falls smoothly), and when it last worked.
    reduction_db: f32,
    limiting_at: Option<Instant>,
    last_frame: Option<Instant>,
    /// Running apps for the "Add app" menu, refreshed when it opens.
    apps: Vec<capture::Device>,
    /// The preview picture, and which frame it shows.
    preview: Option<(egui::TextureHandle, u64)>,
    /// macOS: the preview frame drawn straight from the GPU, and the frames
    /// opened for it (the capture's buffers, each opened once).
    #[cfg(target_os = "macos")]
    preview_gpu: Option<(crate::gpu_frames::Shown, u64)>,
    #[cfg(target_os = "macos")]
    preview_frames: crate::gpu_frames::Frames,
    /// Preview frames shown in the current second, when it started, and the
    /// rate over the last full second.
    preview_rate: (u32, Option<Instant>, u32),
    /// When the open apps were last checked, for the games-and-apps statuses.
    apps_checked: Option<Instant>,
    /// The last preview frame and what it was of (picture, size, rate,
    /// webcam): shown again at once when the page opens on the same, while
    /// the preview's capture starts (that takes a moment).
    last_preview: Option<(PreviewSetup, std::sync::Arc<capture::preview::PreviewFrame>)>,
    /// The crop's page is open (in Sources' place), and what of the crop is
    /// held there.
    pub(crate) crop_open: bool,
    crop_drag: Option<(crate::webcam_ui::Handle, egui::Rect, egui::Pos2)>,
    /// A row of the video's list dragged by its grip to put it in front or
    /// behind, and how the sources were before (to undo it).
    pub(crate) row_drag: Option<(crate::canvas_ui::Layer, crate::canvas_ui::Arrangement)>,
    /// Displays for the Add menu, refreshed when it opens.
    pub(crate) add_displays: Vec<capture::Device>,
    /// A dialog open (a preset made, renamed, deleted; a scene deleted).
    pub(crate) dialog: Option<crate::scenes_ui::Dialog>,
    /// A settings dialog open: a source's, the clip mix's or the video's.
    pub(crate) editing: Option<crate::inspector_ui::Editing>,
}

/// Dev aid: `HASTECLIP_OPEN_SOURCES` names whose settings are open
/// (`screen`, `webcam`: the first one, `mix`, `video`) or the crop's page
/// (`crop`).
pub(crate) fn open_dev(app: &mut App, names: &str) {
    use crate::canvas_ui::Layer;
    use crate::inspector_ui::Editing;
    let webcam = app.settings.first_webcam().map(|w| w.key);
    for name in names.split(',').map(str::trim) {
        match name {
            "screen" => app.edit_layer(Layer::Screen),
            "webcam" => {
                if let Some(k) = webcam {
                    app.edit_layer(Layer::Webcam(k));
                }
            }
            "mix" => app.sources_view.editing = Some(Editing::Mix),
            "video" => app.sources_view.editing = Some(Editing::Video),
            "crop" => app.sources_view.crop_open = true,
            _ => {}
        }
    }
}

/// What a preview shows: the picture, its height and shape, the frame rate,
/// the webcam, and whether it's all of the screen, uncropped (to crop on).
pub(crate) type PreviewSetup = (capture::VideoSource, capture::VideoSize, u32, Option<capture::webcam::Webcam>, bool);

/// How wide a sound source's name is in a row `row_w` wide: [`NAME_W`], less
/// in a row too narrow for it beside the controls (the fader at
/// [`MIN_FADER`], the volume, mute and the settings button; see
/// `sound_row`), so they never run past the row's edge. A problem's mark
/// goes after the name, in its room. The clip's mix's meter starts there
/// too.
fn name_width(row_w: f32) -> f32 {
    (row_w - 36.0 - GEAR_ROOM - MIN_FADER - 66.0 - 26.0 - 12.0).clamp(48.0, NAME_W)
}

/// What a row keeps at its right end for its settings button.
const GEAR_ROOM: f32 = 36.0;

/// A row's settings button, in `rect` (drawn while `shown`): whether it was
/// clicked. Hidden, it takes no clicks (the row has them).
fn settings_button(ui: &mut egui::Ui, id: egui::Id, rect: egui::Rect, shown: bool, tip: &str) -> bool {
    use crate::theme;
    if !shown {
        return false;
    }
    let pal = theme::pal(ui);
    let r = ui.interact(rect, id, Sense::click());
    if r.hovered() {
        ui.painter().rect_filled(rect, 5, pal.control_h);
    }
    let ink = if r.hovered() { pal.text } else { pal.text2 };
    ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, egui_phosphor::regular::SLIDERS_HORIZONTAL, egui::FontId::proportional(15.0), ink);
    r.on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
}

/// A sound source's symbol.
fn sound_icon(s: &AudioSourceCfg) -> &'static str {
    use egui_phosphor::regular as icon;
    match &s.kind {
        SourceKind::Microphone { .. } => icon::MICROPHONE,
        SourceKind::Desktop { .. } => icon::SPEAKER_HIFI,
        SourceKind::App { app_name, .. } if app_name.to_lowercase().contains("discord") => icon::DISCORD_LOGO,
        SourceKind::App { .. } => icon::APP_WINDOW,
    }
}

/// Mute: lit while it's muted.
fn mute_button(ui: &mut egui::Ui, muted: &mut bool) {
    use crate::theme::{self, W};
    let pal = theme::pal(ui);
    let (r, resp) = ui.allocate_exact_size(egui::vec2(26.0, 24.0), Sense::click());
    let hv = theme::toggle(ui.ctx(), resp.id.with("hv"), resp.hovered(), theme::FAST);
    let on = theme::toggle(ui.ctx(), resp.id.with("on"), *muted, theme::FAST);
    ui.painter().rect_filled(r, 4, theme::mix(theme::mix(pal.control, pal.control_h, hv), pal.warn, on));
    ui.painter().text(r.center(), egui::Align2::CENTER_CENTER, "M", theme::font(11.0, W::Bold), theme::mix(pal.text2, Color32::from_rgb(20, 21, 24), on));
    if resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(if *muted { "Unmute" } else { "Mute: keep the source but record silence" }).clicked() {
        *muted = !*muted;
    }
}

impl App {
    /// Sources, with the screen's settings open.
    pub(crate) fn show_screen_card(&mut self) {
        self.page = crate::Page::Sources;
        self.sources_view.crop_open = false;
        self.webcam_view.crop = None;
        self.camera_panel = None;
        self.edit_layer(crate::canvas_ui::Layer::Screen);
    }
    pub(crate) fn sources_page(&mut self, ui: &mut egui::Ui) {
        let now = Instant::now();
        // Opening the page: start the meters from silence. Their last state is
        // stale, and the peak held since the last read could be from minutes ago
        // (meters keep collecting while a recording runs).
        if self.sources_view.last_frame.is_none_or(|t| now - t > Duration::from_millis(500)) {
            self.sources_view.meters.clear();
            self.sources_view.master = MeterState::default();
            for s in &self.settings.audio_sources {
                let ch = self.live_audio.channel(&s.id);
                ch.meter.take();
                ch.input.take();
            }
            self.live_audio.master.take();
            self.live_audio.take_reduction_db();
        }
        let dt = self.sources_view.last_frame.map_or(0.0, |t| (now - t).as_secs_f32()).min(0.2);
        self.sources_view.last_frame = Some(now);
        // Meters and the preview move: keep repainting while this page is open,
        // twice per frame of the preview (the recording's frame rate: once
        // per frame missed some, the two not lining up), else about 60 times
        // a second.
        let every = if capture::preview::AVAILABLE { Duration::from_secs_f64(0.5 / self.settings.fps.max(1) as f64) } else { Duration::from_millis(16) };
        crate::ui_kit::repaint_after(ui.ctx(), every);

        // Adjusting the camera, or a crop on all of a picture: a page of its
        // own, in Sources' place.
        if self.camera_panel.is_some() {
            self.camera_page(ui);
            return;
        }
        if self.webcam_view.crop.is_some() {
            self.webcam_crop_page(ui);
            return;
        }
        if self.sources_view.crop_open {
            self.screen_crop_page(ui);
            return;
        }
        let aspect = self.frame_aspect().unwrap_or(16.0 / 9.0);
        // The panel where Clips and Settings have theirs (from the rail's
        // edge, the page's margin inside it), the page beside it. Narrow,
        // the panel folds into the heading.
        let full = ui.max_rect();
        let wide = full.width() >= crate::library_view::PANEL_FROM;
        let edge = full.left() - crate::ui_kit::PAGE_MARGIN as f32;
        let panel = egui::Rect::from_min_max(egui::pos2(edge, full.top()), egui::pos2(edge + crate::library_view::PANEL_W, full.bottom()));
        let page = egui::Rect::from_min_max(egui::pos2(if wide { panel.right() + 24.0 } else { full.left() }, full.top()), full.max);
        let mut page_ui = ui.new_child(egui::UiBuilder::new().max_rect(page).layout(egui::Layout::top_down(egui::Align::Min)));
        // The keys: undo, redo, and what's done to the source selected.
        self.source_keys(page_ui.ctx(), aspect);
        self.sources_head(&mut page_ui, wide);
        let frame = crate::ui_kit::arrive(&mut page_ui, |ui| self.scene_editor(ui, aspect, now, dt));
        if wide {
            let mut panel_ui = ui.new_child(egui::UiBuilder::new().max_rect(panel).layout(egui::Layout::top_down(egui::Align::Min)));
            self.sources_panel(&mut panel_ui);
        }
        self.settings_dialog(ui.ctx(), frame.as_deref(), aspect);
        self.live_audio.set_limiter(self.settings.limiter);
    }

    /// The scene shown, in three places of its own: its layers, the preview
    /// beside them as big as there's room for, and under both the preset's
    /// sound (the same in every scene), as tall as its rows. Narrow, the
    /// preview's on top, the layers and the sound under it. A source's
    /// settings are in a dialog (see `inspector_ui`). The frame shown.
    fn scene_editor(&mut self, ui: &mut egui::Ui, aspect: f32, now: Instant, dt: f32) -> Option<std::sync::Arc<capture::preview::PreviewFrame>> {
        use egui::{Rect, pos2, vec2};
        ui.add_space(4.0);
        let area = ui.available_rect_before_wrap();
        let (w, h) = (area.width(), area.height());
        let place = |ui: &mut egui::Ui, r: Rect| ui.new_child(egui::UiBuilder::new().max_rect(r).layout(egui::Layout::top_down(egui::Align::Min)));
        let frame = if w >= ONE_COLUMN {
            let sound_h = self.sound_height().min(h * SOUND_SHARE);
            let top = Rect::from_min_max(area.min, pos2(area.right(), area.bottom() - sound_h - crate::ui_kit::STACK_GAP));
            let layers = Rect::from_min_size(top.min, vec2(LAYERS_W, top.height()));
            let picture = Rect::from_min_max(pos2(layers.right() + COLUMN_GAP, top.top()), top.max);
            // The toolbar's over it.
            let frame = self.picture_column(&mut place(ui, picture), (top.height() - 40.0).max(120.0));
            crate::ui_kit::scroll(&mut place(ui, layers), "scene_layers", |ui| self.video_list(ui, frame.as_deref(), aspect));
            let sound = Rect::from_min_max(pos2(area.left(), area.bottom() - sound_h), area.max);
            crate::ui_kit::scroll(&mut place(ui, sound), "scene_sound", |ui| self.sound_list(ui, now, dt));
            frame
        } else {
            let mut top = place(ui, area);
            let frame = self.picture_column(&mut top, h * 0.45);
            let below = Rect::from_min_max(pos2(area.left(), top.min_rect().bottom()), area.max);
            crate::ui_kit::scroll(&mut place(ui, below), "scene_lists", |ui| {
                self.video_list(ui, frame.as_deref(), aspect);
                crate::ui_kit::gap(ui, crate::ui_kit::STACK_GAP);
                self.sound_list(ui, now, dt);
                ui.add_space(28.0);
            });
            frame
        };
        ui.allocate_rect(area, Sense::hover());
        frame
    }

    /// How tall the sound is with all of it showing: its head, then the
    /// clip mix's row and each source's (or the line saying there's none).
    fn sound_height(&self) -> f32 {
        let rows = 1 + self.settings.audio_sources.len().max(1);
        50.0 + rows as f32 * (ROW_H + 6.0)
    }

    /// Over the preview: undo, redo and the video's size; then the preview
    /// itself, at most `max_h` tall. The frame shown.
    fn picture_column(&mut self, ui: &mut egui::Ui, max_h: f32) -> Option<std::sync::Arc<capture::preview::PreviewFrame>> {
        capture::preview::request();
        // The capture's newest frame; while it's starting, the last one of the
        // same picture (the page looks as it did straight away).
        let setup = self.video_preview.as_ref().map(|(w, _)| w.clone());
        let frame = match capture::preview::latest() {
            Some(f) => {
                if let Some(setup) = setup {
                    self.sources_view.last_preview = Some((setup, f.clone()));
                }
                Some(f)
            }
            None => self.sources_view.last_preview.as_ref().filter(|(was, _)| Some(was) == setup.as_ref()).map(|(_, f)| f.clone()),
        };
        self.update_preview_texture(ui.ctx(), frame.as_deref());
        // Which listed apps are open, for their status; cheap, so every 2 s.
        let apps_mode = matches!(self.settings.capture, CaptureTarget::Apps { .. });
        if apps_mode && self.sources_view.apps_checked.is_none_or(|t| t.elapsed() > Duration::from_secs(2)) {
            self.sources_view.apps_checked = Some(Instant::now());
            self.windowed_apps = capture::list_windowed_apps();
        }
        // What's recorded; while a preview of another size starts, the size
        // it will be (the button stays, and its choices stay open).
        let size = frame.as_ref().map(|f| f.recorded).or_else(|| capture::frame_size(&self.video_source(), self.settings.video_size()));
        let size_text = size.map_or_else(|| format!("{} fps", self.settings.fps), |(w, h)| format!("{w}×{h} at {} fps", self.settings.fps));
        if capture::preview::AVAILABLE {
            ui.horizontal(|ui| {
                ui.set_min_height(crate::ui_kit::SMALL_H);
                ui.spacing_mut().item_spacing.x = 2.0;
                self.undo_buttons(ui);
                ui.add_space(8.0);
                self.size_button(ui, &size_text);
                // Cropping on the preview: which, and Done, while it lasts.
                if let Some(l) = self.canvas.crop_mode {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        let done = crate::ui_kit::Btn::new(crate::ui_kit::Kind::Primary, "Done").with_icon(egui_phosphor::regular::CHECK).small().tip("Stop cropping (Enter)");
                        if done.show(ui).clicked() {
                            self.canvas.crop_mode = None;
                        }
                        let pal = crate::theme::pal(ui);
                        let what = format!("Cropping {}", self.settings.layer_title(l));
                        ui.add(egui::Label::new(crate::theme::text(what, 12.5, crate::theme::W::Regular).color(pal.text2)).truncate());
                    });
                }
            });
            ui.add_space(6.0);
            self.preview_picture(ui, frame.as_deref(), max_h);
            crate::ui_kit::gap(ui, crate::ui_kit::STACK_GAP);
        }
        frame
    }

    /// The video's sources, the one in front first: a row each, with its
    /// symbol, name, what it shows and an eye to hide it. A click selects it
    /// (on the preview too), twice or its settings button opens its
    /// settings, its grip drags it in front or behind, a right-click opens
    /// its menu; Add puts another in.
    fn video_list(&mut self, ui: &mut egui::Ui, frame: Option<&capture::preview::PreviewFrame>, aspect: f32) {
        use crate::theme::{self, W};
        let pal = theme::pal(ui);
        self.webcam_fit_pending(ui.ctx(), aspect);
        let order = self.layer_order();
        ui.horizontal(|ui| {
            ui.set_min_height(30.0);
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.label(theme::text("Layers", 15.0, W::Semibold).color(pal.text));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| self.video_add_menu(ui, aspect));
        });
        ui.add_space(4.0);
        self.canvas.card_hover = None;
        let mut rows = Vec::new();
        egui::Frame::new().fill(pal.raise).corner_radius(8).inner_margin(egui::Margin::same(4)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 2.0;
            for l in &order {
                let r = self.video_row(ui, *l, frame, aspect);
                rows.push((*l, r));
            }
        });
        self.reorder_rows(ui, &rows);
    }

    /// One video source's row (see `video_list`): where it was drawn.
    fn video_row(&mut self, ui: &mut egui::Ui, layer: crate::canvas_ui::Layer, frame: Option<&capture::preview::PreviewFrame>, aspect: f32) -> egui::Rect {
        use crate::theme::{self, W};
        use egui::{Align2, FontId, pos2, vec2};
        use egui_phosphor::regular as icon;
        let pal = theme::pal(ui);
        let selected = self.selected_layer() == Some(layer);
        let held = self.sources_view.row_drag.as_ref().is_some_and(|(l, _)| *l == layer);
        let on = self.layer_enabled(layer);
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
        crate::demo::mark(ui.ctx(), || format!("row:{}", self.settings.layer_title(layer)), rect);
        let hv = theme::toggle(ui.ctx(), resp.id.with("hv"), resp.hovered() || held, theme::FAST);
        let fill = if selected { pal.sel } else { theme::fade(pal.control, hv) };
        ui.painter().rect_filled(rect, 6, fill);
        if resp.hovered() {
            self.canvas.card_hover = Some(layer);
        }
        let painter = ui.painter().clone();
        let cy = rect.center().y;
        // Its grip, while the pointer's on the row: drag it up or down.
        let grip_rect = egui::Rect::from_min_max(pos2(rect.left(), rect.top()), pos2(rect.left() + 20.0, rect.bottom()));
        let grip = ui.interact(grip_rect, resp.id.with("grip"), Sense::drag());
        if hv > 0.0 || grip.hovered() {
            let ink = theme::fade(if grip.hovered() || held { pal.text } else { pal.text3 }, hv.max(f32::from(u8::from(grip.hovered()))));
            painter.text(pos2(rect.left() + 10.0, cy), Align2::CENTER_CENTER, icon::DOTS_SIX_VERTICAL, FontId::proportional(14.0), ink);
        }
        let grip = grip.on_hover_text("Drag up or down: the top one's in front").on_hover_cursor(egui::CursorIcon::Grab);
        if grip.drag_started() {
            self.sources_view.row_drag = Some((layer, self.arrangement()));
        }
        // Its eye: in your clips, or hidden.
        let eye_rect = egui::Rect::from_center_size(pos2(rect.right() - 18.0, cy), vec2(26.0, 26.0));
        let eye = ui.interact(eye_rect, resp.id.with("eye"), Sense::click());
        if eye.hovered() {
            painter.rect_filled(eye_rect, 5, pal.control_h);
        }
        painter.text(eye_rect.center(), Align2::CENTER_CENTER, if on { icon::EYE } else { icon::EYE_SLASH }, FontId::proportional(15.0), if on { pal.text2 } else { pal.text3 });
        let eye = eye.on_hover_text(if on { "Hide it: out of your clips, its settings kept  (H)" } else { "Show it: back in your clips  (H)" }).on_hover_cursor(egui::CursorIcon::PointingHand);
        if eye.clicked() {
            let was = self.arrangement();
            self.set_layer_enabled(layer, !on);
            self.keep_for_undo(was);
        }
        // Its settings button, beside the eye, while the pointer's on the
        // row or it's selected.
        let gear_rect = egui::Rect::from_center_size(pos2(eye_rect.left() - 16.0, cy), vec2(26.0, 26.0));
        let gear_shown = ui.rect_contains_pointer(rect) || selected;
        if settings_button(ui, resp.id.with("gear"), gear_rect, gear_shown && !held, "Its settings  (double-click it)") {
            self.edit_layer(layer);
        }
        // Its symbol, name, and what it shows (or what's wrong with it).
        let mut x = rect.left() + 24.0;
        let dim = |c: Color32| if on { c } else { theme::fade(c, 0.5) };
        painter.text(pos2(x + 8.0, cy), Align2::CENTER_CENTER, crate::inspector_ui::layer_icon(self, layer), FontId::proportional(16.0), dim(pal.text2));
        x += 26.0;
        let right = gear_rect.left() - 6.0;
        let name = self.settings.layer_title(layer);
        let g = painter.layout_job(crate::ui_kit::single_line_job(&name, theme::font(13.5, W::Medium), dim(pal.text), (right - x).max(10.0) * 0.6));
        let name_w = g.size().x;
        painter.galley(pos2(x, cy - g.size().y / 2.0), g, pal.text);
        x += name_w + 8.0;
        let (line, warn) = self.layer_line(layer, frame);
        let (line, color) = match (warn, on) {
            (Some(_), true) => (self.layer_problem(layer).unwrap_or(line), pal.warn),
            (_, false) => ("Hidden".to_owned(), pal.text3),
            _ => (line, pal.text3),
        };
        let g = painter.layout_job(crate::ui_kit::single_line_job(&line, theme::font(12.5, W::Regular), color, (right - x).max(10.0)));
        painter.galley(pos2(x, cy - g.size().y / 2.0), g, color);
        if resp.clicked() || resp.secondary_clicked() {
            self.select_layer(layer);
        }
        if resp.double_clicked() {
            self.edit_layer(layer);
        }
        let was = self.arrangement();
        crate::ui_kit::context_menu(&resp, 240.0, |ui| self.layer_menu(ui, layer, aspect));
        if self.arrangement() != was {
            self.keep_for_undo(was);
        }
        rect
    }

    /// A source's problem in a few words, for its row (its full words are
    /// over its settings).
    fn layer_problem(&self, layer: crate::canvas_ui::Layer) -> Option<String> {
        match layer {
            crate::canvas_ui::Layer::Webcam(key) => {
                let w = self.settings.webcam(key)?;
                matches!(capture::webcam::status_of(&w.id), capture::webcam::Status::Unavailable(_)).then(|| "Not working".to_owned())
            }
            crate::canvas_ui::Layer::Visual(key) => self.visual_state(key).map(str::to_owned),
            crate::canvas_ui::Layer::Screen => None,
        }
    }

    /// A row dragged by its grip, live (the preview shows it): it goes to
    /// the place of the row the pointer's over. Let go, it can be undone.
    fn reorder_rows(&mut self, ui: &egui::Ui, rows: &[(crate::canvas_ui::Layer, egui::Rect)]) {
        let Some(held) = self.sources_view.row_drag.as_ref().map(|(l, _)| *l) else { return };
        let down = ui.input(|i| i.pointer.primary_down());
        if down && let Some(p) = ui.input(|i| i.pointer.interact_pos()) {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
            let to = rows.iter().position(|(_, r)| p.y < r.bottom()).unwrap_or(rows.len().saturating_sub(1));
            let at = rows.iter().position(|(l, _)| *l == held);
            if at.is_some_and(|at| at != to) {
                self.move_layer(held, to);
            }
        }
        if !down
            && let Some((_, was)) = self.sources_view.row_drag.take()
            && self.arrangement() != was
        {
            self.keep_for_undo(was);
        }
    }

    /// Add, over the video's list: a webcam (each camera found), and where
    /// every source is drawn another display, a window, an image, a colour,
    /// words or a web page. What's added is selected; words and a web page,
    /// which have nothing to show yet, open their settings.
    fn video_add_menu(&mut self, ui: &mut egui::Ui, aspect: f32) {
        use crate::settings::{TextStyle, Visual};
        use crate::ui_kit::{Btn, Item};
        use egui_phosphor::regular as icon;
        // Where one webcam's drawn, a scene has one at most.
        let scene_has_webcam = self.layer_order().iter().any(|l| matches!(l, crate::canvas_ui::Layer::Webcam(_)));
        let webcam = capture::webcam::AVAILABLE && (capture::layers::MANY || !scene_has_webcam);
        if !webcam && !capture::layers::MANY {
            return;
        }
        let b = Btn::normal("Add").with_icon(icon::PLUS).trail(icon::CARET_DOWN).small().id("add_video").tip("Put a source in your video").show(ui);
        if b.clicked() {
            self.refresh_cameras();
            if capture::layers::MANY {
                self.sources_view.add_displays = capture::list_screens();
                self.windowed_apps = capture::list_windowed_apps();
            }
        }
        let mut added: Option<crate::canvas_ui::Layer> = None;
        let mut fill_in = false;
        let mut pick_image = false;
        crate::ui_kit::menu_popup(&b, false, 230.0, |ui| {
            if webcam {
                crate::ui_kit::submenu(ui, Item::new("Webcam").icon(icon::WEBCAM), |ui| {
                    let here = self.layer_order();
                    for c in self.cameras().to_vec() {
                        // A camera opens once: one webcam for it in the
                        // preset, which another scene's may be (it's shared).
                        let had = self.settings.webcam_of(&c.id).map(|w| w.key);
                        let shown = had.is_some_and(|k| here.contains(&crate::canvas_ui::Layer::Webcam(k)));
                        let item = Item::new(c.name.clone()).enabled(!shown);
                        let item = if shown { item.tip("Already in this scene") } else { item };
                        if item.show(ui).clicked() {
                            added = Some(match had {
                                Some(k) if self.settings.add_to_scene(crate::canvas_ui::Layer::Webcam(k)) => crate::canvas_ui::Layer::Webcam(k),
                                _ => crate::canvas_ui::Layer::Webcam(self.add_webcam(c, aspect)),
                            });
                        }
                    }
                    if self.cameras().is_empty() {
                        crate::ui_kit::menu_head(ui, "No cameras found.");
                    }
                });
            }
            if !capture::layers::MANY {
                return;
            }
            if capture::screens_listed() {
                crate::ui_kit::submenu(ui, Item::new("Display").icon(icon::MONITOR), |ui| {
                    for d in self.sources_view.add_displays.clone() {
                        if Item::new(d.name.clone()).show(ui).clicked() {
                            added = Some(crate::canvas_ui::Layer::Visual(self.add_visual(Visual::Display { id: d.id, name: d.name })));
                        }
                    }
                    if self.sources_view.add_displays.is_empty() {
                        crate::ui_kit::menu_head(ui, "No displays found.");
                    }
                });
            }
            if capture::layers::WINDOWS {
                crate::ui_kit::submenu(ui, Item::new("Window").icon(icon::APP_WINDOW), |ui| {
                    egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                        for app in self.windowed_apps.clone() {
                            let tex = self.app_icons.get(ui.ctx(), &app.id).map(|t| t.id());
                            if Item::new(app.name.clone()).image(tex).show(ui).clicked() {
                                added = Some(crate::canvas_ui::Layer::Visual(self.add_visual(Visual::Window { app: app.id, name: app.name })));
                            }
                        }
                    });
                    if self.windowed_apps.is_empty() {
                        crate::ui_kit::menu_head(ui, "No windows found.");
                    }
                });
            }
            crate::ui_kit::menu_sep(ui);
            if Item::new("Image…").icon(icon::IMAGE).tip("A logo or a frame, its transparency kept").show(ui).clicked() {
                pick_image = true;
            }
            if Item::new("Text").icon(icon::TEXT_T).tip("Your name, a title").show(ui).clicked() {
                added = Some(crate::canvas_ui::Layer::Visual(self.add_visual(Visual::Text(TextStyle::default()))));
                fill_in = true;
            }
            if Item::new("Colour").icon(icon::PAINT_BUCKET).tip("All of the video one colour, at the back: a background").show(ui).clicked() {
                let key = self.add_visual(Visual::Colour { rgba: [24, 24, 28, 255] });
                let last = self.layer_order().len().saturating_sub(1);
                self.move_layer(crate::canvas_ui::Layer::Visual(key), last);
                added = Some(crate::canvas_ui::Layer::Visual(key));
            }
            if capture::layers::BROWSER && Item::new("Web page").icon(icon::GLOBE).tip("Alerts, a chat overlay, a widget (its transparency kept)").show(ui).clicked() {
                added = Some(crate::canvas_ui::Layer::Visual(self.add_visual(Visual::Browser { url: "https://".into(), width: 800, height: 600, reload: 0 })));
                fill_in = true;
            }
            // The preset's sources this scene doesn't have: the same source
            // (its settings shared), where it was last placed.
            let outside = self.settings.outside_scene();
            if !outside.is_empty() {
                crate::ui_kit::menu_sep(ui);
                crate::ui_kit::submenu(ui, Item::new("From other scenes").icon(icon::STACK), |ui| {
                    for l in outside {
                        let item = Item::new(self.settings.layer_title(l)).icon(crate::inspector_ui::layer_icon(self, l)).value(self.layer_line(l, None).0);
                        if item.show(ui).clicked() && self.settings.add_to_scene(l) {
                            added = Some(l);
                        }
                    }
                });
            }
        });
        // Picked after the menu's closed: the file dialog waits for you.
        if pick_image && let Some(path) = rfd::FileDialog::new().add_filter("Images", &crate::inspector_ui::IMAGE_EXTS).pick_file() {
            added = Some(crate::canvas_ui::Layer::Visual(self.add_visual(Visual::Image { path })));
        }
        match added {
            Some(l) if fill_in => self.edit_layer(l),
            Some(l) => self.select_layer(l),
            None => {}
        }
    }

    /// The preset's sound, the same in every scene: the clip's mix first,
    /// then each source, a row each with its level and volume as one
    /// control, its volume as a number, and mute. Twice on a row, or its
    /// settings button, opens its settings; Add puts another in. While
    /// capturing, a lock by its name says what changes now and what waits.
    fn sound_list(&mut self, ui: &mut egui::Ui, now: Instant, dt: f32) {
        use crate::theme::{self, W};
        let pal = theme::pal(ui);
        ui.horizontal(|ui| {
            ui.set_min_height(30.0);
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.add(egui::Label::new(theme::text("Sound", 15.0, W::Semibold).color(pal.text)).sense(Sense::hover())).on_hover_text("The preset's sound: the same in every scene");
            if !self.rec_state.idle() {
                let mic = if cfg!(target_os = "linux") { ", noise removal" } else { ", noise removal and which microphone" };
                let stop = format!("Stop {}", self.rec_state.words());
                let note = if capture::SOURCES_WHILE_CAPTURING {
                    format!("Volume, mute{mic} change right away, as do sources added, turned off or removed. {stop} to move one or change where it goes.")
                } else {
                    format!("Volume, mute{mic} change right away. {stop} to add or remove sources.")
                };
                ui.add(egui::Label::new(theme::ic(egui_phosphor::regular::LOCK_SIMPLE, 12.0).color(pal.text3)).sense(Sense::hover())).on_hover_text(note);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| self.sound_add_menu(ui));
        });
        ui.add_space(4.0);
        let mut menu_action = None;
        egui::Frame::new().fill(pal.raise).corner_radius(8).inner_margin(egui::Margin::same(4)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 2.0;
            self.mix_row(ui, now, dt);
            for i in 0..self.settings.audio_sources.len() {
                if let Some(a) = self.sound_row(ui, i, now, dt) {
                    menu_action = Some((i, a));
                }
            }
            if self.settings.audio_sources.is_empty() {
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.add_space(10.0);
                    ui.label(theme::text("No sound yet: add your microphone, or what your computer plays.", 12.5, W::Regular).color(pal.text3));
                });
                ui.add_space(8.0);
            }
        });
        if let Some((i, a)) = menu_action {
            self.sound_action(i, a);
        }
        self.keep_listening();
    }

    /// The clip's mix, first in the sound's list: its level (both sides),
    /// and its peak. Twice, or its settings button, opens its settings (the
    /// limiter).
    fn mix_row(&mut self, ui: &mut egui::Ui, now: Instant, dt: f32) {
        use crate::theme::{self, W};
        use egui::{Align2, FontId, pos2, vec2};
        let pal = theme::pal(ui);
        let view = &mut self.sources_view;
        view.master.update(self.live_audio.master.take(), now, dt);
        let reduction = self.live_audio.take_reduction_db();
        view.reduction_db = if reduction > view.reduction_db { reduction } else { (view.reduction_db - 12.0 * dt).max(reduction) };
        if reduction > 0.5 {
            view.limiting_at = Some(now);
        }
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H + 4.0), Sense::click());
        let hv = theme::toggle(ui.ctx(), resp.id.with("hv"), resp.hovered(), theme::FAST);
        ui.painter().rect_filled(rect, 6, theme::fade(pal.control, hv));
        let cy = rect.center().y;
        let right = rect.right() - GEAR_ROOM;
        ui.painter().text(pos2(rect.left() + 18.0, cy), Align2::CENTER_CENTER, egui_phosphor::regular::WAVEFORM, FontId::proportional(16.0), pal.text2);
        ui.painter().text(pos2(rect.left() + 36.0, cy), Align2::LEFT_CENTER, "Clip mix", theme::font(13.5, W::Medium), pal.text);
        let view = &self.sources_view;
        let readout = format!("{} dB", view.master.readout().replace('-', "−"));
        let peak_w = 64.0;
        ui.painter().text(pos2(right - 4.0, cy), Align2::RIGHT_CENTER, readout, theme::font(12.5, W::Medium), view.master.readout_color(ui.visuals()));
        let meter_rect = egui::Rect::from_min_max(pos2(rect.left() + 36.0 + name_width(rect.width()), cy - 7.0), pos2(right - 4.0 - peak_w, cy + 7.0));
        if meter_rect.width() > 20.0 {
            let mut mui = ui.new_child(egui::UiBuilder::new().max_rect(meter_rect).layout(egui::Layout::top_down(egui::Align::Min)));
            let style = meter::Style { bar_h: 5.0, gap: 2.0, scale: false, dimmed: false };
            meter::stereo(&mut mui, egui::Id::new("mix_row_meter"), &view.master, &style);
        }
        let gear_rect = egui::Rect::from_center_size(pos2(rect.right() - GEAR_ROOM / 2.0, cy), vec2(26.0, 26.0));
        let gear = settings_button(ui, resp.id.with("gear"), gear_rect, ui.rect_contains_pointer(rect), "Its settings: the limiter  (double-click it)");
        if resp.on_hover_text("Every source in the clip's sound, mixed: what people hear").double_clicked() || gear {
            self.sources_view.editing = Some(crate::inspector_ui::Editing::Mix);
        }
    }

    /// One sound source's row (see `sound_list`): a menu's action, if one
    /// was picked on its right-click.
    fn sound_row(&mut self, ui: &mut egui::Ui, i: usize, now: Instant, dt: f32) -> Option<CardAction> {
        use crate::theme::{self, W};
        use egui::{Align2, FontId, pos2, vec2};
        let pal = theme::pal(ui);
        let id = self.settings.audio_sources[i].id.clone();
        let channel = self.live_audio.channel(&id);
        let meters = self.sources_view.meters.entry(id.clone()).or_default();
        meters.input.update(channel.input.take(), now, dt);
        meters.output.update(channel.meter.take(), now, dt);
        let meters = meters.clone();
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H + 4.0), Sense::click());
        let hv = theme::toggle(ui.ctx(), resp.id.with("hv"), resp.hovered(), theme::FAST);
        ui.painter().rect_filled(rect, 6, theme::fade(pal.control, hv));
        let s = &self.settings.audio_sources[i];
        let on = s.enabled;
        let dim = |c: Color32| if on { c } else { theme::fade(c, 0.5) };
        let cy = rect.center().y;
        let glyph = sound_icon(s);
        let lead = egui::Rect::from_center_size(pos2(rect.left() + 18.0, cy), vec2(17.0, 17.0));
        let own = match &s.kind {
            SourceKind::App { bundle_id, .. } => crate::app_icons::paint(ui, &mut self.app_icons, &bundle_id.clone(), lead),
            _ => false,
        };
        let s = &self.settings.audio_sources[i];
        if !own {
            ui.painter().text(lead.center(), Align2::CENTER_CENTER, glyph, FontId::proportional(16.0), dim(pal.text2));
        }
        let name_w = name_width(rect.width());
        // What's wrong with it, on its row: a mark after its name (its words
        // on hover), the name a little shorter for it; the fader keeps its
        // length.
        let problem = status_text(channel.status()).filter(|_| on);
        let mark_room = if problem.is_some() { 22.0 } else { 0.0 };
        let name_job = crate::ui_kit::single_line_job(&s.name, theme::font(13.5, W::Medium), dim(if s.muted { pal.text2 } else { pal.text }), name_w - 8.0 - mark_room);
        let g = ui.painter().layout_job(name_job);
        let name_end = rect.left() + 36.0 + g.size().x;
        ui.painter().galley(pos2(rect.left() + 36.0, cy - g.size().y / 2.0), g, pal.text);
        if let Some((text, color)) = problem {
            let r = egui::Rect::from_center_size(pos2(name_end + 13.0, cy), vec2(16.0, 16.0));
            ui.painter().text(r.center(), Align2::CENTER_CENTER, egui_phosphor::regular::WARNING, FontId::proportional(14.0), color(pal));
            ui.interact(r, resp.id.with("problem"), Sense::hover()).on_hover_text(text);
        }
        // The fader, its value and mute, in what's left of the row (but its
        // settings button's room).
        let controls = egui::Rect::from_min_max(pos2(rect.left() + 36.0 + name_w, rect.top()), pos2(rect.right() - GEAR_ROOM, rect.bottom()));
        let mut action = None;
        if on {
            let mut cui = ui.new_child(egui::UiBuilder::new().max_rect(controls).layout(egui::Layout::left_to_right(egui::Align::Center)));
            cui.spacing_mut().item_spacing.x = 6.0;
            let source = &mut self.settings.audio_sources[i];
            let fader_w = (controls.width() - 66.0 - 26.0 - 12.0).max(40.0);
            let state = if source.muted { &meters.input } else { &meters.output };
            let fresp = fader(&mut cui, &mut source.volume_db, state, source.muted, fader_w, &source.id);
            volume_value(&mut cui, &mut source.volume_db, fresp.secondary_clicked(), &source.id);
            mute_button(&mut cui, &mut source.muted);
        } else {
            ui.painter().text(pos2(rect.right() - GEAR_ROOM - 6.0, cy), Align2::RIGHT_CENTER, "Off", theme::font(12.5, W::Regular), pal.text3);
        }
        // Volume applies live, even mid-recording.
        let s = &self.settings.audio_sources[i];
        channel.set_volume(from_db(s.volume_db), s.muted);
        channel.set_denoise(s.noise_reduction.into());
        channel.set_mono(s.mono);
        // Its settings: its button (while the pointer's on the row), or
        // twice on the row.
        let gear_rect = egui::Rect::from_center_size(pos2(rect.right() - GEAR_ROOM / 2.0, cy), vec2(26.0, 26.0));
        if settings_button(ui, resp.id.with("gear"), gear_rect, ui.rect_contains_pointer(rect), "Its settings  (double-click it)") || resp.double_clicked() {
            action = Some(CardAction::Settings);
        }
        crate::ui_kit::context_menu(&resp, 220.0, |ui| {
            if let Some(a) = self.sound_menu(ui, i) {
                action = Some(a);
            }
        });
        action
    }

    /// A sound source's menu (right-click its row, or the ⋯ in its
    /// settings' dialog): its settings, move it, mute it, turn it off,
    /// remove it.
    fn sound_menu(&mut self, ui: &mut egui::Ui, i: usize) -> Option<CardAction> {
        use crate::ui_kit::Item;
        use egui_phosphor::regular as icon;
        let locked = (!self.rec_state.idle()).then(|| self.stop_first());
        let fixed = locked.as_ref().filter(|_| !capture::SOURCES_WHILE_CAPTURING);
        let has_app_sources = self.settings.audio_sources.iter().any(|s| matches!(s.kind, SourceKind::App { .. }));
        let n = self.settings.audio_sources.len();
        let mut action = None;
        // Its settings, but in their dialog (whose ⋯ this is).
        if self.sources_view.editing.is_none() {
            if Item::new("Settings…").icon(icon::SLIDERS_HORIZONTAL).show(ui).clicked() {
                action = Some(CardAction::Settings);
            }
            crate::ui_kit::menu_sep(ui);
        }
        let up = Item::new("Move up").icon(icon::CARET_UP).enabled(i > 0 && locked.is_none());
        let up = match &locked {
            Some(why) => up.tip(why.clone()),
            None => up,
        };
        if up.show(ui).clicked() {
            action = Some(CardAction::MoveUp);
        }
        let down = Item::new("Move down").icon(icon::CARET_DOWN).enabled(i + 1 < n && locked.is_none());
        let down = match &locked {
            Some(why) => down.tip(why.clone()),
            None => down,
        };
        if down.show(ui).clicked() {
            action = Some(CardAction::MoveDown);
        }
        crate::ui_kit::menu_sep(ui);
        let source = &mut self.settings.audio_sources[i];
        if Item::new(if source.muted { "Unmute" } else { "Mute" }).icon(icon::SPEAKER_X).show(ui).clicked() {
            source.muted = !source.muted;
        }
        if Item::new(if source.enabled { "Turn off" } else { "Turn on" }).icon(icon::POWER).enabled(fixed.is_none()).show(ui).clicked() {
            source.enabled = !source.enabled;
        }
        if let SourceKind::Desktop { exclude_apps } = &mut source.kind {
            crate::ui_kit::menu_sep(ui);
            let item = Item::new("Leave out apps added below").check(*exclude_apps).enabled(has_app_sources && locked.is_none());
            if item.tip("Apps you add as their own source won't also be heard here, so nothing plays twice").show(ui).clicked() {
                *exclude_apps = !*exclude_apps;
            }
        }
        crate::ui_kit::menu_sep(ui);
        if Item::new("Remove").icon(icon::TRASH).danger().show(ui).clicked() {
            action = Some(if fixed.is_some() { CardAction::Locked } else { CardAction::Remove });
        }
        action
    }

    /// Do what a sound source's menu asked.
    fn sound_action(&mut self, i: usize, a: CardAction) {
        match a {
            CardAction::Remove => {
                let s = self.settings.audio_sources.remove(i);
                self.toast_icon(egui_phosphor::regular::TRASH, format!("Removed {}", s.name));
            }
            CardAction::Settings => self.sources_view.editing = Some(crate::inspector_ui::Editing::Sound(self.settings.audio_sources[i].id.clone())),
            CardAction::MoveUp if i > 0 => self.settings.audio_sources.swap(i, i - 1),
            CardAction::MoveDown if i + 1 < self.settings.audio_sources.len() => self.settings.audio_sources.swap(i, i + 1),
            CardAction::Listen => self.toggle_listening(i),
            CardAction::Locked => {
                let why = self.stop_first();
                self.toast_icon(egui_phosphor::regular::LOCK_SIMPLE, why.trim_end_matches('.').to_owned());
            }
            _ => {}
        }
    }

    /// Add, over the sound's list: a microphone (its settings open, to pick
    /// which), one app on its own, or everything the computer plays; and
    /// looking for microphones again.
    fn sound_add_menu(&mut self, ui: &mut egui::Ui) {
        use crate::ui_kit::{Btn, Item};
        use egui_phosphor::regular as icon;
        // Which sources are recorded is set when capture starts, but where
        // one can be added to it as it runs (`capture::SOURCES_WHILE_CAPTURING`).
        let idle = self.rec_state.idle();
        let can_add = idle || capture::SOURCES_WHILE_CAPTURING;
        let stop_first = self.stop_first();
        let b = Btn::normal("Add").with_icon(icon::PLUS).trail(icon::CARET_DOWN).small().id("add_sound").tip(if can_add { "Put a sound in your clips" } else { stop_first.as_str() }).show(ui);
        if b.clicked() {
            if can_add {
                self.sources_view.apps = capture::list_apps();
            } else {
                self.toast_icon(icon::LOCK_SIMPLE, stop_first.trim_end_matches('.'));
                egui::Popup::close_id(ui.ctx(), b.id.with("menu"));
            }
        }
        if !can_add {
            return;
        }
        let mut new_mic: Option<String> = None;
        let mut refresh = false;
        crate::ui_kit::menu_popup(&b, false, 230.0, |ui| {
            if Item::new("Microphone").icon(icon::MICROPHONE).tip("A microphone or audio interface").show(ui).clicked() {
                // Captured at once: the default microphone as it is now
                // (the list is looked at again when capturing starts).
                if !idle {
                    self.refresh_audio_devices();
                }
                let n = self.settings.audio_sources.iter().filter(|s| matches!(s.kind, SourceKind::Microphone { .. })).count();
                let name = if n == 0 { "Microphone".to_owned() } else { format!("Microphone {}", n + 1) };
                let s = AudioSourceCfg::new(&name, SourceKind::Microphone { device: capture::audio::DEFAULT_DEVICE.into() });
                new_mic = Some(s.id.clone());
                self.settings.audio_sources.push(s);
            }
            crate::ui_kit::submenu(ui, Item::new("App").icon(icon::APP_WINDOW), |ui| {
                crate::ui_kit::menu_head(ui, "One app on its own: a game, Discord, music");
                let taken: Vec<String> = self.settings.audio_sources.iter().filter_map(|s| match &s.kind {
                    SourceKind::App { bundle_id, .. } => Some(bundle_id.clone()),
                    _ => None,
                }).collect();
                egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                    for app in self.sources_view.apps.clone() {
                        let already = taken.contains(&app.id);
                        let tex = self.app_icons.get(ui.ctx(), &app.id).map(|t| t.id());
                        let item = Item::new(app.name.clone()).image(tex).enabled(!already);
                        let item = if already { item.tip("Already added") } else { item };
                        if item.show(ui).clicked() {
                            let s = AudioSourceCfg::new(&app.name, SourceKind::App { bundle_id: app.id.clone(), app_name: app.name.clone() });
                            self.settings.audio_sources.push(s);
                        }
                    }
                });
                if self.sources_view.apps.is_empty() {
                    // PipeWire only knows apps by their sound.
                    crate::ui_kit::menu_head(ui, if cfg!(target_os = "linux") { "No apps are playing sound. Start one, then look again." } else { "No apps found." });
                }
            });
            let has_desktop = self.settings.audio_sources.iter().any(|s| matches!(s.kind, SourceKind::Desktop { .. }));
            let item = Item::new("Desktop sound").icon(icon::SPEAKER_HIFI).enabled(!has_desktop);
            let item = if has_desktop { item.tip("Already added") } else { item.tip("Everything your computer plays") };
            if item.show(ui).clicked() {
                let s = AudioSourceCfg::new("Desktop sound", SourceKind::Desktop { exclude_apps: true });
                self.settings.audio_sources.push(s);
            }
            crate::ui_kit::menu_sep(ui);
            if Item::new("Look for microphones again").icon(icon::ARROWS_CLOCKWISE).tip("After you've plugged one in").show(ui).clicked() {
                refresh = true;
            }
        });
        if refresh {
            self.refresh_audio_devices();
            self.toast_icon(icon::ARROWS_CLOCKWISE, "Looked for microphones again");
        }
        if let Some(id) = new_mic {
            self.sources_view.editing = Some(crate::inspector_ui::Editing::Sound(id));
        }
    }

    /// A sound source's settings, in its dialog: its name, switch and menu;
    /// its volume; what it takes in; where it goes; a microphone's clean-up,
    /// and listening to it.
    pub(crate) fn sound_inspector(&mut self, ui: &mut egui::Ui, id: &str) {
        use crate::theme::{self, W};
        use crate::ui_kit::{Btn, Item, Kind, Row};
        use egui_phosphor::regular as icon;
        let pal = theme::pal(ui);
        let Some(i) = self.settings.audio_sources.iter().position(|s| s.id == id) else {
            self.sources_view.editing = None;
            return;
        };
        let locked = (!self.rec_state.idle()).then(|| self.stop_first());
        let fixed = locked.clone().filter(|_| !capture::SOURCES_WHILE_CAPTURING);
        let picker_locked = locked.clone().filter(|_| !matches!(self.settings.audio_sources[i].kind, SourceKind::Microphone { .. }));
        let note = self.added_note(&self.settings.audio_sources[i]);
        let has_app_sources = self.settings.audio_sources.iter().any(|s| matches!(s.kind, SourceKind::App { .. }));
        let channel = self.live_audio.channel(id);
        let listening = self.listening.as_ref().filter(|l| l.source == id).map(|l| l.monitor.latency_ms());
        let meters = self.sources_view.meters.get(id).cloned().unwrap_or_default();
        let mut action: Option<CardAction> = None;
        // Its head: symbol, name (click to rename), switch, menu.
        let rename_id = egui::Id::new(("rename_sound", id));
        let draft_id = rename_id.with("draft");
        let renaming = ui.data(|d| d.get_temp::<String>(draft_id));
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.set_min_height(34.0);
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.label(theme::ic(sound_icon(&self.settings.audio_sources[i]), 18.0).color(pal.text2));
            let name_w = (ui.available_width() - 90.0).max(60.0);
            ui.allocate_ui_with_layout(egui::vec2(name_w, 30.0), egui::Layout::left_to_right(egui::Align::Center), |ui| match renaming {
                Some(mut draft) => {
                    if name_field(ui, rename_id, &mut draft, theme::font(16.0, W::Semibold), name_w - 8.0) {
                        self.settings.audio_sources[i].name = draft;
                        ui.data_mut(|d| d.remove::<String>(draft_id));
                    } else {
                        ui.data_mut(|d| d.insert_temp(draft_id, draft));
                    }
                }
                None => {
                    forget_name_field(ui, rename_id);
                    let name = self.settings.audio_sources[i].name.clone();
                    let r = ui.add(egui::Label::new(theme::text(&name, 16.0, W::Semibold).color(pal.text)).truncate().sense(Sense::click()));
                    if r.on_hover_text("Click to rename it").on_hover_cursor(egui::CursorIcon::Text).clicked() {
                        ui.data_mut(|d| d.insert_temp(draft_id, name.clone()));
                    }
                }
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                let more = Btn::icon(Kind::Ghost, icon::DOTS_THREE).small().tip("Move, mute, turn off, remove").show(ui);
                crate::ui_kit::menu_popup(&more, false, 220.0, |ui| {
                    if let Some(a) = self.sound_menu(ui, i) {
                        action = Some(a);
                    }
                });
                let source = &mut self.settings.audio_sources[i];
                let mut on = source.enabled;
                let sw = crate::ui_kit::switch(ui, &mut on).on_hover_text(if on { "On: off keeps its settings, and records nothing from it" } else { "Off: on records it again" });
                if sw.changed() {
                    if fixed.is_some() {
                        action = Some(CardAction::Locked);
                    } else {
                        source.enabled = on;
                    }
                }
            });
        });
        let source = &self.settings.audio_sources[i];
        let what = match &source.kind {
            SourceKind::Desktop { exclude_apps: true } if has_app_sources => "Everything except apps added as their own source".to_owned(),
            SourceKind::Desktop { .. } => "Everything your computer plays".to_owned(),
            SourceKind::App { app_name, .. } => format!("Only {app_name}"),
            SourceKind::Microphone { .. } => "Microphone".to_owned(),
        };
        ui.horizontal(|ui| {
            ui.add_space(26.0);
            ui.label(theme::text(what, 12.5, W::Regular).color(pal.text3));
        });
        if let Some((t, c)) = status_text(channel.status()).filter(|_| source.enabled) {
            ui.horizontal(|ui| {
                ui.add_space(26.0);
                ui.add(egui::Label::new(theme::text(t, 12.5, W::Regular).color(c(pal))).wrap());
            });
        }
        if let Some((n, leaves_out)) = &note {
            ui.horizontal(|ui| {
                ui.add_space(26.0);
                ui.add(egui::Label::new(theme::text(n, 12.5, W::Regular).color(if *leaves_out { pal.warn } else { pal.text3 })).wrap());
            });
        }
        // Its volume: the level and volume as one control, its value, mute.
        crate::ui_kit::section_head(ui, "Volume", true, |_| {});
        crate::ui_kit::list(ui, ("sound_volume", id), |l| {
            egui::Frame::new().inner_margin(egui::Margin::symmetric(14, 12)).show(l.ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    let source = &mut self.settings.audio_sources[i];
                    let w = (ui.available_width() - 66.0 - 26.0 - 16.0).max(60.0);
                    let state = if source.muted { &meters.input } else { &meters.output };
                    let fresp = fader(ui, &mut source.volume_db, state, source.muted, w, &format!("insp_{}", source.id));
                    volume_value(ui, &mut source.volume_db, fresp.secondary_clicked(), &format!("insp_{}", source.id));
                    mute_button(ui, &mut source.muted);
                });
            });
        });
        // What it takes in, and where it goes.
        crate::ui_kit::section_head(ui, "Source", false, |_| {});
        crate::ui_kit::list(ui, ("sound_source", id), |l| {
            let audio = self.audio.clone();
            match self.settings.audio_sources[i].kind.clone() {
                SourceKind::Microphone { .. } => {
                    let (a, _) = l.row(Row::new("Microphone"), |ui| mic_picker(ui, &mut self.settings.audio_sources[i], &audio, picker_locked.is_some()));
                    if let Some(a) = a {
                        action = Some(a);
                    }
                }
                SourceKind::Desktop { exclude_apps } => {
                    let note = if has_app_sources { "Apps added as their own source aren't heard here too" } else { "Once you add an app on its own" };
                    let row = Row::new("Leave out apps added on their own").note(note).locked(locked.clone().filter(|_| has_app_sources));
                    let (_, clicked_locked) = l.row(row, |ui| {
                        let mut on = exclude_apps;
                        if crate::ui_kit::switch(ui, &mut on).changed()
                            && let SourceKind::Desktop { exclude_apps } = &mut self.settings.audio_sources[i].kind
                        {
                            *exclude_apps = on;
                        }
                    });
                    if clicked_locked {
                        action = Some(CardAction::Locked);
                    }
                }
                SourceKind::App { app_name, bundle_id } => {
                    l.row(Row::new("App").note(bundle_id), |ui| {
                        ui.label(theme::text(app_name, 13.0, W::Regular).color(pal.text2));
                    });
                }
            }
            let route = Route::of(&self.settings.audio_sources[i]);
            let row = Row::new("Goes into").tip("Where it goes", "Clip sound: part of what everyone hears when you play or share a clip. Own track: also kept on its own, so you can change its volume later in the editor.").locked(locked.clone());
            let (_, clicked_locked) = l.row(row, |ui| {
                let r = crate::ui_kit::select(ui, route.label(), 190.0);
                crate::ui_kit::menu_popup(&r, false, 260.0, |ui| {
                    for r in [Route::Both, Route::Mix, Route::Track] {
                        if Item::new(r.label()).check(route == r).tip(r.tip()).show(ui).clicked() {
                            let s = &mut self.settings.audio_sources[i];
                            (s.in_mix, s.own_track) = match r {
                                Route::Both => (true, true),
                                Route::Mix => (true, false),
                                Route::Track => (false, true),
                            };
                        }
                    }
                });
            });
            if clicked_locked {
                action = Some(CardAction::Locked);
            }
        });
        // A microphone's clean-up, and hearing it.
        if matches!(self.settings.audio_sources[i].kind, SourceKind::Microphone { .. }) {
            use crate::settings::NoiseReduction;
            use crate::ui_kit::Seg;
            crate::ui_kit::section_head(ui, "Microphone", false, |_| {});
            crate::ui_kit::list(ui, ("sound_mic", id), |l| {
                const MODES: [NoiseReduction; 3] = [NoiseReduction::Off, NoiseReduction::Light, NoiseReduction::Strong];
                let opts = [
                    Seg::from("Off"),
                    Seg { icon: None, label: "Light", tip: Some("Takes out steady noise: fans, hum, hiss. Your keyboard stays") },
                    Seg { icon: None, label: "Strong", tip: Some("Takes out everything but your voice") },
                ];
                let row = Row::new("Noise reduction").tip("Noise reduction", "Light takes out steady noise behind your voice, like a fan, hum or hiss, and keeps sounds that come and go, like your keyboard. Strong takes out everything but your voice. Changes apply straight away.");
                let chosen = MODES.iter().position(|m| *m == self.settings.audio_sources[i].noise_reduction).unwrap_or(0);
                let (picked, _) = l.row(row, |ui| crate::ui_kit::segmented_opts(ui, ("noise_reduction", id), &opts, chosen, None, true));
                if let Some(k) = picked {
                    self.settings.audio_sources[i].noise_reduction = MODES[k];
                }
                let row = Row::new("Mono").tip("Mono", "For a mic heard on one side only, like one in an audio interface's first input (its left): you're heard in the middle, on both sides, as loud as before.");
                l.row(row, |ui| {
                    crate::ui_kit::switch(ui, &mut self.settings.audio_sources[i].mono);
                });
                let note = match listening {
                    Some(latency) => format!("Listening{}. Use headphones, or it feeds back.", latency.map_or(String::new(), |ms| format!(", {ms:.0} ms behind"))),
                    None => "Hear it through your headphones, to check it works and how it sounds".to_owned(),
                };
                let row = Row::new("Listen").note(note).warn(listening.is_some());
                l.row(row, |ui| {
                    let on = listening.is_some();
                    let btn = if on { Btn::new(Kind::On, "Listening") } else { Btn::normal("Listen") };
                    if btn.with_icon(icon::HEADPHONES).small().id(("listen", id)).show(ui).clicked() {
                        action = Some(CardAction::Listen);
                    }
                });
            });
        }
        if let Some(a) = action {
            self.sound_action(i, a);
        }
    }

    /// The clip's mix, in its dialog: its level with its scale and peak,
    /// and the limiter, with how much it's turning the mix down.
    pub(crate) fn mix_inspector(&mut self, ui: &mut egui::Ui) {
        use crate::theme::{self, W};
        use crate::ui_kit::Row;
        let pal = theme::pal(ui);
        let now = Instant::now();
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.set_min_height(34.0);
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.label(theme::ic(egui_phosphor::regular::WAVEFORM, 18.0).color(pal.text2));
            ui.label(theme::text("Clip mix", 16.0, W::Semibold).color(pal.text));
        });
        ui.horizontal(|ui| {
            ui.add_space(26.0);
            ui.add(egui::Label::new(theme::text("Every source in the clip's sound, mixed: what people hear when you share a clip.", 12.5, W::Regular).color(pal.text3)).wrap());
        });
        let view = &self.sources_view;
        let limiting = self.settings.limiter && view.limiting_at.is_some_and(|t| now - t < LIMIT_HOLD);
        crate::ui_kit::section_head(ui, "Level", true, |ui| {
            let readout = view.master.readout().replace('-', "−");
            ui.label(theme::text(format!("peak {readout} dB"), 12.5, W::Regular).color(view.master.readout_color(ui.visuals())));
        });
        crate::ui_kit::list(ui, "mix_level", |l| {
            egui::Frame::new().inner_margin(egui::Margin::symmetric(14, 12)).show(l.ui, |ui| {
                let style = meter::Style { bar_h: 7.0, gap: 3.0, scale: true, dimmed: false };
                meter::stereo(ui, egui::Id::new("master_meter"), &view.master, &style);
                ui.add_space(6.0);
                ui.add(egui::Label::new(theme::text("Aim for speech peaking around −12 dB, out of the red.", 12.0, W::Regular).color(pal.text3)).wrap());
            });
        });
        crate::ui_kit::section_head(ui, "Limiter", false, |_| {});
        let reduction = view.reduction_db;
        let clipped = view.master.clipped();
        crate::ui_kit::list(ui, "mix_limiter", |l| {
            let note = if clipped {
                "Too loud: it distorted. Turn some sources down."
            } else if limiting {
                "Working: turn sources down a little for the cleanest sound."
            } else {
                "When sources add up too loud, the mix is turned down for a moment instead of distorting. Ceiling −1 dB."
            };
            l.row(Row::new("Limiter").note(note).warn(clipped || limiting), |ui| {
                crate::ui_kit::switch(ui, &mut self.settings.limiter);
            });
            if self.settings.limiter {
                l.row(Row::new("Turning down"), |ui| {
                    let gr = if reduction >= 0.1 { format!("−{reduction:.1} dB") } else { "0.0 dB".to_owned() };
                    ui.label(theme::text(gr, 12.5, W::Medium).color(pal.text2));
                    meter::reduction(ui, reduction, 80.0).on_hover_text("How much the limiter is turning the mix down right now (0–12 dB)");
                });
            }
        });
    }

    /// "Listen" on source `i`: hear it live, or stop.
    fn toggle_listening(&mut self, i: usize) {
        let source = &self.settings.audio_sources[i];
        if self.listening.as_ref().is_some_and(|l| l.source == source.id) {
            self.listening = None;
            return;
        }
        self.listening = None;
        if let Some(err) = self.start_listening(i) {
            self.toast_error(format!("Couldn't play the microphone: {err}"));
        }
    }

    fn start_listening(&mut self, i: usize) -> Option<String> {
        let source = &self.settings.audio_sources[i];
        let SourceKind::Microphone { device } = &source.kind else { return None };
        let Some(device) = self.audio.resolve_input(device) else { return Some("no microphone".into()) };
        match capture::monitor::MicMonitor::start(&device, self.live_audio.channel(&source.id)) {
            Ok(monitor) => {
                self.listening = Some(Listening { source: source.id.clone(), device, monitor });
                None
            }
            Err(e) => Some(e.to_string()),
        }
    }

    /// Stop listening to a microphone that's gone, off or removed; follow it
    /// to another device.
    fn keep_listening(&mut self) {
        let Some(l) = &self.listening else { return };
        let found = self.settings.audio_sources.iter().position(|s| s.id == l.source && s.enabled);
        let Some(i) = found else {
            self.listening = None;
            return;
        };
        let SourceKind::Microphone { device } = &self.settings.audio_sources[i].kind else { return };
        if self.audio.resolve_input(device).as_deref() != Some(l.device.as_str()) {
            self.listening = None;
            if let Some(err) = self.start_listening(i) {
                self.toast_error(format!("Couldn't play the microphone: {err}"));
            }
        }
    }

    /// Why something that's set when capture starts is locked now.
    pub(crate) fn stop_first(&self) -> String {
        format!("Stop {} to change this.", if self.rec_state.idle() { "clipping".to_owned() } else { self.rec_state.words() })
    }

    /// What a source added while capturing doesn't have yet, said on its
    /// card, and whether that leaves something out (a warning). The file's
    /// tracks are set when the capture starts: a source added since is in
    /// the clip's sound at once, with no track of its own, and in the track
    /// of the rest of it if there is one. Without, the editor, which makes
    /// the clip's sound again from the tracks, leaves it out. None for a
    /// source the capture started with (its own track goes on, though it's
    /// turned off and on), one it doesn't have, or one that's all there (as
    /// it is while only streaming, once there's sound: a stream sends the
    /// clip's sound alone).
    fn added_note(&self, source: &AudioSourceCfg) -> Option<(String, bool)> {
        let has = |list: &[capture::sources::AudioSource]| list.iter().any(|s| s.id == source.id);
        let added = capture::SOURCES_WHILE_CAPTURING && !self.rec_state.idle() && has(&self.capturing_sources) && !has(&self.started_sources);
        if !added {
            return None;
        }
        added_source_note(self.rec_state, &self.started_sources, source)
    }

    /// The video's size and frame rate, as recorded, over the preview: a
    /// click opens their dialog, so the video can change shape while it's
    /// arranged.
    fn size_button(&mut self, ui: &mut egui::Ui, text: &str) {
        use crate::theme::{self, W};
        let pal = theme::pal(ui);
        let locked = !self.rec_state.idle();
        let galley = ui.painter().layout_no_wrap(text.to_owned(), theme::font(12.5, W::Regular), pal.text3);
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(galley.size().x + if locked { 30.0 } else { 16.0 }, 24.0), Sense::click());
        crate::demo::mark(ui.ctx(), || "button:Video size".into(), rect);
        let hv = theme::toggle(ui.ctx(), resp.id.with("hv"), resp.hovered(), theme::FAST);
        if hv > 0.0 {
            ui.painter().rect_filled(rect, crate::ui_kit::RADIUS, theme::fade(pal.control, hv));
        }
        let ink = theme::mix(pal.text3, pal.text, hv);
        ui.painter().galley_with_override_text_color(egui::pos2(rect.left() + 8.0, rect.center().y - galley.size().y / 2.0), galley, ink);
        if locked {
            let mark = egui::pos2(rect.right() - 11.0, rect.center().y);
            ui.painter().text(mark, egui::Align2::CENTER_CENTER, egui_phosphor::regular::LOCK_SIMPLE, egui::FontId::proportional(12.0), ink);
        }
        let tip = if locked { format!("Locked while {}: the video's size is set when the capture starts", self.rec_state.words()) } else { "The video's size and frame rate: click to change them".to_owned() };
        if resp.on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
            self.sources_view.editing = Some(crate::inspector_ui::Editing::Video);
        }
    }

    /// The screen's cropped and fills the video, at another size than the
    /// crop's: a note, and a button to make the video the crop's size (every
    /// pixel of it as it is). In the video's dialog, under its size.
    pub(crate) fn crop_match(&mut self, ui: &mut egui::Ui) {
        use crate::theme::{self, W};
        let pal = theme::pal(ui);
        let video = capture::frame_size(&self.video_source(), self.settings.video_size());
        let Some((w, h)) = self.crop_size().filter(|m| Some(*m) != video) else { return };
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label(theme::text(format!("Your screen's cropped to {w}×{h}"), 12.5, W::Regular).color(pal.text2));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let b = crate::ui_kit::Btn::normal("Match the crop").small().tip(format!("Make your video {w}×{h}, the crop's size: every pixel of it as it is"));
                if b.locked((!self.rec_state.idle()).then(|| self.stop_first())).show(ui).clicked() {
                    self.settings.resolution = crate::settings::OutputResolution::Exact(w, h);
                }
            });
        });
    }

    /// The size a video would be to show the screen's crop as it is (even,
    /// as big as a video can be; turned, as it shows), when it's cropped and
    /// fills the video.
    fn crop_size(&self) -> Option<(u32, u32)> {
        let (sw, sh) = self.crop_whole()?;
        let crop = self.settings.screen_crop.filter(|c| !c.is_none() && self.settings.screen.placed.is_none())?;
        let [_, _, w, h] = capture::Crop::from(crop).rect(sw, sh);
        let (w, h) = crate::canvas_ui::Turned::of(&self.settings.screen).size((w, h));
        ((w, h) != crate::canvas_ui::Turned::of(&self.settings.screen).size((sw & !1, sh & !1))).then(|| capture::output_size(w, h, None))
    }

    /// The display's own size, uncropped, when it's known.
    pub(crate) fn crop_whole(&self) -> Option<(u32, u32)> {
        match self.video_source() {
            source @ capture::VideoSource::Screen { .. } => capture::frame_size(&source, Default::default()),
            capture::VideoSource::Apps { .. } => None,
        }
    }

    /// The crop's page, in Sources' place: all of the screen with what's
    /// kept drawn on it to drag, and its size and place as numbers. It starts
    /// centred, and stays so as its size changes, until it's put somewhere.
    /// It's only what's shown of the screen, so it changes while clipping
    /// too; the video's size can be made the crop's, for every pixel of it.
    fn screen_crop_page(&mut self, ui: &mut egui::Ui) {
        use crate::settings::ScreenCrop;
        use crate::theme::{self, W};
        use crate::ui_kit::{Btn, Row};
        let pal = theme::pal(ui);
        // Back to Sources: the arrow, or Esc.
        let back_key = !crate::ui_kit::overlay_open(ui.ctx()) && !ui.ctx().egui_wants_keyboard_input() && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
        if back_key {
            self.sources_view.crop_open = false;
            return;
        }
        // Its size's link keeps the shape as its edges are dragged too.
        let keep = crate::ui_kit::size_field_keeps(ui.ctx(), "screen_crop").is_some();
        let mut back = false;
        self.page_head(ui, "", |_, ui| {
            back = Btn::icon(crate::ui_kit::Kind::Ghost, egui_phosphor::regular::CARET_LEFT).tip("Back to Sources  (Esc)").show(ui).clicked();
            ui.label(theme::text("Crop", 20.0, W::Semibold).color(pal.text));
        });
        if back {
            self.sources_view.crop_open = false;
            return;
        }
        capture::preview::request();
        let frame = capture::preview::latest();
        self.update_preview_texture(ui.ctx(), frame.as_deref());
        crate::ui_kit::repaint_after(ui.ctx(), Duration::from_millis(33));
        let Some((sw, sh)) = self.crop_whole() else {
            ui.label(theme::text("The display's size isn't known yet.", 13.0, W::Regular).color(pal.text3));
            return;
        };
        // None set: all of the screen, centred.
        let crop = self.settings.screen_crop.filter(|c| !c.is_none()).unwrap_or(ScreenCrop { w: sw, h: sh, at: None });
        let [x, y, w, h] = capture::Crop::from(crop).rect(sw, sh);
        let centred = crop.at.is_none();
        let mut set: Option<ScreenCrop> = None;
        // What the video does with it, when it fills the video: the video's
        // size, the crop's (as a video can be), and why that can't change now.
        let video = capture::frame_size(&self.video_source(), self.settings.video_size());
        // The crop as it shows in the video: turned, if the screen is.
        let (tw, th) = crate::canvas_ui::Turned::of(&self.settings.screen).size((w, h));
        let matched = capture::output_size(tw, th, None);
        let filling = self.settings.screen.placed.is_none() && (w, h) != (sw & !1, sh & !1);
        let size_lock = (!self.rec_state.idle()).then(|| format!("Stop {} to change your video's size: it's set when the capture starts", self.rec_state.words()));
        let mut match_crop = false;
        // The least kept each way.
        let least = 64u32;

        let picture = |ui: &mut egui::Ui, this: &mut Self, max_h: f32, set: &mut Option<ScreenCrop>| {
            let aspect = sw as f32 / sh.max(1) as f32;
            let pw = ui.available_width().min(max_h * aspect);
            ui.vertical_centered(|ui| {
                let (rect, resp) = ui.allocate_exact_size(egui::vec2(pw, pw / aspect), Sense::click_and_drag());
                let p = ui.painter();
                p.rect_filled(rect, 6.0, pal.stage);
                // Only a picture of the whole screen is cropped on (one of
                // the video shows for a moment as the preview starts over).
                let whole = frame.as_ref().is_some_and(|f| (f.recorded.0 as f32 / f.recorded.1.max(1) as f32 - aspect).abs() < 0.01);
                if !(whole && this.paint_preview(p, rect)) {
                    let text = match this.video_preview.as_ref().and_then(|(_, v)| v.error()) {
                        Some(e) => format!("No preview: {e}"),
                        None => "Starting preview…".to_owned(),
                    };
                    p.text(rect.center(), egui::Align2::CENTER_CENTER, text, egui::FontId::proportional(13.0), Color32::from_gray(170));
                }
                let bounds = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(sw as f32, sh as f32));
                let kept = egui::Rect::from_min_size(egui::pos2(x as f32, y as f32), egui::vec2(w as f32, h as f32));
                // Dragged: where it's put (no longer centred).
                if let Some(k) = crate::webcam_ui::crop_editor(ui, &resp, rect, bounds, kept, egui::vec2(least as f32, least as f32), keep, &mut this.sources_view.crop_drag) {
                    let r = |v: f32| v.round().max(0.0) as u32;
                    *set = Some(ScreenCrop { w: r(k.width()), h: r(k.height()), at: Some([r(k.min.x), r(k.min.y)]) });
                }
            });
            ui.add_space(8.0);
            ui.vertical_centered(|ui| {
                let hint = if keep {
                    "Drag the edges or corners (the shape's kept), or the middle to move it."
                } else {
                    "Drag the edges or corners (Shift keeps the shape), or the middle to move it."
                };
                ui.label(theme::text(hint, 12.5, W::Regular).color(pal.text3));
            });
        };
        let mut locked_click = false;
        let numbers = |ui: &mut egui::Ui, set: &mut Option<ScreenCrop>, locked_click: &mut bool, match_crop: &mut bool| {
            let small = |ui: &mut egui::Ui, t: &str| ui.label(theme::text(t, 12.5, W::Regular).color(pal.text3));
            // Laid out left to right at the row's end, so Tab goes through
            // them in reading order: `width` across, made by `add`.
            let boxes = |ui: &mut egui::Ui, width: f32, add: &mut dyn FnMut(&mut egui::Ui)| {
                ui.allocate_ui_with_layout(egui::vec2(width, crate::ui_kit::CONTROL_H), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    add(ui);
                });
            };
            const BOX: f32 = 64.0;
            crate::ui_kit::list(ui, "screen_crop", |l| {
                // Its shape: the biggest of it there's room for, where it is,
                // and kept as it's sized (it's the size's link).
                let (_, c) = l.row(Row::new("Aspect ratio"), |ui| {
                    let now = crate::ui_kit::size_field_keeps(ui.ctx(), "screen_crop").map(crate::ui_kit::ratio_of);
                    let from = crate::ui_kit::ratio_of(w as f64 / h.max(1) as f64);
                    match crate::ui_kit::ratio_field(ui, "screen_crop_ratio", ("Any", ""), now, from) {
                        Some(None) => crate::ui_kit::set_size_field_keeps(ui.ctx(), "screen_crop", None),
                        Some(Some([aw, ah])) => {
                            crate::ui_kit::set_size_field_keeps(ui.ctx(), "screen_crop", Some(aw as f64 / ah as f64));
                            let centre = (x as f64 + w as f64 / 2.0, y as f64 + h as f64 / 2.0);
                            // All of the display, when it's that shape.
                            *set = Some(match media::Crop::largest(sw, sh, aw, ah, centre).within(sw, sh) {
                                Some(c) => ScreenCrop { w: c.w, h: c.h, at: (!centred).then_some([c.x, c.y]) },
                                None => ScreenCrop::default(),
                            });
                        }
                        None => {}
                    }
                });
                *locked_click |= c;
                // Its size: the game's resolution. Centred, it stays in the
                // middle; else its top-left corner stays put. With the shape
                // kept, one follows the other.
                let (_, c) = l.row(Row::new("Size").note("The game's resolution, like 1600 × 900"), |ui| {
                    if let Some((w, h)) = crate::ui_kit::size_field(ui, "screen_crop", (w, h), least..=sw, least..=sh) {
                        *set = Some(ScreenCrop { w, h, ..crop });
                    }
                });
                *locked_click |= c;
                // Where it is: its top-left corner, or the middle.
                let note = if centred { "Centred on the screen".to_owned() } else { "Its top-left corner, from the screen's".to_owned() };
                let (_, c) = l.row(Row::new("Position").note(note), |ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    let b = Btn::normal("Centre").small().enabled(!centred).tip("Put it in the middle of the screen, and keep it there");
                    if b.show(ui).clicked() {
                        *set = Some(ScreenCrop { at: None, ..crop });
                    }
                    boxes(ui, BOX * 2.0 + 6.0 + 16.0 + 6.0 * 3.0, &mut |ui| {
                        if let Some(v) = crate::ui_kit::number_box(ui, egui::Id::new("screen_crop_x"), x, 0..=sw - w, BOX) {
                            *set = Some(ScreenCrop { w, h, at: Some([v, y]) });
                        }
                        small(ui, ",");
                        if let Some(v) = crate::ui_kit::number_box(ui, egui::Id::new("screen_crop_y"), y, 0..=sh - h, BOX) {
                            *set = Some(ScreenCrop { w, h, at: Some([x, v]) });
                        }
                        small(ui, "px");
                    });
                });
                *locked_click |= c;
                // The video: as big as the crop, every pixel as it is; else
                // what it does to it, and a way to make it the crop's size.
                if let (true, Some((vw, vh))) = (filling, video) {
                    let note = if (vw, vh) == matched {
                        "The crop's size: every pixel as it is".to_owned()
                    } else {
                        let (crop_shape, video_shape) = (tw as f32 / th.max(1) as f32, vw as f32 / vh.max(1) as f32);
                        let scale = vh as f32 / capture::Shape { w: vw, h: vh, framing: capture::Framing::Fill }.of(tw, th).1.max(1) as f32;
                        let does = if (crop_shape / video_shape - 1.0).abs() > 0.01 {
                            if crop_shape > video_shape { "the crop's sides are cut off to fit it" } else { "the crop's top and bottom are cut off to fit it" }
                        } else if scale > 1.01 {
                            "the crop is made bigger to fill it, a little softer"
                        } else if scale < 0.99 {
                            "the crop is made smaller to fit it"
                        } else {
                            "about the crop's size"
                        };
                        format!("{vw}×{vh}: {does}")
                    };
                    let lock = size_lock.clone().filter(|_| (vw, vh) != matched);
                    let (_, c) = l.row(Row::new("Video size").note(note).locked(lock), |ui| {
                        if (vw, vh) != matched {
                            let (mw, mh) = matched;
                            let b = Btn::normal("Match the crop").small().tip(format!("Make your video {mw}×{mh}, the crop's size: every pixel of it as it is"));
                            *match_crop = b.show(ui).clicked();
                        } else {
                            ui.label(theme::ic(egui_phosphor::regular::CHECK, 14.0).color(pal.text3));
                        }
                    });
                    *locked_click |= c;
                }
            });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.add_space(2.0);
                let what = if (w, h) == (sw, sh) { format!("All of the screen, {sw}×{sh}") } else { format!("{w}×{h} of {sw}×{sh}") };
                ui.label(theme::text(what, 12.5, W::Regular).color(pal.text3));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if (w, h) != (sw, sh) && Btn::ghost("Reset").small().tip("All of the screen again").show(ui).clicked() {
                        *set = Some(ScreenCrop::default());
                    }
                });
            });
        };

        ui.add_space(4.0);
        let avail_w = ui.available_width();
        let avail_h = ui.available_height();
        if avail_w >= TWO_COLUMNS {
            let col = (avail_w - 28.0) / 2.0;
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 28.0;
                ui.allocate_ui_with_layout(egui::vec2(col, avail_h), egui::Layout::top_down(egui::Align::Min), |ui| {
                    ui.set_width(col);
                    ui.add_space(4.0);
                    picture(ui, self, avail_h * 0.7, &mut set);
                });
                ui.allocate_ui_with_layout(egui::vec2(col, avail_h), egui::Layout::top_down(egui::Align::Min), |ui| {
                    ui.set_width(col);
                    numbers(ui, &mut set, &mut locked_click, &mut match_crop);
                });
            });
        } else {
            crate::ui_kit::scroll(ui, "screen_crop_page", |ui| {
                picture(ui, self, avail_h * 0.4, &mut set);
                ui.add_space(16.0);
                numbers(ui, &mut set, &mut locked_click, &mut match_crop);
            });
        }
        if locked_click && let Some(why) = &size_lock {
            self.toast_icon(egui_phosphor::regular::LOCK_SIMPLE, why.clone());
        }
        if match_crop && size_lock.is_none() {
            self.settings.resolution = crate::settings::OutputResolution::Exact(matched.0, matched.1);
        }
        if let Some(c) = set {
            let all = c.is_none() || capture::Crop::from(c).rect(sw, sh) == [0, 0, sw & !1, sh & !1];
            self.settings.screen_crop = (!all).then_some(c);
        }
    }

    fn update_preview_texture(&mut self, ctx: &egui::Context, frame: Option<&capture::preview::PreviewFrame>) {
        let Some(f) = frame else { return };
        if self.sources_view.preview.as_ref().is_some_and(|(_, seq)| *seq == f.seq) {
            return;
        }
        #[cfg(target_os = "macos")]
        if self.sources_view.preview_gpu.as_ref().is_some_and(|(_, seq)| *seq == f.seq) {
            return;
        }
        self.count_preview_frame();
        // macOS: the frame itself, on the GPU, drawn as it is.
        #[cfg(target_os = "macos")]
        if let Some(s) = &f.surface {
            if let Some(shown) = self.sources_view.preview_frames.show(s, f.width, f.height) {
                self.sources_view.preview_gpu = Some((shown, f.seq));
            }
            return;
        }
        if f.rgba.is_empty() {
            return;
        }
        // Opaque, so it's already premultiplied: one copy, no per-pixel work.
        let pixels: Vec<egui::Color32> = bytemuck::cast_slice(&f.rgba).to_vec();
        let image = egui::ColorImage::new([f.width as usize, f.height as usize], pixels);
        match &mut self.sources_view.preview {
            Some((tex, seq)) => {
                tex.set(image, egui::TextureOptions::LINEAR);
                *seq = f.seq;
            }
            None => self.sources_view.preview = Some((ctx.load_texture("capture_preview", image, egui::TextureOptions::LINEAR), f.seq)),
        }
    }

    /// Draw the preview picture held (macOS: the frame on the GPU; else the
    /// texture) into `rect`. False if there's none yet.
    pub(crate) fn paint_preview(&self, p: &egui::Painter, rect: egui::Rect) -> bool {
        #[cfg(target_os = "macos")]
        if let Some((shown, _)) = &self.sources_view.preview_gpu {
            shown.paint(p, rect);
            return true;
        }
        match &self.sources_view.preview {
            Some((tex, _)) => {
                p.image(tex.id(), rect, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
                true
            }
            None => false,
        }
    }

    /// The size of the preview picture held, in pixels.
    pub(crate) fn preview_size(&self) -> Option<(u32, u32)> {
        #[cfg(target_os = "macos")]
        if let Some((shown, _)) = &self.sources_view.preview_gpu {
            return Some(shown.size());
        }
        self.sources_view.preview.as_ref().map(|(t, _)| t.size()).map(|[w, h]| (w as u32, h as u32))
    }

    /// Count a new preview frame, for the rate shown.
    fn count_preview_frame(&mut self) {
        let (count, since, rate) = &mut self.sources_view.preview_rate;
        *count += 1;
        match since {
            Some(t) if t.elapsed() >= Duration::from_secs(1) => {
                *rate = *count;
                *count = 0;
                *since = Some(Instant::now());
            }
            None => *since = Some(Instant::now()),
            _ => {}
        }
    }

    /// The preview itself: as wide as there's room for, at most `max_h`
    /// tall, the sources placed on it (see `canvas_ui`).
    fn preview_picture(&mut self, ui: &mut egui::Ui, frame: Option<&capture::preview::PreviewFrame>, max_h: f32) {
        // The frame's shape; between captures (none has a frame for a moment,
        // starting to clip) the shape of the picture still shown, so it isn't
        // drawn stretched.
        let shown = self.preview_size().map(|(w, h)| w as f32 / h.max(1) as f32);
        let aspect = frame.map(|f| f.width as f32 / f.height.max(1) as f32).or(shown).unwrap_or(16.0 / 9.0);
        let w = ui.available_width().min(max_h.max(160.0) * aspect);
        ui.vertical_centered(|ui| {
            let (rect, response) = ui.allocate_exact_size(egui::vec2(w, w / aspect), Sense::click_and_drag());
            crate::demo::mark(ui.ctx(), || "preview".into(), rect);
            let p = ui.painter();
            p.rect_filled(rect, 6.0, crate::theme::pal(ui).stage);
            let message = |text: &str| {
                p.text(rect.center(), egui::Align2::CENTER_CENTER, text, egui::FontId::proportional(13.0), Color32::from_gray(170));
            };
            let drawn = self.paint_preview(p, rect);
            // While a new preview starts (another frame rate, size, webcam)
            // the last picture stays up; the message is only for when there's
            // none to show. With every source off, the video is black: said.
            let any_on = self.layer_order().into_iter().any(|l| self.layer_enabled(l));
            match self.video_preview.as_ref().and_then(|(_, p)| p.error()) {
                Some(e) => message(&format!("No preview: {e}")),
                None if !drawn => message("Starting preview…"),
                None if !any_on => message("Nothing's in your video: show a source in the list below"),
                None => {}
            }
            if frame.is_some() {
                self.canvas_on_preview(ui, rect, &response);
            }
        });
    }

    /// The games and apps to record, in the screen's card: each with whether
    /// it's open (and which is being recorded) and a remove button, then a
    /// way to add more, and the Tabbed out switch. Changes apply right away,
    /// even while capturing.
    pub(crate) fn app_rows(&mut self, ui: &mut egui::Ui, apps: Vec<CaptureApp>, away_screen: bool, frame: Option<&capture::preview::PreviewFrame>) {
        use crate::theme::{self, W};
        use crate::ui_kit::{Btn, Item, Kind};
        use egui_phosphor::regular as icon;
        let pal = theme::pal(ui);
        let showing = frame.filter(|f| !f.waiting).and_then(|f| f.app.clone());
        let capturing = !self.rec_state.idle();
        let mut list = apps.clone();
        let mut away = away_screen;
        let mut remove = None;
        {
            if list.is_empty() {
                ui.add_space(10.0);
                ui.label(theme::text("No games or apps yet. Add the games you play.", 13.0, W::Regular).color(pal.text3));
                ui.add_space(8.0);
            }
            for (i, app) in list.iter().enumerate() {
                let open = self.windowed_apps.iter().any(|a| a.id.eq_ignore_ascii_case(&app.id));
                let active = showing.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(&app.id));
                let (dot, status) = if active {
                    (Some(pal.live), if capturing { "Recording now" } else { "In the preview" })
                } else if open {
                    (None, "Open")
                } else {
                    (None, "Not open")
                };
                ui.horizontal(|ui| {
                    ui.set_min_height(42.0);
                    ui.spacing_mut().item_spacing.x = 10.0;
                    // The game's picture, when it's one Discord knows.
                    let (lead, _) = ui.allocate_exact_size(egui::vec2(18.0, 18.0), Sense::hover());
                    let tex = crate::discord::game_for_folder(&crate::clips::folder_name(&app.name)).and_then(|g| g.icon).and_then(|url| self.web_images.icon(ui.ctx(), &url));
                    match tex {
                        Some(t) => {
                            egui::Image::from_texture((t.id(), lead.size())).corner_radius(4).paint_at(ui, lead);
                        }
                        // Else the app's own icon, else a symbol.
                        None if crate::app_icons::paint(ui, &mut self.app_icons, &app.id, lead.expand(1.0)) => {}
                        None => {
                            ui.painter().text(lead.center(), egui::Align2::CENTER_CENTER, icon::APP_WINDOW, egui::FontId::proportional(16.0), pal.text3);
                        }
                    }
                    ui.label(theme::text(&app.name, 13.5, W::Medium).color(pal.text)).on_hover_text(&app.id);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        if Btn::icon(Kind::Ghost, icon::X).small().tip(format!("Stop recording {}", app.name)).show(ui).clicked() {
                            remove = Some(i);
                        }
                        ui.label(theme::text(status, 12.5, W::Regular).color(pal.text3));
                        if let Some(c) = dot {
                            let (r, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), Sense::hover());
                            crate::shell::status_dot(ui.painter(), r.center(), c, ui.input(|i| i.time));
                        }
                    });
                });
                crate::ui_kit::divider(ui, 0.0);
            }
            ui.add_space(6.0);
            let add = Btn::ghost("Add game or app").with_icon(icon::PLUS).small().id("add_capture_app").show(ui);
            if add.clicked() {
                self.windowed_apps = capture::list_windowed_apps();
            }
            crate::ui_kit::menu_popup(&add, true, 260.0, |ui| {
                crate::ui_kit::menu_head(ui, "Open apps with a window");
                let mut any = false;
                egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                    for app in self.windowed_apps.clone() {
                        if list.iter().any(|a| a.id.eq_ignore_ascii_case(&app.id)) {
                            continue;
                        }
                        any = true;
                        if Item::new(app.name.clone()).tip(app.id.clone()).show(ui).clicked() {
                            list.push(CaptureApp { id: app.id, name: app.name });
                        }
                    }
                });
                if !any {
                    crate::ui_kit::menu_head(ui, "Nothing else is open. Start the game or app, then add it here.");
                }
            });
            ui.add_space(6.0);
        }
        if list.len() > 1 {
            crate::ui_kit::hint(ui, "Records the one you're using, and keeps it while you click into something else as long as it's on screen.");
            ui.add_space(6.0);
        }
        crate::ui_kit::divider(ui, 0.0);
        ui.horizontal(|ui| {
            ui.set_min_height(GROUP_ROW_H);
            ui.spacing_mut().item_spacing.x = 6.0;
            ui.label(theme::text("Show “Tabbed out” when you leave it", 13.5, W::Regular).color(pal.text));
            crate::ui_kit::info(ui, "Tabbed out", "When the game you were in stops showing (most games minimize when you alt-tab), or none of yours is open, clips show the HasteClip logo with “Tabbed out”. Off: they keep the game's last picture, or show black when none is open. A window that's still on screen keeps being recorded either way.");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                crate::ui_kit::switch(ui, &mut away);
            });
        });
        crate::ui_kit::divider(ui, 0.0);
        ui.horizontal(|ui| {
            ui.set_min_height(GROUP_ROW_H);
            ui.spacing_mut().item_spacing.x = 6.0;
            ui.label(theme::text("Record the whole time a game is open", 13.5, W::Regular).color(pal.text));
            crate::ui_kit::info(ui, "Whole sessions", "When a game or app on this list opens, HasteClip records until it closes, then saves the recording. Clipping waits meanwhile, and starts again after. While it records, Save clip and “HasteClip that” mark the moment in the recording. Stop the recording yourself and it waits until the game opens again.");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                crate::ui_kit::switch(ui, &mut self.settings.record_sessions);
            });
        });
        if let Some(i) = remove {
            let name = list.remove(i).name;
            self.toast_icon(icon::X_CIRCLE, format!("Stopped recording {name}"));
        }
        if list != apps || away != away_screen {
            self.settings.capture = CaptureTarget::Apps { apps: list, away_screen: away };
        }
    }

    /// Run a level-only capture while this page is open and nothing is recording,
    /// so the meters work before you start; stop it otherwise. Restarts when the
    /// sources change. Called every frame from the app loop.
    /// Like the meters' capture: a preview-only video capture while this page is
    /// open and nothing records (a running capture feeds the preview itself).
    /// Restarts when what to record changes.
    pub(crate) fn ensure_video_preview(&mut self) {
        // Off the page: the capture's buffers opened for drawing are let go
        // too (each texture keeps its whole frame alive). They were kept until
        // 64 had been opened: every visit's captures stayed, ~5 MB a visit at
        // 1080p. The last frame stays, to show at once when the page opens.
        #[cfg(target_os = "macos")]
        if !self.wants_sources_capture() && self.sources_view.preview_gpu.is_some() {
            self.sources_view.preview_gpu = None;
            self.sources_view.preview_frames = Default::default();
        }
        // While clipping, the capture shows what it records; the crop's page
        // has one of its own, of all of the screen (shown over it till it closes).
        let want = capture::preview::AVAILABLE && self.wants_sources_capture() && (self.rec_state.idle() || self.sources_view.crop_open);
        if !want {
            self.video_preview = None;
            return;
        }
        // The meters' capture first: started together, the first time in a
        // session, the two waited on each other five seconds (the meters
        // stood still all that time).
        if self.video_preview.is_none() && self.level_monitor.as_ref().is_some_and(|(_, m)| !m.ready()) {
            return;
        }
        // What a recording would be: same picture, size and frame rate. While
        // the crop's set: all of the screen as it is, to crop on.
        // Where every source is drawn, they're taken up live (`capture_layers`);
        // where one webcam is, another one starts the preview again.
        let webcam = if capture::layers::MANY { None } else { self.webcam_source() };
        let wanted = match self.video_source() {
            source @ capture::VideoSource::Screen { .. } if self.sources_view.crop_open => (source, Default::default(), self.settings.fps, None, true),
            source => (source, self.settings.video_size(), self.settings.fps, webcam.clone(), false),
        };
        let webcam = wanted.3.clone();
        // A changed app list reaches the running preview without a restart,
        // as does anything else its size. Another size (another display)
        // starts it again at that size: switched in place, it kept the old
        // display's size, the new one in black borders and the webcam drawn
        // in a frame of the wrong shape (until clipping started a new capture).
        if let Some((have, preview)) = &mut self.video_preview {
            if *have != wanted
                && (&have.1, have.2, &have.3, have.4) == (&wanted.1, wanted.2, &wanted.3, wanted.4)
                && capture::frame_size(&have.0, have.1) == capture::frame_size(&wanted.0, wanted.1)
                && preview.update(&wanted.0)
            {
                have.0 = wanted.0.clone();
            }
        }
        if self.video_preview.as_ref().is_none_or(|(w, _)| *w != wanted) {
            let (source, size, fps, _, whole) = &wanted;
            // All of the screen: placed on its own, filling the picture.
            let screen = if *whole { std::sync::Arc::new(std::sync::Mutex::new(capture::ScreenPlacement::whole(capture::Framing::Fit))) } else { self.screen_placement.clone() };
            // Both happen in the background: the old capture closes as the new
            // one opens, and only the new one's frames are shown.
            let layers = if *whole { capture::layers::main_only() } else { self.capture_layers.clone() };
            let preview = capture::preview::VideoPreview::start(source, *size, *fps, Some(self.away_screen.clone()), webcam, layers, screen);
            self.video_preview = Some((wanted, preview));
        }
    }

    pub(crate) fn ensure_level_monitor(&mut self) {
        let want = self.wants_sources_capture() && self.rec_state.idle();
        if !want {
            self.level_monitor = None;
            return;
        }
        let sources = self.capture_sources();
        let current = self.level_monitor.as_ref().map(|(s, _)| s);
        if current != Some(&sources) {
            self.level_monitor = None; // stop the old one before opening devices again
            self.level_monitor = Some((sources.clone(), Background::start(sources, self.live_audio.clone())));
        }
        if let Some(e) = self.level_monitor.as_ref().and_then(|(_, m)| m.take_error()) {
            self.toast_error(format!("Couldn't open audio: {e}"));
        }
    }

    /// The Sources page is open, or about to be (the pointer is on its place
    /// in the rail): its preview and meters start then, so they're running
    /// by the time it shows.
    pub(crate) fn wants_sources_capture(&self) -> bool {
        self.page == crate::Page::Sources || self.sources_warm.is_some_and(|t| t.elapsed() < Duration::from_secs(2))
    }
}

/// The meters' capture, opened and closed on a thread of its own: opening
/// every audio device took a third of a second, and the page froze for it
/// each time it opened.
pub(crate) struct Background {
    /// The monitor once open; `None` in it after it's let go of, so one that
    /// opens only then is closed again.
    monitor: std::sync::Arc<std::sync::Mutex<Option<Option<capture::sources::LevelMonitor>>>>,
    error: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    started: Instant,
}

impl Background {
    fn start(sources: Vec<capture::sources::AudioSource>, live: std::sync::Arc<capture::mixer::LiveAudio>) -> Self {
        let monitor = std::sync::Arc::new(std::sync::Mutex::new(Some(None)));
        let error = std::sync::Arc::new(std::sync::Mutex::new(None));
        let (m, e) = (monitor.clone(), error.clone());
        std::thread::spawn(move || match capture::sources::LevelMonitor::start(&sources, live) {
            Ok(started) => {
                let mut slot = m.lock().unwrap();
                match slot.as_mut() {
                    Some(open) => *open = Some(started),
                    // Let go of meanwhile: closed here, on this thread.
                    None => {
                        drop(slot);
                        drop(started);
                    }
                }
            }
            Err(err) => *e.lock().unwrap() = Some(format!("{err:#}")),
        });
        Self { monitor, error, started: Instant::now() }
    }

    /// Open (or failed, or taking very long): what waits for it may start.
    fn ready(&self) -> bool {
        self.started.elapsed() > Duration::from_secs(8) || self.error.lock().unwrap().is_some() || self.monitor.lock().unwrap().as_ref().is_some_and(|m| m.is_some())
    }

    fn take_error(&self) -> Option<String> {
        self.error.lock().unwrap().take()
    }
}

impl Drop for Background {
    /// Closed on a thread too (stopping waits for the devices).
    fn drop(&mut self) {
        let open = self.monitor.lock().unwrap().take().flatten();
        if let Some(open) = open {
            std::thread::spawn(move || drop(open));
        }
    }
}

enum CardAction {
    Remove,
    MoveUp,
    MoveDown,
    /// "Listen" on a microphone: hear it live, or stop.
    Listen,
    /// Something locked while capturing was clicked: say why.
    Locked,
    /// Its settings, in their dialog.
    Settings,
}

/// Hearing a microphone live: which source, from which device.
pub(crate) struct Listening {
    pub source: String,
    pub device: String,
    pub monitor: capture::monitor::MicMonitor,
}

/// Where a source goes, as one choice.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Route {
    Both,
    Mix,
    Track,
}

impl Route {
    fn of(s: &AudioSourceCfg) -> Self {
        match (s.in_mix, s.own_track) {
            (true, false) => Route::Mix,
            (false, true) => Route::Track,
            _ => Route::Both,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Route::Both => "Clip sound and own track",
            Route::Mix => "Clip sound only",
            Route::Track => "Own track only",
        }
    }
    fn tip(self) -> &'static str {
        match self {
            Route::Both => "Heard in the clip, and kept on its own to rebalance later",
            Route::Mix => "Heard in the clip, not kept separately",
            Route::Track => "Not heard in the clip until you turn it up in the editor",
        }
    }
}

/// A name edited in place: focused as it opens (once: taking the focus
/// back every frame left no way out of it). Enter or clicking away keeps
/// what's typed, trimmed; Esc, or leaving it empty, puts back what it was.
/// Returns whether editing is over.
pub(crate) fn name_field(ui: &mut egui::Ui, id: egui::Id, name: &mut String, font: egui::FontId, width: f32) -> bool {
    let was_id = id.with("was");
    let was = ui.data(|d| d.get_temp::<String>(was_id));
    let opening = was.is_none();
    if opening {
        ui.data_mut(|d| d.insert_temp(was_id, name.clone()));
    }
    // Free text: a name.
    #[allow(clippy::disallowed_methods)]
    let r = ui.add(egui::TextEdit::singleline(name).id(id).font(font).desired_width(width));
    if opening {
        r.request_focus();
        return false;
    }
    if !r.lost_focus() {
        return false;
    }
    let was = was.unwrap_or_default();
    if ui.input(|i| i.key_pressed(egui::Key::Escape)) || name.trim().is_empty() {
        *name = was;
    } else {
        *name = name.trim().to_owned();
    }
    forget_name_field(ui, id);
    true
}

/// A [`name_field`] that isn't open: the next one starts afresh.
pub(crate) fn forget_name_field(ui: &egui::Ui, id: egui::Id) {
    ui.data_mut(|d| d.remove::<String>(id.with("was")));
}

/// What `source`, added while `state` runs, doesn't have yet, on a capture
/// that started with the sources `started` (see [`App::added_note`]).
fn added_source_note(state: crate::RecState, started: &[capture::sources::AudioSource], source: &AudioSourceCfg) -> Option<(String, bool)> {
    // The capture starts again once all it does has stopped.
    let again = format!("you stop {} and start again", state.words());
    let (tracks, _, rest) = capture::sources::track_layout(started);
    if tracks.is_empty() {
        // Nothing made has a track for sound: "clips or the stream".
        let made: Vec<&str> = [(state.clipping, "clips"), (state.recording, "the recording"), (state.streaming, "the stream")].into_iter().filter(|(on, _)| *on).map(|(_, w)| w).collect();
        let made = match made.as_slice() {
            [first @ .., last] if !first.is_empty() => format!("{} or {last}", first.join(", ")),
            _ => made.concat(),
        };
        return Some((format!("Not in {made} until {again}: this started without sound."), true));
    }
    // Streaming alone writes no file, and a stream sends the mix: a source
    // added goes into it at once (and one left out of the mix never does).
    if !(state.clipping || state.recording) {
        return None;
    }
    if !source.in_mix {
        return Some((format!("Not recorded until {again}: its own track starts then."), true));
    }
    // In a track apart: the rest's, or there are no tracks apart.
    let kept = rest || tracks.len() == 1;
    match (source.own_track, kept) {
        (true, true) => Some((format!("In the clip's sound now. Its own track starts once {again}."), false)),
        (true, false) => Some((format!("In the clip's sound now. Its own track starts once {again}; until then, changing track volumes in the editor leaves it out."), true)),
        (false, true) => None,
        (false, false) => Some((format!("In the clip's sound now, but until {again}, changing track volumes in the editor leaves it out."), true)),
    }
}

/// A source's volume and level as one control: the meter runs along the
/// track (green, yellow from −18, red from −6), the knob sits at the volume,
/// and a tick marks 0 dB. Drag the knob, or anywhere on the track (Shift:
/// finer); double-click for 0 dB. A right-click on it types one (see
/// [`volume_value`]).
fn fader(ui: &mut egui::Ui, db: &mut f32, state: &MeterState, dimmed: bool, width: f32, id: &str) -> egui::Response {
    let pal = crate::theme::pal(ui);
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, 22.0), Sense::click_and_drag());
    let track = egui::Rect::from_min_max(egui::pos2(rect.left(), rect.top() + 7.0), egui::pos2(rect.right(), rect.top() + 15.0));
    let pos = |d: f32| ((d - FADER_MIN_DB) / (FADER_MAX_DB - FADER_MIN_DB)).clamp(0.0, 1.0);
    // The meter, on the same scale as the knob (dB along the fader).
    meter::fader_meter(ui.painter(), track, state, dimmed, |d| track.left() + track.width() * pos(d), pal);
    // 0 dB.
    let zero = track.left() + track.width() * pos(0.0);
    ui.painter().vline(zero, rect.top() + 4.0..=rect.bottom() - 4.0, egui::Stroke::new(1.0, crate::theme::fade(pal.text3, 0.5)));
    // Taken hold of, the knob goes to the pointer; then it follows it, a
    // fifth as far with Shift held, for fine steps. `raw`: where it's been
    // dragged to, past the ends too (so it comes back with the pointer).
    let raw_key = egui::Id::new(("fader_raw", id));
    let fine = ui.input(|i| i.modifiers.shift);
    let raw = if resp.drag_started() {
        resp.interact_pointer_pos().map(|p| FADER_MIN_DB + ((p.x - track.left()) / track.width()).clamp(0.0, 1.0) * (FADER_MAX_DB - FADER_MIN_DB))
    } else if resp.dragged() {
        let was = ui.data(|d| d.get_temp::<f32>(raw_key)).unwrap_or(*db);
        Some(was + resp.drag_delta().x / track.width() * (FADER_MAX_DB - FADER_MIN_DB) * if fine { 0.2 } else { 1.0 })
    } else {
        None
    };
    if let Some(raw) = raw {
        ui.data_mut(|d| d.insert_temp(raw_key, raw));
        let mut v = raw.clamp(FADER_MIN_DB, FADER_MAX_DB);
        // A small detent at 0 dB (not in fine steps, which go past it).
        if !fine && v.abs() < 0.6 {
            v = 0.0;
        }
        *db = (v * 10.0).round() / 10.0;
    }
    if resp.double_clicked() {
        *db = 0.0;
    }
    let hv = crate::theme::toggle(ui.ctx(), egui::Id::new(("fader_hv", id)), resp.hovered() || resp.dragged(), crate::theme::FAST);
    let x = crate::theme::value(ui.ctx(), egui::Id::new(("fader_x", id)), track.left() + track.width() * pos(*db) - rect.left(), if resp.dragged() { 0.0 } else { crate::theme::FAST }) + rect.left();
    let knob = egui::Rect::from_center_size(egui::pos2(x, rect.center().y), egui::vec2(8.0 + 2.0 * hv, 20.0));
    let shadow = egui::Shadow { offset: [0, 1], blur: 4, spread: 0, color: Color32::from_black_alpha(110) };
    ui.painter().add(shadow.as_shape(knob, 3));
    ui.painter().rect_filled(knob, 3, Color32::WHITE);
    resp.on_hover_cursor(egui::CursorIcon::ResizeHorizontal).on_hover_text("Volume: drag the knob (Shift: finer). Double-click for 0 dB, right-click to type one")
}

/// A source's volume, shown, and typed over on a click (or a right-click,
/// or `start`: one on its fader). Enter or clicking away sets it, Esc
/// leaves it as it was.
fn volume_value(ui: &mut egui::Ui, db: &mut f32, start: bool, id: &str) {
    use crate::theme::{self, W};
    let pal = theme::pal(ui);
    let key = egui::Id::new(("volume_typing", id));
    let edit_id = key.with("edit");
    let mut typing: Option<String> = ui.data(|d| d.get_temp(key));
    let (r, resp) = ui.allocate_exact_size(egui::vec2(66.0, 22.0), Sense::click());
    let started = typing.is_none() && (start || resp.clicked() || resp.secondary_clicked());
    if started {
        typing = Some(crate::ui_kit::db_edit_text(*db, FADER_MIN_DB));
    }
    match typing.as_mut() {
        None => {
            let hv = theme::toggle(ui.ctx(), resp.id.with("hv"), resp.hovered(), theme::FAST);
            ui.painter().rect_filled(r, 4, theme::fade(pal.sel, hv));
            ui.painter().text(r.right_center() - egui::vec2(6.0, 0.0), egui::Align2::RIGHT_CENTER, db_text(*db), theme::font(12.5, W::Medium), theme::mix(pal.text2, pal.text, hv));
            resp.on_hover_cursor(egui::CursorIcon::Text).on_hover_text("Click to type a volume");
        }
        Some(text) => {
            ui.painter().rect_filled(r, 4, pal.control);
            let font = theme::font(12.5, W::Medium);
            let unit = ui.painter().layout_no_wrap("dB".to_owned(), font.clone(), pal.text3);
            let field = egui::Rect::from_min_max(r.left_top() + egui::vec2(5.0, 0.0), egui::pos2(r.right() - 6.0 - unit.size().x - 4.0, r.bottom()));
            let mut fui = ui.new_child(egui::UiBuilder::new().max_rect(field).layout(egui::Layout::right_to_left(egui::Align::Center)));
            // A volume as it's said: "-7.5", "+3", "4,5 dB", and "−∞" or
            // "-inf" for silence (read by `parse_db`, #26).
            #[allow(clippy::disallowed_methods)]
            let out = egui::TextEdit::singleline(text)
                .id(edit_id)
                .frame(egui::Frame::NONE)
                .font(font)
                .text_color(pal.text)
                .horizontal_align(egui::Align::Max)
                .desired_width(field.width())
                .margin(egui::Margin::ZERO)
                .show(&mut fui);
            ui.painter().galley(egui::pos2(r.right() - 6.0 - unit.size().x, r.center().y - unit.size().y / 2.0), unit, pal.text3);
            ui.painter().rect_stroke(r, 4, egui::Stroke::new(1.5, pal.text2), egui::StrokeKind::Inside);
            if started {
                // All of it chosen: what's typed replaces it.
                out.response.request_focus();
                let mut state = out.state.clone();
                state.cursor.set_char_range(Some(egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(text.chars().count()))));
                state.store(ui.ctx(), edit_id);
            } else if !out.response.has_focus() {
                if !ui.input(|i| i.key_pressed(egui::Key::Escape))
                    && let Some(v) = crate::ui_kit::parse_db(text)
                {
                    *db = (v.clamp(FADER_MIN_DB, FADER_MAX_DB) * 10.0).round() / 10.0;
                }
                typing = None;
            }
        }
    }
    ui.data_mut(|d| match typing {
        Some(t) => {
            d.insert_temp(key, t);
        }
        None => d.remove::<String>(key),
    });
}

fn db_text(db: f32) -> String {
    if db <= FADER_MIN_DB + 0.05 {
        "−∞ dB".to_owned()
    } else if db.abs() < 0.05 {
        "0.0 dB".to_owned()
    } else {
        format!("{}{:.1} dB", if db > 0.0 { "+" } else { "−" }, db.abs())
    }
}

/// A microphone's device: the current one, a menu of the others.
fn mic_picker(ui: &mut egui::Ui, source: &mut AudioSourceCfg, audio: &capture::audio::AudioDevices, locked: bool) -> Option<CardAction> {
    use crate::ui_kit::Item;
    use capture::audio::DEFAULT_DEVICE;
    let SourceKind::Microphone { device } = &mut source.kind else { return None };
    // Devices by their names ("AT2020USB+ Analog Stereo"), not their ids
    // (on Linux, PipeWire's "alsa_input.usb-…").
    let name_of = |id: &str| audio.inputs.iter().find(|d| d.id == id).map(|d| d.name.clone());
    let default_label = match &audio.default_input {
        Some(id) => format!("Default ({})", name_of(id).unwrap_or_else(|| id.clone())),
        None => "Default".to_owned(),
    };
    let selected = if device == DEFAULT_DEVICE {
        default_label.clone()
    } else if let Some(name) = name_of(device) {
        name
    } else {
        format!("{device} (disconnected)")
    };
    let w = ui.available_width().min(210.0);
    let shown = truncate_to(ui, &selected, w - 40.0);
    let r = crate::ui_kit::select(ui, &shown, 0.0).on_hover_text(&selected);
    if locked {
        if r.clicked() {
            egui::Popup::close_id(ui.ctx(), r.id.with("menu"));
            return Some(CardAction::Locked);
        }
        return None;
    }
    crate::ui_kit::menu_popup(&r, false, 260.0, |ui| {
        if Item::new(default_label.clone()).check(device == DEFAULT_DEVICE).show(ui).clicked() {
            *device = DEFAULT_DEVICE.to_owned();
        }
        crate::ui_kit::menu_sep(ui);
        for d in &audio.inputs {
            if Item::new(d.name.clone()).check(&d.id == device).show(ui).clicked() {
                *device = d.id.clone();
            }
        }
    });
    None
}

/// `text`, shortened with … to fit `width` at the select's size.
fn truncate_to(ui: &egui::Ui, text: &str, width: f32) -> String {
    let font = crate::theme::font(13.0, crate::theme::W::Medium);
    let fits = |t: &str| ui.painter().layout_no_wrap(t.to_owned(), font.clone(), Color32::PLACEHOLDER).size().x <= width;
    if fits(text) {
        return text.to_owned();
    }
    let mut chars: Vec<char> = text.chars().collect();
    while !chars.is_empty() {
        chars.pop();
        let t: String = chars.iter().collect::<String>() + "…";
        if fits(&t) {
            return t;
        }
    }
    "…".into()
}

/// A source's state when it isn't simply live: waiting for its app, or
/// unavailable.
fn status_text(status: SourceStatus) -> Option<(&'static str, fn(&crate::theme::Pal) -> Color32)> {
    match status {
        SourceStatus::Live | SourceStatus::Off => None,
        SourceStatus::WaitingForApp => Some((if cfg!(target_os = "linux") { "Starts when the app plays sound" } else { "Starts when the app opens" }, |p| p.text3)),
        SourceStatus::Unavailable => Some(("Not connected, or couldn't be opened", |p| p.warn)),
    }
}

/// How tall a row of the games and apps' list is.
pub(crate) const GROUP_ROW_H: f32 = 42.0;

pub(crate) fn from_db(db: f32) -> f32 {
    if db <= FADER_MIN_DB + 0.05 { 0.0 } else { 10f32.powf(db / 20.0) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A name field being edited, frame by frame, with what was pressed.
    struct Field {
        ctx: egui::Context,
        name: String,
        done: bool,
    }

    impl Field {
        fn open(name: &str) -> Self {
            let mut f = Self { ctx: egui::Context::default(), name: name.to_owned(), done: false };
            crate::theme::install_fonts(&f.ctx);
            f.frame(Vec::new());
            f.frame(Vec::new());
            f
        }

        fn frame(&mut self, events: Vec<egui::Event>) {
            let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(600.0, 400.0))), events, ..Default::default() };
            let (name, done) = (&mut self.name, &mut self.done);
            self.ctx
                .run_ui(input, |ui| {
                    if !*done {
                        *done = name_field(ui, egui::Id::new("name"), name, egui::FontId::proportional(14.0), 170.0);
                    }
                })
                .textures_delta
                .clear();
        }

        fn key(&mut self, key: egui::Key) {
            let press = |pressed| egui::Event::Key { key, physical_key: None, pressed, repeat: false, modifiers: Default::default() };
            self.frame(vec![press(true), press(false)]);
            self.frame(Vec::new());
        }

        fn typed(&mut self, text: &str) {
            self.frame(vec![egui::Event::Text(text.to_owned())]);
        }
    }

    #[test]
    fn enter_keeps_the_new_name() {
        let mut f = Field::open("Mic");
        f.typed(" 2  ");
        assert!(!f.done, "still editing while typing");
        f.key(egui::Key::Enter);
        assert!(f.done, "Enter ends editing");
        assert_eq!(f.name, "Mic 2");
    }

    #[test]
    fn escape_or_nothing_puts_the_name_back() {
        let mut f = Field::open("Mic");
        f.typed(" 2");
        f.key(egui::Key::Escape);
        assert!(f.done, "Esc ends editing");
        assert_eq!(f.name, "Mic");

        let mut f = Field::open("Mic");
        for _ in 0..3 {
            f.key(egui::Key::Backspace);
        }
        assert_eq!(f.name, "");
        f.key(egui::Key::Enter);
        assert_eq!((f.done, f.name.as_str()), (true, "Mic"));
    }

    #[test]
    fn clicking_away_keeps_it() {
        let mut f = Field::open("Desktop");
        f.typed(" sound");
        let at = egui::pos2(500.0, 300.0);
        let click = |pressed| egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: Default::default() };
        f.frame(vec![egui::Event::PointerMoved(at), click(true)]);
        f.frame(vec![click(false)]);
        f.frame(Vec::new());
        assert!(f.done, "clicking away ends editing");
        assert_eq!(f.name, "Desktop sound");
    }

    fn doing(clipping: bool, recording: bool, streaming: bool) -> crate::RecState {
        crate::RecState { clipping, recording, streaming }
    }

    /// A source a capture started with.
    fn started(id: &str, in_mix: bool, own_track: bool) -> capture::sources::AudioSource {
        let kind = capture::sources::SourceKind::Desktop { exclude_app_sources: true };
        capture::sources::AudioSource { id: id.to_owned(), name: id.to_owned(), kind, in_mix, own_track }
    }

    /// A source added since.
    fn added(in_mix: bool, own_track: bool) -> AudioSourceCfg {
        let mut s = AudioSourceCfg::new("Game", SourceKind::Desktop { exclude_apps: true });
        (s.in_mix, s.own_track) = (in_mix, own_track);
        s
    }

    #[test]
    fn a_source_added_while_only_streaming_is_all_there() {
        let started = [started("mic", true, true)];
        // In the mix (the stream's sound), with a track or without; or kept
        // out of it, as it would be from any stream.
        for (in_mix, own_track) in [(true, true), (true, false), (false, true)] {
            assert_eq!(added_source_note(doing(false, false, true), &started, &added(in_mix, own_track)), None);
        }
    }

    #[test]
    fn a_source_added_to_a_capture_without_sound_says_what_it_isnt_in() {
        let note = |state| added_source_note(state, &[], &added(true, true));
        let stream = "Not in the stream until you stop streaming and start again: this started without sound.";
        assert_eq!(note(doing(false, false, true)), Some((stream.to_owned(), true)));
        let both = "Not in clips or the stream until you stop clipping and streaming and start again: this started without sound.";
        assert_eq!(note(doing(true, false, true)), Some((both.to_owned(), true)));
        let all = "Not in clips, the recording or the stream until you stop clipping, recording and streaming and start again: this started without sound.";
        assert_eq!(note(doing(true, true, true)), Some((all.to_owned(), true)));
    }

    #[test]
    fn a_source_added_while_clipping_says_what_it_waits_for() {
        let clipping = doing(true, false, false);
        // A mic on its own track and the desktop in the mix alone: the rest
        // track keeps a source added in the mix.
        let with_rest = [started("mic", true, true), started("desktop", true, false)];
        assert_eq!(added_source_note(clipping, &with_rest, &added(true, false)), None);
        let (text, warn) = added_source_note(clipping, &with_rest, &added(true, true)).unwrap();
        assert!(!warn && text.ends_with("Its own track starts once you stop clipping and start again."), "{text}");
        // Every source on its own track: none keeps it, so the editor's
        // track volumes leave it out.
        let own_tracks = [started("mic", true, true)];
        let (text, warn) = added_source_note(clipping, &own_tracks, &added(true, false)).unwrap();
        assert!(warn && text.ends_with("changing track volumes in the editor leaves it out."), "{text}");
        // Out of the mix: only its own track takes it, once the capture
        // starts again.
        let note = added_source_note(doing(true, false, true), &own_tracks, &added(false, true));
        let wait = "Not recorded until you stop clipping and streaming and start again: its own track starts then.";
        assert_eq!(note, Some((wait.to_owned(), true)));
    }
}
