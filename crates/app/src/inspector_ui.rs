//! The panel beside the preview on the Sources page: the settings of what's
//! selected (on the preview, or in a list under it), laid out the same way
//! for every source in the video: its name, its switch and its menu; then
//! what it shows (its own section: which camera, display or window, the
//! picture, the words); then where it is (Transform), and what of it shows
//! (Crop, for what has a picture to cut). A sound source, or the clip's mix,
//! shows its own settings here too (see `sources_ui`).

use egui_phosphor::regular as icon;

use crate::App;
use crate::canvas_ui::{Layer, Selected};
use crate::settings::{CaptureTarget, TextAlign, TextStyle, Visual};
use crate::theme::{self, W};
use crate::ui_kit::{self, Btn, Item, Kind, Row};

/// The panel's width, beside the preview on a page wide enough for both.
pub(crate) const INSPECTOR_W: f32 = 360.0;

/// The sizes offered for words.
const TEXT_SIZES: [u32; 8] = [24, 36, 48, 64, 72, 96, 144, 200];
/// Image files offered.
pub(crate) const IMAGE_EXTS: [&str; 6] = ["png", "jpg", "jpeg", "webp", "bmp", "gif"];

/// A source's symbol, in the list and over its settings.
pub(crate) fn layer_icon(app: &App, layer: Layer) -> &'static str {
    match layer {
        Layer::Screen if matches!(app.settings.capture, CaptureTarget::Apps { .. }) => icon::APP_WINDOW,
        Layer::Screen => icon::MONITOR,
        Layer::Webcam(_) => icon::WEBCAM,
        Layer::Visual(key) => match app.settings.visual(key).map(|v| &v.visual) {
            Some(Visual::Window { .. }) => icon::APP_WINDOW,
            Some(Visual::Image { .. }) => icon::IMAGE,
            Some(Visual::Colour { .. }) => icon::PAINT_BUCKET,
            Some(Visual::Text(_)) => icon::TEXT_T,
            Some(Visual::Browser { .. }) => icon::GLOBE,
            _ => icon::MONITOR,
        },
    }
}

impl App {
    /// The panel's content for what's selected; a change to the video's
    /// sources made here can be undone (typing a word, or stepping a
    /// number, as one step).
    pub(crate) fn inspector(&mut self, ui: &mut egui::Ui, frame: Option<&capture::preview::PreviewFrame>, aspect: f32) {
        let was = self.arrangement();
        let what = format!("{:?}", self.canvas.selected);
        match self.canvas.selected.clone() {
            None => self.inspector_empty(ui),
            Some(Selected::Layer(l)) if self.layer_order().contains(&l) => self.layer_inspector(ui, l, frame, aspect),
            Some(Selected::Layer(_)) => self.canvas.selected = None,
            Some(Selected::Sound(id)) => self.sound_inspector(ui, &id),
            Some(Selected::Mix) => self.mix_inspector(ui),
        }
        if self.arrangement() != was {
            self.keep_for_undo_as(was, what);
        }
    }

    /// Nothing selected: how to select something, and the keys.
    fn inspector_empty(&mut self, ui: &mut egui::Ui) {
        let pal = theme::pal(ui);
        ui.add_space(36.0);
        ui.vertical_centered(|ui| {
            ui.label(theme::ic(icon::CURSOR_CLICK, 26.0).color(pal.text3));
            ui.add_space(8.0);
            ui.label(theme::text("Nothing selected", 15.0, W::Semibold).color(pal.text));
            ui.add_space(4.0);
            ui.add(egui::Label::new(theme::text("Click a source on the preview, or in a list under it, to change it here.", 13.0, W::Regular).color(pal.text3)).wrap());
        });
        ui_kit::section_head(ui, "Keys", false, |_| {});
        let ctrl = crate::shortcuts::ctrl_name();
        let keys: [(&str, Vec<&str>); 7] = [
            ("Remove", vec!["Del"]),
            ("Nudge (Shift: 10 px)", vec!["←", "↑", "→", "↓"]),
            ("Hide or show", vec!["H"]),
            ("Duplicate", vec![ctrl, "D"]),
            ("Back, forward", vec![ctrl, "[", "]"]),
            ("Undo, redo", vec![ctrl, "Z"]),
            ("Let go", vec!["Esc"]),
        ];
        ui_kit::list(ui, "inspector_keys", |l| {
            for (what, k) in keys {
                let k: Vec<String> = k.into_iter().map(str::to_owned).collect();
                l.row(Row::new(what), |ui| {
                    let w = ui_kit::key_hint_width(ui, &k, 12.0);
                    let (r, _) = ui.allocate_exact_size(egui::vec2(w, 20.0), egui::Sense::hover());
                    ui_kit::key_hint(ui.painter(), egui::pos2(r.left(), r.center().y), &k, 12.0, pal.text2);
                });
            }
        });
    }

