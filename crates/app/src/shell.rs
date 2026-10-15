//! The window around the pages: the rail with the app's places, the recorder
//! top right, page headings and toasts.
//!
//! - **The rail** holds the places (Clips, Sources, Settings) as big filled
//!   icons with a word under them, so a place can't be mistaken for a filter.
//!   On macOS it runs to the top of the window, under the traffic lights.
//! - **The recorder** sits in the same place on every page but the player and
//!   editor: what's happening, then the one thing you'd do now (Start
//!   clipping, Save clip, Stop recording). The ∨ by the status holds the rest
//!   as plain actions. Clipping and recording never run together, so Record
//!   is only offered when you're not clipping.
//! - **Toasts** rise in at the bottom, say what happened with an icon, and
//!   can carry one action (Undo, Show).

use std::time::{Duration, Instant};

use egui::{Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, vec2};

use crate::settings::ShortcutAction as A;
use crate::theme::{self, W, icon};
use crate::ui_kit::{self, Btn, Item, Kind};
use crate::ui_kit::Dismissed as _;
use crate::{App, Page, RecState, thumbs};

/// The rail's width.
pub const RAIL_W: f32 = 78.0;
/// The height of a page's heading row (the recorder's row).
pub const HEAD_H: f32 = 52.0;

/// Whether the window draws under a see-through title bar (macOS): the rail
/// and headers keep clear of the traffic lights.
pub fn under_titlebar() -> bool {
    cfg!(target_os = "macos")
}

/// macOS: the window's buttons (close, minimise, fullscreen) centred in a
/// title bar as tall as the headings ([`HEAD_H`]), as Finder's are: an empty
/// toolbar in the unified style does it, and macOS keeps them there through
/// resizing, fullscreen and focus (moving the buttons by hand has to be
/// redone after each, and flickers).
#[cfg(target_os = "macos")]
pub fn centre_window_buttons(window: &impl raw_window_handle::HasWindowHandle) {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSToolbar, NSView, NSWindowToolbarStyle};
    use objc2_foundation::NSString;
    use raw_window_handle::RawWindowHandle;
    let Ok(handle) = window.window_handle() else { return };
    let RawWindowHandle::AppKit(h) = handle.as_raw() else { return };
    let Some(mtm) = MainThreadMarker::new() else { return };
    // SAFETY: eframe's AppKit handle points at the live content NSView, and
    // this runs on the main thread (checked above).
    let view: &NSView = unsafe { h.ns_view.cast::<NSView>().as_ref() };
    let Some(window) = view.window() else { return };
    let toolbar = NSToolbar::initWithIdentifier(mtm.alloc(), &NSString::from_str("hesteclips"));
    window.setToolbar(Some(&toolbar));
    window.setToolbarStyle(NSWindowToolbarStyle::Unified);
    place_window_buttons(view);
}

/// macOS: the window's buttons centred over the rail (the toolbar centres
/// them up and down; across, macOS puts them nearer the edge). macOS lays
/// them out again now and then (resizing, fullscreen, focus), so this runs
/// every frame; it only moves them when they've moved.
#[cfg(target_os = "macos")]
pub fn keep_window_buttons(window: &impl raw_window_handle::HasWindowHandle) {
    use objc2_app_kit::NSView;
    use raw_window_handle::RawWindowHandle;
    let Ok(handle) = window.window_handle() else { return };
    let RawWindowHandle::AppKit(h) = handle.as_raw() else { return };
    if objc2::MainThreadMarker::new().is_none() {
        return;
    }
    // SAFETY: eframe's AppKit handle points at the live content NSView, on
    // the main thread (checked above).
    let view: &NSView = unsafe { h.ns_view.cast::<NSView>().as_ref() };
    place_window_buttons(view);
}