    /// A video source's settings: its head, its own section, Transform and
    /// Crop; a webcam's camera settings after those.
    fn layer_inspector(&mut self, ui: &mut egui::Ui, layer: Layer, frame: Option<&capture::preview::PreviewFrame>, aspect: f32) {
        self.inspector_head(ui, layer, frame, aspect);
        match layer {
            Layer::Screen => self.screen_section(ui, frame),
            Layer::Webcam(key) => self.webcam_section(ui, key),
            Layer::Visual(key) => self.visual_section(ui, key),
        }
        self.transform_section(ui, layer);
        if self.layer_cropped(layer).is_some() {
            self.crop_section(ui, layer, aspect);
        }
        if let Layer::Webcam(key) = layer {
            self.camera_settings_sections(ui, key);
        }
        ui.add_space(28.0);
    }

    /// Its symbol, its name (click to rename), its switch and its menu;
    /// under them what it is now, and what's wrong with it.
    fn inspector_head(&mut self, ui: &mut egui::Ui, layer: Layer, frame: Option<&capture::preview::PreviewFrame>, aspect: f32) {
        let pal = theme::pal(ui);
        let rename_id = egui::Id::new(("rename_layer", layer));
        let draft_id = rename_id.with("draft");
        let renaming = ui.data(|d| d.get_temp::<String>(draft_id));
        let title = self.settings.layer_title(layer);
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.set_min_height(34.0);
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.label(theme::ic(layer_icon(self, layer), 18.0).color(pal.text2));
            let name_w = (ui.available_width() - 90.0).max(60.0);
            ui.allocate_ui_with_layout(egui::vec2(name_w, 30.0), egui::Layout::left_to_right(egui::Align::Center), |ui| match renaming {
                Some(mut draft) => {
                    let done = crate::sources_ui::name_field(ui, rename_id, &mut draft, theme::font(16.0, W::Semibold), name_w - 8.0);
                    if done {
                        self.settings.set_layer_title(layer, &draft);
                        ui.data_mut(|d| d.remove::<String>(draft_id));
                    } else {
                        ui.data_mut(|d| d.insert_temp(draft_id, draft));
                    }
                }
                None => {
                    crate::sources_ui::forget_name_field(ui, rename_id);
                    let r = ui.add(egui::Label::new(theme::text(&title, 16.0, W::Semibold).color(pal.text)).truncate().sense(egui::Sense::click()));
                    if r.on_hover_text("Click to rename it").on_hover_cursor(egui::CursorIcon::Text).clicked() {
                        ui.data_mut(|d| d.insert_temp(draft_id, title.clone()));
                    }
                }
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                let more = Btn::icon(Kind::Ghost, icon::DOTS_THREE).small().tip("Reset, turn, flip, order, duplicate, hide, remove").show(ui);
                ui_kit::menu_popup(&more, false, 240.0, |ui| self.layer_menu(ui, layer, aspect));
                let mut on = self.layer_enabled(layer);
                let tip = if on { "In your clips: off keeps its settings for later" } else { "Out of your clips: on puts it back" };
                if ui_kit::switch(ui, &mut on).on_hover_text(tip).changed() {
                    self.set_layer_enabled(layer, on);
                }
            });
        });
        let (line, warn) = self.layer_line(layer, frame);
        let pad = 26.0;
        ui.horizontal(|ui| {
            ui.add_space(pad);
            ui.add(egui::Label::new(theme::text(line, 12.5, W::Regular).color(pal.text3)).truncate());
        });
        if let Some(w) = warn {
            ui.horizontal(|ui| {
                ui.add_space(pad);
                ui.add(egui::Label::new(theme::text(w, 12.5, W::Regular).color(pal.warn)).wrap());
            });
        }
    }

    /// What a source is now, in a few words (its kind, then what it shows),
    /// and what's wrong with it, if anything: for its row in the list and
    /// over its settings.
    pub(crate) fn layer_line(&self, layer: Layer, frame: Option<&capture::preview::PreviewFrame>) -> (String, Option<String>) {
        match layer {
            Layer::Screen => {
                let apps = matches!(self.settings.capture, CaptureTarget::Apps { .. }) && capture::app_capture();
                if apps {
                    let line = match frame.filter(|f| !f.waiting).and_then(|f| f.app.clone()) {
                        Some(app) => format!("Following {app}"),
                        None => "None of them showing".to_owned(),
                    };
                    (line, None)
                } else {
                    let display = self.screens.get(self.display_at()).map_or_else(|| "Main display".to_owned(), |d| d.name.clone());
                    let cropped = self.crop_whole().zip(self.settings.screen_crop.filter(|c| !c.is_none())).map(|((w, h), c)| capture::Crop::from(c).rect(w, h));
                    let line = match cropped {
                        Some([_, _, w, h]) => format!("{display} · cropped to {w}×{h}"),
                        None => display,
                    };
                    (line, None)
                }
            }
            Layer::Webcam(key) => {
                let Some(w) = self.settings.webcam(key) else { return (String::new(), None) };
                let warn = match capture::webcam::status_of(&w.id) {
                    capture::webcam::Status::Unavailable(why) if w.enabled => {
                        let mut why = why;
                        if let Some(first) = why.get(..1) {
                            why.replace_range(..1, &first.to_uppercase());
                        }
                        Some(format!("{why}."))
                    }
                    _ => None,
                };
                let state = match capture::webcam::status_of(&w.id) {
                    capture::webcam::Status::Opening if w.enabled => " · opening…",
                    _ => "",
                };
                (format!("{}{state}", w.name), warn)
            }
            Layer::Visual(key) => (self.visual_line(key), self.visual_state(key).map(str::to_owned)),
        }
    }

    // --- What each kind shows ---

    /// The screen: what's recorded (all of a display, or your games and
    /// apps), which display or which games, and how it fills the video.
    fn screen_section(&mut self, ui: &mut egui::Ui, frame: Option<&capture::preview::PreviewFrame>) {
        let idle = self.rec_state.idle();
        let apps_mode = matches!(self.settings.capture, CaptureTarget::Apps { .. });
        ui_kit::section_head(ui, if apps_mode && capture::app_capture() { "Games and apps" } else { "Screen" }, true, |_| {});
        ui_kit::list(ui, "inspector_screen", |l| {
            // What to record: changes right away, while capturing too (the
            // clip switches mid-way; nothing restarts).
            if capture::app_capture() {
                let opts = [
                    ui_kit::Seg { icon: None, label: "Whole screen", tip: Some("Everything on one display") },
                    ui_kit::Seg { icon: None, label: "Games and apps", tip: Some("Only the games and apps you pick, following whichever you're using") },
                ];
                let (picked, _) = l.row(Row::new("Record"), |ui| ui_kit::segmented_opts(ui, "capture_kind", &opts, usize::from(apps_mode), None, true));
                if let Some(i) = picked {
                    if i == 0 && apps_mode {
                        let apps = std::mem::replace(&mut self.settings.capture, CaptureTarget::Screen);
                        if matches!(apps, CaptureTarget::Apps { .. }) {
                            self.settings.idle_apps = Some(apps);
                        }
                    } else if i == 1 && !apps_mode {
                        self.settings.capture = self.settings.idle_apps.take().unwrap_or(CaptureTarget::Apps { apps: Vec::new(), away_screen: true });
                        self.windowed_apps = capture::list_windowed_apps();
                    }
                }
            }
            match self.settings.capture.clone() {
                CaptureTarget::Apps { apps, away_screen } if capture::app_capture() => {
                    ui_kit::divider(l.ui, 0.0);
                    egui::Frame::new().inner_margin(egui::Margin::symmetric(16, 2)).show(l.ui, |ui| self.app_rows(ui, apps, away_screen, frame));
                }
                // Linux on Wayland: the desktop's own dialog picks the screen (once; it's remembered).
                _ if cfg!(target_os = "linux") && !capture::screens_listed() => {
                    let (clicked, _) = l.row(Row::new("Display").note("Picked in your desktop's sharing dialog"), |ui| {
                        Btn::normal("Choose another…").small().tip("Your desktop asks again which screen to record").locked((!idle).then(|| "Stop capturing to switch screens".to_owned())).show(ui).clicked()
                    });
                    if clicked {
                        capture::choose_screen_again();
                        self.video_preview = None; // restarts, and asks
                    }
                }
                _ => {
                    let names: Vec<String> = self.screens.iter().map(|d| d.name.clone()).collect();
                    let at = self.display_at();
                    // While clipping, the video stays the size it started at:
                    // a display that would make it another size waits.
                    let recording = frame.map(|f| f.recorded).filter(|_| !idle);
                    let sized: Vec<Option<(u32, u32)>> = match recording {
                        Some(_) => self.screens.iter().map(|d| capture::frame_size(&self.display_source(&d.id), self.settings.video_size())).collect(),
                        None => Vec::new(),
                    };
                    let waits = |i: usize| recording.is_some_and(|now| sized.get(i).copied().flatten().is_some_and(|size| size != now));
                    let stop = format!("Stop {}", self.rec_state.words());
                    let mut pick = None;
                    l.row(Row::new("Display"), |ui| {
                        let current = names.get(at).cloned().unwrap_or_else(|| "Main display".to_owned());
                        let r = ui_kit::select(ui, &current, 170.0);
                        ui_kit::menu_popup(&r, false, 240.0, |ui| {
                            for (i, name) in names.iter().enumerate() {
                                let mut item = Item::new(name.clone()).check(at == i);
                                if waits(i) {
                                    let (w, h) = sized[i].unwrap_or_default();
                                    item = item.enabled(false).value("Stop clipping first").tip(format!("It would make your video {w}×{h}, and its size is set when clipping starts. {stop} to switch, or give your video a size of its own."));
                                }
                                if item.show(ui).clicked() {
                                    pick = Some(i);
                                }
                            }
                        });
                    });
                    if let Some(i) = pick {
                        self.settings.display_id = Some(self.screens[i].id.clone());
                        self.settings.display_index = i;
                    }
                }
            }
            // How the video puts it in, while it isn't placed by hand.
            let now = self.settings.screen.placed.is_none().then_some(self.settings.framing);
            let note = if now.is_none() { "Placed by hand: pick one to fill the video again" } else { "" };
            let (f, _) = l.row(Row::new("Fill the video").note(note), |ui| crate::canvas_ui::framing_choice(ui, "inspector_framing", now));
            if let Some(f) = f {
                self.screen_framed(f);
            }
        });
    }

    /// A webcam: which camera, and in what size and frame rate.
    fn webcam_section(&mut self, ui: &mut egui::Ui, key: u64) {
        let Some(cam) = self.settings.webcam(key).cloned() else { return };
        ui_kit::section_head(ui, "Camera", true, |_| {});
        let mut camera_pick: Option<capture::Device> = None;
        let mut format_pick: Option<Option<capture::webcam::Format>> = None;
        let formats = capture::webcam::formats(&cam.id);
        let current = cam.format.map(capture::webcam::Format::from);
        let taken: Vec<String> = self.settings.webcams.iter().filter(|w| w.key != key).map(|w| w.id.clone()).collect();
        let mut refresh = false;
        let cameras = self.cameras().to_vec();
        ui_kit::list(ui, "inspector_camera", |l| {
            l.row(Row::new("Camera"), |ui| {
                let r = ui_kit::select(ui, &cam.name, 190.0);
                refresh = r.clicked();
                ui_kit::menu_popup(&r, false, 240.0, |ui| {
                    for c in &cameras {
                        // A camera opens once: one another webcam shows can't be this one's too.
                        let shown = taken.contains(&c.id);
                        let item = Item::new(c.name.clone()).check(c.id == cam.id).enabled(!shown);
                        let item = if shown { item.tip("Another webcam in your clips shows it") } else { item };
                        if item.show(ui).clicked() && c.id != cam.id {
                            camera_pick = Some(c.clone());
                        }
                    }
                    if cameras.is_empty() {
                        ui_kit::menu_head(ui, "No cameras found.");
                    }
                });
            });
            l.row(Row::new("Resolution"), |ui| {
                let label = current.map_or_else(|| "Automatic".to_owned(), |f| crate::webcam_ui::size_label(f.width, f.height));
                let r = ui_kit::select(ui, &label, 150.0);
                ui_kit::menu_popup(&r, false, 250.0, |ui| {
                    egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                        if Item::new("Automatic").check(current.is_none()).tip("The sharpest picture up to 1080p at 30 fps or more").show(ui).clicked() {
                            format_pick = Some(None);
                        }
                        if formats.is_empty() {
                            ui_kit::menu_head(ui, "The camera's sizes show once it's open.");
                        }
                        for (w, h) in crate::webcam_ui::sizes(&formats) {
                            let on = current.is_some_and(|f| (f.width, f.height) == (w, h));
                            let fastest = formats.iter().filter(|f| (f.width, f.height) == (w, h)).map(|f| f.fps()).fold(0.0, f32::max);
                            if Item::new(crate::webcam_ui::size_label(w, h)).value(format!("up to {}", crate::webcam_ui::fps_label(fastest))).check(on).show(ui).clicked() && !on {
                                format_pick = Some(crate::webcam_ui::rate_for(&formats, (w, h), current));
                            }
                        }
                    });
                });
            });
            if let Some(f) = current {
                l.row(Row::new("Frame rate"), |ui| {
                    let r = ui_kit::select(ui, &crate::webcam_ui::fps_label(f.fps()), 150.0);
                    ui_kit::menu_popup(&r, false, 200.0, |ui| {
                        for o in formats.iter().filter(|o| (o.width, o.height) == (f.width, f.height)) {
                            if Item::new(crate::webcam_ui::fps_label(o.fps())).check(*o == f).show(ui).clicked() && *o != f {
                                format_pick = Some(Some(*o));
                            }
                        }
                    });
                });
            }
        });
        if refresh {
            self.refresh_cameras();
        }
        if let Some(c) = camera_pick {
            self.webcam_refit_later(key, &cam.id);
            if let Some(w) = self.settings.webcam_mut(key) {
                w.id = c.id;
                w.name = c.name;
                w.format = None; // that camera's own formats
            }
        }
        if let Some(p) = format_pick {
            self.webcam_refit_later(key, &cam.id);
            if let Some(w) = self.settings.webcam_mut(key) {
                w.format = p.map(Into::into);
            }
        }
    }

    /// A boxed source: which display or window, the image, the colour, the
    /// words, the page.
    fn visual_section(&mut self, ui: &mut egui::Ui, key: u64) {
        let Some(v) = self.settings.visual(key).cloned() else { return };
        if let Visual::Text(style) = &v.visual {
            self.text_section(ui, key, style.clone());
            return;
        }
        ui_kit::section_head(ui, v.visual.kind_name(), true, |_| {});
        let mut set: Option<Visual> = None;
        let mut refresh_displays = false;
        let mut refresh_windows = false;
        let displays = self.sources_view.add_displays.clone();
        let windows = self.windowed_apps.clone();
        ui_kit::list(ui, ("inspector_visual", key), |l| match &v.visual {
            Visual::Display { id, name } => {
                l.row(Row::new("Display"), |ui| {
                    let r = ui_kit::select(ui, name, 170.0);
                    refresh_displays = r.clicked();
                    ui_kit::menu_popup(&r, false, 240.0, |ui| {
                        for s in &displays {
                            if Item::new(s.name.clone()).check(s.id == *id).show(ui).clicked() && s.id != *id {
                                set = Some(Visual::Display { id: s.id.clone(), name: s.name.clone() });
                            }
                        }
                        if displays.is_empty() {
                            ui_kit::menu_head(ui, "No displays found.");
                        }
                    });
                });
            }
            Visual::Window { app, name } => {
                l.row(Row::new("App").note("Its main window, without its title bar"), |ui| {
                    let r = ui_kit::select(ui, name, 170.0);
                    refresh_windows = r.clicked();
                    ui_kit::menu_popup(&r, false, 260.0, |ui| {
                        egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                            for a in &windows {
                                let tex = self.app_icons.get(ui.ctx(), &a.id).map(|t| t.id());
                                if Item::new(a.name.clone()).image(tex).check(a.id == *app).show(ui).clicked() && a.id != *app {
                                    set = Some(Visual::Window { app: a.id.clone(), name: a.name.clone() });
                                }
                            }
                        });
                        if windows.is_empty() {
                            ui_kit::menu_head(ui, "No windows found.");
                        }
                    });
                });
            }
            Visual::Image { path } => {
                let name = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
                l.row(Row::new("File").note(name), |ui| {
                    if Btn::normal("Choose…").small().tip("Another picture").show(ui).clicked()
                        && let Some(p) = rfd::FileDialog::new().add_filter("Images", &IMAGE_EXTS).pick_file()
                    {
                        set = Some(Visual::Image { path: p });
                    }
                });
            }
            Visual::Colour { rgba } => {
                l.row(Row::new("Colour"), |ui| {
                    if let Some(c) = crate::visuals_ui::colour_button(ui, *rgba) {
                        set = Some(Visual::Colour { rgba: c });
                    }
                });
            }
            Visual::Browser { url, width, height, reload } => {
                // Typed in full, then taken when it's done (Enter, or
                // clicking away): each key would load the page again.
                let draft_id = egui::Id::new(("browser_url", key));
                let mut draft: String = l.ui.data(|d| d.get_temp(draft_id)).unwrap_or_else(|| url.clone());
                let mut done = false;
                wide_row(l, "Address", &mut |ui| {
                    // Free text: a web address.
                    let out = ui_kit::text_field(ui, &mut draft, "https://");
                    done = out.response.lost_focus();
                });
                if done {
                    l.ui.data_mut(|d| d.remove::<String>(draft_id));
                    let typed = draft.trim().to_owned();
                    if typed != *url && !typed.is_empty() {
                        let typed = if typed.contains("://") { typed } else { format!("https://{typed}") };
                        set = Some(Visual::Browser { url: typed, width: *width, height: *height, reload: *reload });
                    }
                } else {
                    l.ui.data_mut(|d| d.insert_temp(draft_id, draft));
                }
                l.row(Row::new("Page size").note("How big it's drawn, as a browser window would be"), |ui| {
                    if let Some((w, h)) = ui_kit::size_field(ui, &format!("browser_size_{key}"), (*width, *height), 16..=3840, 16..=2160) {
                        set = Some(Visual::Browser { url: url.clone(), width: w, height: h, reload: *reload });
                    }
                });
                l.row(Row::new("Page").note("Transparent where the page is"), |ui| {
                    if Btn::normal("Reload").with_icon(icon::ARROWS_CLOCKWISE).small().tip("Load the page again").show(ui).clicked() {
                        set = Some(Visual::Browser { url: url.clone(), width: *width, height: *height, reload: reload.wrapping_add(1) });
                    }
                });
            }
            Visual::Text(_) => {}
        });
        if refresh_displays {
            self.sources_view.add_displays = capture::list_screens();
        }
        if refresh_windows {
            self.windowed_apps = capture::list_windowed_apps();
        }
        if let Some(new) = set {
            // Another picture is another shape: it goes where it goes at first.
            let reshaped = matches!(new, Visual::Display { .. } | Visual::Window { .. } | Visual::Image { .. });
            if let Some(d) = self.settings.visual_mut(key) {
                d.visual = new;
                if reshaped {
                    d.placed = None;
                    d.crop = None;
                }
            }
            self.canvas.sizes.remove(&key);
        }
    }

    /// Words: what they say, their font, style, size, colour and lining
    /// up; an outline round them, a shadow under them.
    fn text_section(&mut self, ui: &mut egui::Ui, key: u64, style: TextStyle) {
        let mut s = style.clone();
        ui_kit::section_head(ui, "Text", true, |_| {});
        let families = crate::fonts::families();
        ui_kit::list(ui, ("inspector_text", key), |l| {
            wide_row(l, "Words", &mut |ui| {
                // Free text: the words a text source shows, a line each.
                #[allow(clippy::disallowed_methods)]
                let edit = egui::TextEdit::multiline(&mut s.text).desired_rows(2).desired_width(f32::INFINITY).font(theme::font(14.0, W::Regular));
                let pal = theme::pal(ui);
                egui::Frame::new().fill(pal.control).corner_radius(ui_kit::RADIUS).inner_margin(egui::Margin::symmetric(10, 7)).show(ui, |ui| ui.add(edit));
            });
            l.row(Row::new("Font"), |ui| {
                let shown = if s.font.is_empty() { crate::fonts::OWN.to_owned() } else { s.font.clone() };
                let r = ui_kit::select(ui, &shown, 170.0);
                // Typing in its box mustn't close it: only a click outside does.
                egui::Popup::from_toggle_button_response(&r)
                    .id(r.id.with("fonts"))
                    .gap(6.0)
                    .align(egui::RectAlign::BOTTOM_END)
                    .frame(ui_kit::menu_frame(ui))
                    .style(ui_kit::menu_style)
                    .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                    .show(|ui| ui_kit::menu_body(ui, 260.0, |ui| font_menu(ui, families.as_deref(), &mut s.font)));
            });
            let fam = if s.font.is_empty() { None } else { crate::fonts::family(&s.font) };
            let styles = fam.map_or_else(|| (100..=900).step_by(100).map(|w| (w as u16, false)).collect(), |f| f.styles());
            l.row(Row::new("Style"), |ui| {
                let r = ui_kit::select(ui, &crate::fonts::style_name(s.weight, s.italic), 150.0);
                ui_kit::menu_popup(&r, false, 200.0, |ui| {
                    egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                        for (w, i) in &styles {
                            if Item::new(crate::fonts::style_name(*w, *i)).check((s.weight, s.italic) == (*w, *i)).show(ui).clicked() {
                                (s.weight, s.italic) = (*w, *i);
                            }
                        }
                    });
                });
            });
            l.row(Row::new("Size").note("How tall its letters are in a 1080p video"), |ui| {
                let field = ui_kit::ValueField {
                    id: egui::Id::new(("text_size", key)),
                    value: f64::from(s.size),
                    decimals: 0,
                    range: 4.0..=600.0,
                    unit: "px",
                    width: 70.0,
                    hint: "How tall its letters are, in pixels of a 1080p video",
                    presets: TEXT_SIZES.iter().map(|&v| ui_kit::Preset::new(format!("{v} px"), f64::from(v))).collect(),
                };
                if let Some(n) = field.show(ui) {
                    s.size = n.round() as u32;
                }
            });
            l.row(Row::new("Colour"), |ui| {
                if let Some(c) = crate::visuals_ui::colour_button(ui, s.rgba) {
                    s.rgba = c;
                }
            });
            l.row(Row::new("Line up"), |ui| {
                const ALIGNS: [TextAlign; 3] = [TextAlign::Left, TextAlign::Centre, TextAlign::Right];
                let opts = [
                    ui_kit::Seg { icon: Some(icon::TEXT_ALIGN_LEFT), label: "", tip: Some("Lines start at the left") },
                    ui_kit::Seg { icon: Some(icon::TEXT_ALIGN_CENTER), label: "", tip: Some("Lines in the middle") },
                    ui_kit::Seg { icon: Some(icon::TEXT_ALIGN_RIGHT), label: "", tip: Some("Lines end at the right") },
                ];
                let now = ALIGNS.iter().position(|a| *a == s.align).unwrap_or(1);
                if let Some(i) = ui_kit::segmented_opts(ui, ("text_align", key), &opts, now, None, true) {
                    s.align = ALIGNS[i];
                }
            });
            l.row(Row::new("Line spacing"), |ui| {
                let field = ui_kit::ValueField {
                    id: egui::Id::new(("text_spacing", key)),
                    value: f64::from(s.line_spacing),
                    decimals: 0,
                    range: 50.0..=300.0,
                    unit: "%",
                    width: 70.0,
                    hint: "Between its lines, as much of the font's own spacing",
                    presets: [80u32, 100, 120, 150].iter().map(|&v| ui_kit::Preset::new(format!("{v} %"), f64::from(v))).collect(),
                };
                if let Some(n) = field.show(ui) {
                    s.line_spacing = n.round() as u32;
                }
            });
        });
        ui_kit::section_head(ui, "Outline and shadow", false, |_| {});
        ui_kit::list(ui, ("inspector_text_look", key), |l| {
            l.row(Row::new("Outline").note("A line round the letters"), |ui| {
                ui_kit::switch(ui, &mut s.outline);
            });
            let outline = s.outline;
            l.well(("text_outline", key), outline, |l| {
                l.row(Row::new("Width").kid(), |ui| {
                    if let Some(v) = px_field(ui, ("outline_w", key), s.outline_width as i64, 1..=60) {
                        s.outline_width = v as u32;
                    }
                });
                l.row(Row::new("Colour").kid(), |ui| {
                    if let Some(c) = crate::visuals_ui::colour_button(ui, s.outline_rgba) {
                        s.outline_rgba = c;
                    }
                });
            });
            l.row(Row::new("Shadow").note("Under the letters, so they read on anything"), |ui| {
                ui_kit::switch(ui, &mut s.shadow);
            });
            let shadow = s.shadow;
            l.well(("text_shadow", key), shadow, |l| {
                l.row(Row::new("Moved").note("Right and down, in pixels").kid(), |ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    let pal = theme::pal(ui);
                    ui.label(theme::text("px", 12.5, W::Regular).color(pal.text3));
                    if let Some(v) = ui_kit::signed_box(ui, egui::Id::new(("shadow_y", key)), i64::from(s.shadow_y), -200..=200, 56.0) {
                        s.shadow_y = v as i32;
                    }
                    if let Some(v) = ui_kit::signed_box(ui, egui::Id::new(("shadow_x", key)), i64::from(s.shadow_x), -200..=200, 56.0) {
                        s.shadow_x = v as i32;
                    }
                });
                l.row(Row::new("Blur").kid(), |ui| {
                    if let Some(v) = px_field(ui, ("shadow_blur", key), i64::from(s.shadow_blur), 0..=100) {
                        s.shadow_blur = v as u32;
                    }
                });
                l.row(Row::new("Colour").note("Its opacity too").kid(), |ui| {
                    if let Some(c) = crate::visuals_ui::colour_button(ui, s.shadow_rgba) {
                        s.shadow_rgba = c;
                    }
                });
            });
        });
        if s != style
            && let Some(v) = self.settings.visual_mut(key)
        {
            v.visual = Visual::Text(s);
        }
    }
}

/// A row whose control is as wide as the row, under its name (an address,
/// words).
pub(crate) fn wide_row(l: &mut ui_kit::List, label: &str, add: &mut dyn FnMut(&mut egui::Ui)) {
    let pal = theme::pal(l.ui);
    l.row(Row::new(label), |_| {});
    egui::Frame::new().inner_margin(egui::Margin { left: 16, right: 16, top: 0, bottom: 12 }).show(l.ui, |ui| {
        ui.set_width(ui.available_width());
        let _ = pal;
        add(ui);
    });
}

/// A number of pixels, in its box with "px" after it: the new number, once
/// it's typed.
pub(crate) fn px_field(ui: &mut egui::Ui, id: impl std::hash::Hash + std::fmt::Debug, value: i64, range: std::ops::RangeInclusive<i64>) -> Option<i64> {
    let pal = theme::pal(ui);
    ui.spacing_mut().item_spacing.x = 6.0;
    ui.label(theme::text("px", 12.5, W::Regular).color(pal.text3));
    ui_kit::signed_box(ui, egui::Id::new(id), value, range, 56.0)
}

/// The fonts to pick from: the app's own first, then what's installed, a
/// box over them to find one by name.
fn font_menu(ui: &mut egui::Ui, families: Option<&Vec<crate::fonts::Family>>, font: &mut String) {
    let Some(families) = families else {
        ui_kit::menu_head(ui, "Finding your fonts…");
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
        return;
    };
    let find_id = egui::Id::new("font_menu_find");
    let mut find: String = ui.data(|d| d.get_temp(find_id)).unwrap_or_default();
    ui.horizontal(|ui| {
        ui.add_space(6.0);
        ui.set_width(ui.available_width() - 6.0);
        // Free text: part of a font's name, to find it.
        ui_kit::text_field(ui, &mut find, "Find a font");
    });
    ui.data_mut(|d| d.insert_temp(find_id, find.clone()));
    ui.add_space(4.0);
    let needle = find.trim().to_lowercase();
    egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
        let mut any = false;
        for f in families.iter().filter(|f| needle.is_empty() || f.name.to_lowercase().contains(&needle)) {
            any = true;
            let own = f.name == crate::fonts::OWN;
            let on = if own { font.is_empty() || font.as_str() == crate::fonts::OWN } else { font.eq_ignore_ascii_case(&f.name) };
            let item = Item::new(f.name.clone()).check(on);
            let item = if own { item.value("HasteClip's") } else { item };
            if item.show(ui).clicked() {
                *font = if own { String::new() } else { f.name.clone() };
            }
        }
        if !any {
            ui_kit::menu_head(ui, "No font by that name.");
        }
    });
}