#[cfg(target_os = "macos")]
fn place_window_buttons(view: &objc2_app_kit::NSView) {
    use objc2_app_kit::{NSWindowButton, NSWindowStyleMask};
    let Some(window) = view.window() else { return };
    // Fullscreen keeps a window's toolbar on screen: ours is empty (it's only
    // there to centre the buttons), so it showed as a grey bar over the
    // picture. Hidden while fullscreen, back after.
    let fullscreen = window.styleMask().contains(NSWindowStyleMask::FullScreen);
    if let Some(toolbar) = window.toolbar()
        && toolbar.isVisible() == fullscreen
    {
        toolbar.setVisible(!fullscreen);
    }
    let buttons = [NSWindowButton::CloseButton, NSWindowButton::MiniaturizeButton, NSWindowButton::ZoomButton];
    // Each button's frame is its own size; they're spaced evenly, centred
    // as a group over the rail.
    let frames: Vec<_> = buttons.iter().filter_map(|b| window.standardWindowButton(*b)).collect();
    let Some(first) = frames.first() else { return };
    let w = first.frame().size.width;
    let gap = frames.get(1).map_or(6.0, |b| b.frame().origin.x - first.frame().origin.x - w);
    let group = w * frames.len() as f64 + gap * (frames.len() as f64 - 1.0);
    let left = ((RAIL_W as f64 - group) / 2.0).round();
    for (i, b) in frames.iter().enumerate() {
        let mut f = b.frame();
        let x = left + i as f64 * (w + gap);
        if (f.origin.x - x).abs() > 0.5 {
            f.origin.x = x;
            b.setFrameOrigin(f.origin);
        }
    }
}

/// Whether the page sits in a frame of the rail's colour, under a title bar
/// of that colour (Windows): its top-left corner is rounded, with a hairline
/// round its top and left edges, so the title bar and rail read as one frame
/// around it. (macOS has no title bar; Linux's is the desktop's own colour.)
pub fn framed_page() -> bool {
    cfg!(target_os = "windows")
}

/// How round the page's top-left corner is, in the frame: Windows 11's own
/// window corners.
pub const PAGE_CORNER: u8 = 8;

/// The hairline round a framed page's top and left edges (see
/// [`framed_page`]), on the page's outermost pixel. Drawn over the page.
pub fn page_edge(painter: &egui::Painter, page: Rect, color: Color32) {
    let (l, t) = (page.left() + 0.5, page.top() + 0.5);
    let r = PAGE_CORNER as f32 - 0.5;
    let mut points = vec![Pos2::new(l, page.bottom())];
    // The corner, a quarter circle from the left edge round to the top.
    let steps = 8;
    for i in 0..=steps {
        let a = std::f32::consts::PI + std::f32::consts::FRAC_PI_2 * i as f32 / steps as f32;
        points.push(Pos2::new(l + r + r * a.cos(), t + r + r * a.sin()));
    }
    points.push(Pos2::new(page.right(), t));
    painter.add(egui::Shape::line(points, Stroke::new(1.0, color)));
}

/// Windows: the title bar in `caption` (the rail's colour, or the page's
/// where there's no rail) with the app's text colour, so it reads as part of
/// the window. Runs every frame; only tells Windows when something changed.
/// (Windows 10 ignores the colours and keeps its own, light or dark.)
#[cfg(target_os = "windows")]
pub fn tint_title_bar(window: &impl raw_window_handle::HasWindowHandle, caption: Color32, dark: bool) {
    use raw_window_handle::RawWindowHandle;
    use std::sync::atomic::{AtomicU32, Ordering};
    use windows::Win32::Foundation::{COLORREF, HWND};
    use windows::core::BOOL;
    use windows::Win32::Graphics::Dwm::{DWMWA_CAPTION_COLOR, DWMWA_TEXT_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute};
    let colorref = |c: Color32| u32::from(c.r()) | u32::from(c.g()) << 8 | u32::from(c.b()) << 16;
    // What Windows was last told: the caption colour, the dark flag, and a
    // bit so it's never 0 (not yet).
    static DONE: AtomicU32 = AtomicU32::new(0);
    let now = colorref(caption) | u32::from(dark) << 24 | 1 << 25;
    if DONE.load(Ordering::Relaxed) == now {
        return;
    }
    let Ok(handle) = window.window_handle() else { return };
    let RawWindowHandle::Win32(h) = handle.as_raw() else { return };
    let hwnd = HWND(h.hwnd.get() as *mut _);
    let p = if dark { &theme::DARK } else { &theme::LIGHT };
    let caption = COLORREF(colorref(caption));
    let text = COLORREF(colorref(p.text));
    // The window buttons' glyphs follow the dark flag, not the caption colour.
    let dark_flag = BOOL::from(dark);
    // SAFETY: `hwnd` is our live top-level window; each value is the size
    // and type the attribute takes.
    unsafe {
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark_flag as *const _ as *const _, size_of::<BOOL>() as u32);
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_CAPTION_COLOR, &caption as *const _ as *const _, size_of::<COLORREF>() as u32);
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_TEXT_COLOR, &text as *const _ as *const _, size_of::<COLORREF>() as u32);
    }
    DONE.store(now, Ordering::Relaxed);
}

/// Which screen's keyboard shortcuts to show.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Keys {
    Player,
    Editor,
}

/// What a toast's button does.
#[derive(Clone)]
pub enum ToastAction {
    /// Open the clip in the player.
    Show(std::path::PathBuf),
    /// Put clips just moved to the Bin back.
    Undelete(crate::library::Binned),
}

#[derive(Clone)]
pub struct Toast {
    pub text: String,
    pub icon: &'static str,
    pub error: bool,
    pub at: Instant,
    pub action: Option<(String, ToastAction)>,
    pub for_: Duration,
}

impl App {
    /// The rail: the app's places, top to bottom.
    pub(crate) fn rail(&mut self, ui: &mut egui::Ui) {
        let p = theme::pal(ui);
        let rect = ui.max_rect();
        ui.painter().rect_filled(rect, 0, p.ground);
        // With the window's own title bar above (not macOS), the first
        // place's pill is centred on the page heading's row, level with its
        // title and buttons.
        let top = rect.top() + if under_titlebar() { 52.0 } else { 9.0 };
        let on = match self.page {
            Page::Clips | Page::View | Page::Edit => Page::Clips,
            Page::VoiceReview | Page::Settings => Page::Settings,
            Page::Sources => Page::Sources,
        };
        let places = [
            (Page::Clips, egui_phosphor::fill::FILM_SLATE, "Clips", None),
            (Page::Sources, egui_phosphor::fill::FADERS, "Sources", Some("What goes into your clips: the picture, your mic, the game's sound".to_owned())),
            (Page::Settings, egui_phosphor::fill::GEAR_SIX, "Settings", Some(format!("Settings  ({})", crate::hotkey_label_cmd(",")))),
        ];
        let mut y = top;
        for (page, glyph, label, tip) in places {
            let r = Rect::from_min_size(Pos2::new(rect.center().x - 32.0, y), vec2(64.0, 58.0));
            let resp = ui.interact(r, egui::Id::new(("rail", label)), Sense::click());
            // About to open Sources: its preview and meters start now, so
            // they're running by the time it shows.
            if page == Page::Sources && resp.hovered() {
                self.sources_warm = Some(Instant::now());
            }
            let chosen = on == page;
            let hv = theme::toggle(ui.ctx(), resp.id.with("hv"), resp.hovered(), theme::FAST);
            let sel = theme::toggle(ui.ctx(), resp.id.with("on"), chosen, theme::NORMAL);
            let pill = Rect::from_center_size(Pos2::new(r.center().x, r.top() + 6.0 + 15.0), vec2(44.0, 30.0));
            let hover_fill = theme::fade(p.sel, hv * (1.0 - sel));
            ui.painter().rect_filled(pill, ui_kit::RADIUS, hover_fill);
            if sel > 0.0 {
                let fill = if p.dark { p.seg_on } else { Color32::WHITE };
                // Grows into place as it's chosen.
                let s = 0.8 + 0.2 * sel;
                let r2 = Rect::from_center_size(pill.center(), pill.size() * s);
                if !p.dark {
                    ui.painter().rect_filled(r2.translate(vec2(0.0, 1.0)), ui_kit::RADIUS, theme::fade(Color32::from_black_alpha(30), sel));
                }
                ui.painter().rect_filled(r2, ui_kit::RADIUS, theme::fade(fill, sel));
            }
            let color = theme::mix(theme::mix(p.text3, p.text, hv), p.text, sel);
            ui.painter().text(pill.center(), Align2::CENTER_CENTER, glyph, FontId::new(20.0, theme::fill_family()), color);
            ui.painter().text(Pos2::new(r.center().x, pill.bottom() + 9.0), Align2::CENTER_CENTER, label, theme::font(11.0, W::Medium), color);
            let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
            let resp = match tip {
                Some(t) => resp.on_hover_text(t),
                None => resp,
            };
            if resp.clicked() && (!chosen || self.page != page) {
                self.page = page;
            }
            y += 66.0;
        }
        // Dragging the empty rail moves the window (it has no title bar).
        let rest = Rect::from_min_max(Pos2::new(rect.left(), rect.top()), Pos2::new(rect.right(), top - 4.0));
        drag_window_area(ui, rest, "rail_top");
        let below = Rect::from_min_max(Pos2::new(rect.left(), y), rect.max);
        drag_window_area(ui, below, "rail_below");
        // The edge between the rail and the page (a framed page draws its own,
        // round its corner).
        if !framed_page() {
            ui.painter().vline(rect.right() - 0.5, rect.y_range(), Stroke::new(1.0, p.line));
        }
    }

    /// The recorder: the status (its ∨ opens the other options) and the one
    /// action that makes sense now, with its shortcut. `main`: its action is
    /// the screen's main one (on Clips); elsewhere it's a normal button.
    /// `compact`: a narrow window, so just the status dot and ∨, and the
    /// action without its shortcut.
    pub(crate) fn recorder(&mut self, ui: &mut egui::Ui, compact: bool) {
        let p = theme::pal(ui);
        let state = self.rec_state;
        let elapsed = self.rec_started.map_or(Duration::ZERO, |t| t.elapsed());
        let len = clock(self.settings.replay_seconds);
        let (dot, title, sub) = match state {
            RecState::Idle => (None, "Not clipping".to_owned(), None),
            RecState::Buffering => (Some(p.live), "Clipping".to_owned(), Some(len)),
            RecState::Recording => (Some(p.rec), "Recording".to_owned(), Some(thumbs::format_duration(elapsed))),
        };
        let font = theme::font(13.0, W::Semibold);
        let tg = ui.painter().layout_no_wrap(title.clone(), font, p.text);
        let sg = sub.as_ref().map(|s| ui.painter().layout_no_wrap(s.clone(), theme::font(13.0, W::Medium), p.text3));
        let stat_target = if compact { 27.0 + 22.0 } else { 27.0 + tg.size().x + sg.as_ref().map_or(0.0, |g| g.size().x + 6.0) + 8.0 + 20.0 };
        let stat_w = theme::value(ui.ctx(), egui::Id::new("rec_stat_w"), stat_target, theme::NORMAL);
        // The one action.
        let saving = self.saving > 0;
        // The app's main action, the same on every page.
        let kind = Kind::Primary;
        let btn = match state {
            RecState::Idle => Btn::new(kind, "Start clipping")
                .keys(self.shortcut_keys(A::ToggleBuffer))
                .tip(format!("Keep the last {} ready to save", crate::settings_ui::length_words(self.settings.replay_seconds))),
            RecState::Buffering => Btn::new(kind, if saving { "Saving…" } else { "Save clip" })
                .keys(self.shortcut_keys(A::SaveClip))
                .tip(format!("Save the last {}", crate::settings_ui::length_words(self.settings.replay_seconds))),
            RecState::Recording => Btn::new(Kind::Stop, "Stop recording").with_icon(egui_phosphor::fill::STOP).keys(self.shortcut_keys(A::ToggleRecord)).tip("Stop and save the recording"),
        }
        .id("recorder_action");
        let btn = if compact { btn.keys(None) } else { btn };
        // While clipping, a quiet square Stop beside Save clip: the pill grows to
        // make room and it fades in as it does. Not while a clip is being saved.
        let stop_w = theme::value(ui.ctx(), egui::Id::new("rec_stop_w"), if state == RecState::Buffering { 32.0 } else { 0.0 }, theme::NORMAL);
        let total = 2.0 + stat_w + 2.0 + btn.width(ui) + stop_w + 2.0;
        // Status first, then the action: laid out left to right in a box
        // allocated from the right.
        ui.allocate_ui_with_layout(vec2(total, 34.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
            let (outer, _) = ui.allocate_exact_size(vec2(total, 34.0), Sense::hover());
            ui.painter().rect_filled(outer, 8, p.control);
            let mut inner = ui.new_child(egui::UiBuilder::new().max_rect(outer.shrink(2.0)).layout(egui::Layout::left_to_right(egui::Align::Center)));
            let ui = &mut inner;
            ui.set_clip_rect(outer.shrink(2.0).intersect(ui.clip_rect()));
            ui.spacing_mut().item_spacing.x = 2.0;
            let (r, resp) = ui.allocate_exact_size(vec2(stat_w, 30.0), Sense::click());
            let menu_id = egui::Id::new("recorder_menu");
            let open = egui::Popup::is_id_open(ui.ctx(), menu_id);
            let hv = theme::toggle(ui.ctx(), resp.id.with("hv"), resp.hovered() || open, theme::FAST);
            let painter = ui.painter_at(r);
            painter.rect_filled(r, ui_kit::RADIUS, theme::fade(p.control_h, hv));
            let c = Pos2::new(r.left() + 15.0, r.center().y);
            match dot {
                Some(color) => status_dot(&painter, c, color, ui.input(|i| i.time)),
                None => {
                    painter.circle_stroke(c, 3.5, Stroke::new(1.5, p.text3));
                }
            }
            // Clipped to the box while its width eases between compact and not.
            let words = ui.painter_at(r.with_max_x(r.right() - 24.0));
            let mut x = r.left() + 27.0;
            words.galley(Pos2::new(x, r.center().y - tg.size().y / 2.0), tg.clone(), p.text);
            x += tg.size().x + 6.0;
            if let Some(g) = sg {
                words.galley(Pos2::new(x, r.center().y - g.size().y / 2.0), g.clone(), p.text3);
            }
            let turn = theme::toggle(ui.ctx(), resp.id.with("turn"), open, theme::NORMAL);
            ui_kit::chevron(&painter, Pos2::new(r.right() - 14.0, r.center().y), 3.5, turn, p.text3);
            let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(if compact { format!("{title}{}", sub.as_ref().map(|s| format!("  {s}")).unwrap_or_default()) } else { "More options".to_owned() });
            egui::Popup::menu(&resp)
                .id(menu_id)
                .gap(6.0)
                .align(egui::RectAlign::BOTTOM_END)
                .frame(ui_kit::menu_frame(ui))
                .style(ui_kit::menu_style)
                .show(|ui| ui_kit::menu_body(ui, 220.0, |ui| self.recorder_menu(ui)));
            if btn.show(ui).clicked() {
                match state {
                    RecState::Idle => self.start_replay_buffer(),
                    RecState::Buffering => self.save_clip(),
                    RecState::Recording => self.stop(),
                }
            }
            if stop_w > 0.5 {
                let key = self.shortcut_keys(A::ToggleBuffer).map(|k| format!("  {}", ui_kit::key_text(&k))).unwrap_or_default();
                ui.scope(|ui| {
                    ui.multiply_opacity((stop_w / 32.0).clamp(0.0, 1.0));
                    let stop = Btn::icon(Kind::Ghost, egui_phosphor::fill::STOP).id("recorder_stop").tip(format!("Stop clipping{key}")).enabled(!saving);
                    if stop.show(ui).clicked() {
                        self.stop();
                    }
                });
            }
        });


    }

    /// The recorder's ∨: the other things to do now, then shortcuts to what's
    /// recorded and how far back a clip reaches. No status repeated.
    fn recorder_menu(&mut self, ui: &mut egui::Ui) {
        match self.rec_state {
            RecState::Idle => {
                if Item::new("Record until you stop").icon(egui_phosphor::fill::RECORD).keys(self.shortcut_keys(A::ToggleRecord)).show(ui).clicked() {
                    self.start_recording();
                }
            }
            RecState::Buffering => {
                if Item::new("Stop clipping").icon(egui_phosphor::fill::STOP).keys(self.shortcut_keys(A::ToggleBuffer)).show(ui).clicked() {
                    self.stop();
                }
            }
            RecState::Recording => {}
        }
        if self.rec_state != RecState::Recording {
            ui_kit::menu_sep(ui);
        }
        if Item::new("Sources").icon(icon::FADERS).value(self.sources_summary()).show(ui).clicked() {
            self.page = Page::Sources;
        }
        if self.rec_state == RecState::Recording {
            return;
        }
        let words = crate::settings_ui::length_words(self.settings.replay_seconds);
        let mut picked = None;
        ui_kit::submenu(ui, Item::new("Clip length").icon(icon::TIMER).value(words), |ui| {
            for secs in crate::settings_ui::REPLAY_CHOICES {
                if Item::new(crate::settings_ui::length_words(secs)).check(self.settings.replay_seconds == secs).show(ui).clicked() {
                    picked = Some(secs);
                }
            }
            ui_kit::menu_sep(ui);
            if Item::new("Any length in Settings…").icon(icon::TEXT_T).show(ui).clicked() {
                self.page = Page::Settings;
                ui.close();
            }
        });
        if let Some(secs) = picked {
            self.settings.replay_seconds = secs;
            ui.close();
        }
    }

    /// What's recorded, in a few words: "osu!, mic, desktop".
    fn sources_summary(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        match &self.settings.capture {
            crate::settings::CaptureTarget::Apps { apps, .. } if !apps.is_empty() => {
                parts.extend(apps.iter().take(2).map(|a| a.name.clone()));
                if apps.len() > 2 {
                    parts.push(format!("{} more", apps.len() - 2));
                }
            }
            _ => parts.push("screen".into()),
        }
        for s in self.settings.audio_sources.iter().filter(|s| s.enabled) {
            parts.push(match &s.kind {
                crate::settings::SourceKind::Microphone { .. } => "mic".into(),
                crate::settings::SourceKind::Desktop { .. } => "desktop".into(),
                crate::settings::SourceKind::App { app_name, .. } => app_name.clone(),
            });
        }
        parts.dedup();
        parts.join(", ")
    }

    /// A page's heading row: the title (and whatever `left` adds after it),
    /// then the recorder on the right. Dragging its empty space moves the
    /// window.
    pub(crate) fn page_head(&mut self, ui: &mut egui::Ui, title: &str, left: impl FnOnce(&mut Self, &mut egui::Ui)) {
        self.page_head_folding(ui, title, |app, ui, _| left(app, ui));
    }

    /// [`Self::page_head`] for a heading with things it can fold away when
    /// it's narrow (`left` gets `tight`: put them in a menu). Room runs out
    /// in this order: first those fold, then the recorder drops its words.
    pub(crate) fn page_head_folding(&mut self, ui: &mut egui::Ui, title: &str, left: impl FnOnce(&mut Self, &mut egui::Ui, bool)) {
        let p = theme::pal(ui);
        let rect = Rect::from_min_size(ui.cursor().min, vec2(ui.available_width(), HEAD_H));
        drag_window_area(ui, rect, "page_head");
        ui.allocate_ui_with_layout(rect.size(), egui::Layout::left_to_right(egui::Align::Center), |ui| {
            ui.set_min_height(HEAD_H);
            ui.add_space(0.0);
            if !title.is_empty() {
                ui.label(theme::text(title, 20.0, W::Semibold).color(p.text));
                ui.add_space(6.0);
            }
            // Narrow: the recorder shrinks to its dot and action, and the
            // left side (title, search) gets what's left, never under it.
            // Compact only when the full one doesn't fit beside what the
            // left side needs (both as wide as last frame; the left side's
            // search can shrink, so it counts at its smallest).
            // The left side's width, laid out in full and folded (each as
            // last drawn; folded unknown until it's been drawn so: guessed).
            let full_key = egui::Id::new(("page_head_right", false));
            let left_key = |tight: bool| egui::Id::new(("page_head_left", title, tight));
            let full_w = ui.data(|d| d.get_temp::<f32>(full_key)).unwrap_or(340.0);
            let left_full = ui.data(|d| d.get_temp::<f32>(left_key(false))).unwrap_or(0.0);
            let left_tight = ui.data(|d| d.get_temp::<f32>(left_key(true))).unwrap_or(left_full * 0.6);
            let room = rect.right() - ui.cursor().min.x;
            let fits = |left: f32| left + 16.0 + full_w <= room;
            let tight = !fits(left_full);
            let compact = tight && !fits(left_tight);
            let key = egui::Id::new(("page_head_right", compact));
            let right_w = ui.data(|d| d.get_temp::<f32>(key)).unwrap_or(if compact { 230.0 } else { 340.0 });
            let left_rect = Rect::from_min_max(ui.cursor().min, Pos2::new((rect.right() - right_w - 16.0).max(ui.cursor().min.x + 40.0), rect.bottom()));
            let mut lui = ui.new_child(egui::UiBuilder::new().max_rect(left_rect).layout(egui::Layout::left_to_right(egui::Align::Center)));
            left(self, &mut lui, tight);
            // What the left side took, less the search's give (it shrinks to 80).
            let used = lui.min_rect().width();
            let give = (ui_kit::SEARCH_W - 80.0).max(0.0);
            let need = if title.is_empty() { used } else { (used - give).max(0.0) };
            ui.data_mut(|d| d.insert_temp(left_key(tight), need));
            let right = ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let start = ui.cursor().max.x;
                self.recorder(ui, compact);
                if !compact {
                    self.update_button(ui);
                    self.skipped_warning(ui);
                }
                start - ui.cursor().max.x
            });
            ui.data_mut(|d| d.insert_temp(key, right.inner));
        });
    }

    /// A toast: what just happened, at the bottom of the window, rising in
    /// and coming into focus.
    pub(crate) fn toasts(&mut self, ctx: &egui::Context, frame: &eframe::Frame) {
        let Some(t) = self.toast.clone() else { return };
        let age = t.at.elapsed();
        let hovered = ctx.data(|d| d.get_temp::<bool>(egui::Id::new("toast_hovered"))).unwrap_or(false);
        if hovered {
            // Kept while the pointer is on it.
            if let Some(toast) = &mut self.toast {
                toast.at = Instant::now() - Duration::from_millis(400).min(age);
            }
        }
        if age > t.for_ + Duration::from_millis(250) {
            self.toast = None;
            return;
        }
        let p = theme::ctx_pal(ctx);
        let rise = theme::ease_out((age.as_secs_f32() / theme::SLOW).min(1.0));
        // Its opacity comes with the focus; it leaves with a plain fade.
        let focus = (age.as_secs_f32() / ui_kit::FOCUS_SECS).min(1.0);
        let fade_out = 1.0 - ((age.as_secs_f32() - t.for_.as_secs_f32()) / 0.25).clamp(0.0, 1.0);
        // Every frame while it moves (the page otherwise repaints only now and then).
        if rise < 1.0 || fade_out < 1.0 {
            ctx.request_repaint();
        }
        let bottom = self.toast_bottom;
        let mut act = None;
        let mut close = false;
        let area = egui::Area::new(egui::Id::new("toast"))
            .order(egui::Order::Foreground)
            .anchor(Align2::CENTER_BOTTOM, vec2(self.toast_x_offset, -bottom + 14.0 * (1.0 - rise)))
            .fade_in(false)
            .interactable(t.action.is_some())
            .show(ctx, |ui| {
                ui.multiply_opacity(fade_out);
                let frame = egui::Frame::new()
                    .fill(p.pop)
                    .stroke(Stroke::new(1.0, if p.dark { Color32::from_white_alpha(18) } else { Color32::from_black_alpha(20) }))
                    .corner_radius(ui_kit::CARD_RADIUS)
                    .shadow(ui.visuals().popup_shadow)
                    .inner_margin(egui::Margin { left: 12, right: if t.action.is_some() { 6 } else { 14 }, top: 6, bottom: 6 });
                ui_kit::focus_in(ui, focus, frame, |ui| {
                        ui.horizontal(|ui| {
                            ui.set_min_height(28.0);
                            ui.spacing_mut().item_spacing.x = 10.0;
                            let icon_color = if t.error { p.danger } else { p.text2 };
                            ui.label(theme::ic(t.icon, 15.0).color(icon_color));
                            ui.add(egui::Label::new(theme::text(&t.text, 13.5, W::Medium).color(p.text)).wrap_mode(egui::TextWrapMode::Extend));
                            if let Some((label, a)) = &t.action {
                                if Btn::normal(label.clone()).small().show(ui).clicked() {
                                    act = Some(a.clone());
                                }
                                if Btn::icon(Kind::Ghost, icon::X).small().tip("Close").show(ui).clicked() {
                                    close = true;
                                }
                            }
                        });
                });
            });
        ctx.data_mut(|d| d.insert_temp(egui::Id::new("toast_hovered"), area.response.contains_pointer()));
        if close {
            self.toast = None;
        }
        if let Some(a) = act {
            self.toast = None;
            match a {
                ToastAction::Show(clip) => {
                    let _ = frame;
                    self.open_viewer(clip);
                }
                ToastAction::Undelete(binned) => self.undelete(binned),
            }
        }
    }
}

impl App {
    /// The keyboard shortcuts of the player or the editor.
    pub(crate) fn keys_dialog(&mut self, ctx: &egui::Context) {
        let Some(which) = self.show_keys else { return };
        let rows: &[(&str, &str)] = match which {
            Keys::Player => &[
                ("Space  K", "Play or pause"),
                ("← →", "Back or ahead 5 seconds"),
                ("J  L", "Back or ahead 10 seconds"),
                (",  .", "One frame back or ahead"),
                ("[  ]", "Previous or next moment"),
                ("B", "Add a marker, or take it away"),
                ("Home  End", "Start or end"),
                ("Scroll", "Scrub"),
                ("M", "Mute"),
                ("<  >", "Slower or faster"),
                ("F", "Fullscreen"),
                ("Q  E", "Previous or next clip"),
                ("F2", "Rename"),
                ("Delete", "Delete the clip"),
                ("Esc", "Back to your clips"),
            ],
            Keys::Editor => &[
                ("Space", "Play or pause"),
                ("← →", "One frame back or ahead"),
                ("Shift ← →", "One second back or ahead"),
                ("I  O", "Start or end here"),
                ("Home  End", "Go to the start or end"),
                ("Cmd Z", "Undo  (Shift for redo)"),
                ("N", "Snapping on or off"),
                ("+  −  \\", "Zoom in, out, or to fit"),
                ("Ctrl scroll", "Zoom the timeline"),
                ("Shift scroll", "Move along the timeline"),
                ("Drag the volume line", "Change a track's volume"),
                ("Double-click it", "Add a volume point"),
                ("Right-click a point", "Remove it"),
                ("Cmd S", "Save"),
                ("F2", "Rename"),
            ],
        };
        let mut close = false;
        let modal = ui_kit::dialog(ctx, "keys_dialog", 440.0, |ui| {
            ui_kit::dialog_title(ui, "Keyboard shortcuts");
            let p = theme::pal(ui);
            egui::Grid::new("keys_grid").num_columns(2).spacing(vec2(18.0, 8.0)).show(ui, |ui| {
                for (k, what) in rows {
                    ui.label(theme::text(*k, 13.5, W::Semibold).color(p.text));
                    ui.label(theme::text(*what, 13.5, W::Regular).color(p.text2));
                    ui.end_row();
                }
            });
            ui_kit::dialog_buttons(ui, |ui| {
                close = ui.add(ui_kit::button("Done", false)).clicked();
            });
        });
        if close || modal.dismissed() {
            self.show_keys = None;
        }
    }
}

/// The status dot: steady, with a soft ring breathing out from it.
pub fn status_dot(painter: &egui::Painter, c: Pos2, color: Color32, time: f64) {
    let t = (time % 1.6 / 1.6) as f32;
    let ring = 4.0 + 6.0 * theme::ease_out(t);
    painter.circle_filled(c, ring, theme::fade(color, 0.35 * (1.0 - t)));
    painter.circle_filled(c, 4.0, color);
    painter.ctx().request_repaint_after(Duration::from_millis(33));
}

/// Dragging this (empty) part of the window moves it: there's no title bar
/// on macOS. Double-clicking zooms it, as a title bar does.
pub fn drag_window_area(ui: &egui::Ui, rect: Rect, id: &str) {
    if !under_titlebar() {
        return;
    }
    let resp = ui.interact(rect, egui::Id::new(("drag_window", id)), Sense::click_and_drag());
    // Only its own empty space: on a button or field in it, the button gets
    // the click and the drag (egui hands a drag to whatever senses drags, so
    // this asks which widget the pointer is on).
    // (The ids first, then each looked up: egui's locks don't nest.)
    let under: Vec<egui::Id> = ui.ctx().interaction_snapshot(|s| s.contains_pointer.iter().copied().collect());
    let mine = under.into_iter().all(|w| w == resp.id || !is_interactive(ui.ctx(), w));
    if !mine {
        return;
    }
    if resp.drag_started_by(egui::PointerButton::Primary) {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }
    if resp.double_clicked() {
        let max = ui.ctx().input(|i| i.viewport().maximized.unwrap_or(false));
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Maximized(!max));
    }
}

/// Whether widget `id` (as drawn last frame) takes clicks or drags.
fn is_interactive(ctx: &egui::Context, id: egui::Id) -> bool {
    ctx.read_response(id).is_some_and(|r| r.sense.senses_click() || r.sense.senses_drag())
}

/// A clip length as a clock: "1:00", "15:00", "1:00:00".
pub fn clock(secs: u32) -> String {
    if secs >= 3600 {
        format!("{}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
    } else {
        format!("{}:{:02}", secs / 60, secs % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The window's drag area with a button on it, the pointer over both:
    /// no lock taken inside another (it froze the app).
    #[test]
    fn drag_area_over_a_button() {
        if !under_titlebar() {
            return;
        }
        let ctx = egui::Context::default();
        let at = egui::pos2(40.0, 20.0);
        for events in [vec![egui::Event::PointerMoved(at)], vec![], vec![]] {
            let raw = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(400.0, 300.0))), events, ..Default::default() };
            let mut out = ctx.run_ui(raw, |ui| {
                let rect = Rect::from_min_size(Pos2::ZERO, vec2(400.0, 52.0));
                drag_window_area(ui, rect, "test");
                ui.put(Rect::from_min_size(egui::pos2(20.0, 10.0), vec2(60.0, 30.0)), egui::Button::new("Button"));
            });
            out.textures_delta.clear();
        }
    }
}
