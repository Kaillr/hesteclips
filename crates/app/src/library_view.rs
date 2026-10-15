//! The library as you see it: the Clips panel (search, All clips, your games,
//! your collections) beside the clip grid.
//!
//! - **The panel** exists only on Clips. Its card is glass: the clips beside
//!   it, blurred, light it from behind (`glass.rs`).
//! - **Cards are footage first**: no borders or badges. Hover shows Edit and
//!   Share as icons and a selection circle; a new clip has a dot by its name;
//!   an edited one a scissors and its new length. The line under a clip is its
//!   game and file size (what matters when sharing); hover the size for when.
//! - **Selection floats**: a bar rises at the bottom instead of pushing the
//!   grid down.
//! - **Motion**: when the grid is laid out again (another game or collection,
//!   a search, another order, clips coming and going, another number of
//!   columns) the cards and day headings still shown slide to their new
//!   places and new ones fade in, instead of the page fading in again; a day
//!   folds with its height; hovering lifts a card's picture a little.

use std::path::PathBuf;
use std::time::Duration;

use egui::{Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2, vec2};

use crate::library::{Action, CardDrag, DropOn, Filter, View};
use crate::settings::LibrarySort;
use crate::theme::{self, W, icon};
use crate::ui_kit::{self, Btn, Item, Kind};
use crate::{App, RecState, clips, share, thumbs};

/// The panel's width, from the rail's edge (its card's right edge is where
/// Settings' search ends too).
pub const PANEL_W: f32 = 244.0;
use crate::library_grid::{DAY_H, GridItem, Key};
/// The window is too narrow for the panel below this page width: its list
/// moves into a menu on the title.
const PANEL_FROM: f32 = 700.0;
/// How long a freshly saved clip has its dot.
const NEW_FOR: Duration = Duration::from_secs(60 * 30);

impl App {
    pub(crate) fn library(&mut self, ui: &mut egui::Ui, frame: &eframe::Frame) {
        self.hover_seen = false;
        // One card for a recording in progress, and one per clip still being written.
        let placeholders = usize::from(self.rec_state == RecState::Recording) + self.saving;
        let new_renders: Vec<usize> = (0..self.renders.len()).filter(|&i| self.renders[i].as_new).collect();
        let empty_library = self.clips.is_empty() && placeholders == 0 && new_renders.is_empty();
        // What's shown: worked out again only when something it's made of
        // changed (see `library_grid`), not every frame.
        let today_folded = self.folded.contains(&chrono::Local::now().date_naive());
        let live = placeholders > 0 || !new_renders.is_empty();
        if live && today_folded && self.library_query.trim().is_empty() {
            // New cards (a recording, a clip being saved) belong to today: it opens.
            self.folded.set(chrono::Local::now().date_naive(), false);
        }
        let thumbs = &mut self.thumbs;
        let lengths_version = thumbs.lengths_version();
        let render_dests: Vec<PathBuf> = self.renders.iter().map(|r| r.dest.clone()).collect();
        self.shown.update(
            crate::library_grid::Inputs {
                clips: &self.clips,
                library_version: self.library_version,
                collections: &self.collections,
                filter: &self.library_filter,
                query: &self.library_query,
                sort: self.settings.library_sort,
                lengths_version,
                placeholders,
                new_renders: new_renders.clone(),
                renders: &render_dests,
            },
            false,
            |c| thumbs.duration(c),
        );
        let (games, collections) = (self.shown.games.clone(), self.shown.collections.clone());
        let mut folders: Vec<String> = games.iter().filter_map(|c| if let Filter::Folder(f) = &c.filter { Some(f.clone()) } else { None }).collect();
        folders.sort_by_key(|f| f.to_lowercase());

        let mut action = self.selection_keys(ui);
        if let Some(a) = self.follow_drag(ui.ctx()) {
            action = Some(a);
        }
        // ⌘F: search.
        if !ui_kit::overlay_open(ui.ctx()) && ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::F)) {
            ui.memory_mut(|m| m.request_focus(egui::Id::new("library_search")));
        }

        let full = ui.max_rect();
        let wide = full.width() >= PANEL_FROM && !empty_library;
        // The panel: from the rail's edge (the page's margin is inside it).
        let panel = Rect::from_min_max(Pos2::new(full.left() - ui_kit::PAGE_MARGIN as f32, full.top()), Pos2::new(full.left() - ui_kit::PAGE_MARGIN as f32 + PANEL_W, full.bottom()));
        let page_left = if wide { panel.right() + 24.0 } else { full.left() };
        let page = Rect::from_min_max(Pos2::new(page_left, full.top()), full.max);
        let mut page_ui = ui.new_child(egui::UiBuilder::new().max_rect(page).layout(egui::Layout::top_down(egui::Align::Min)));

        if empty_library {
            self.empty_state(&mut page_ui);
            return;
        }

        let query = self.library_query.trim().to_lowercase();
        // A clip being brought back into view unfolds its day.
        if let Some(day) = self.reveal_clip.as_ref().and_then(|p| self.clips.iter().find(|c| &c.path == p)).map(|c| c.day()) {
            self.folded.set(day, false);
        }
        let days = self.shown.days.clone();
        let shown_count = self.shown.clips.len();

        // The heading: what's shown, how many, the order, fold all; recorder.
        let title = self.filter_name(&self.library_filter);
        let in_filter = self.shown.in_filter;
        let count = if query.is_empty() { format!("{in_filter} clips") } else { format!("{shown_count} of {in_filter} clips") };
        let wide_page = wide;
        let (games_c, cols_c) = (games.clone(), collections.clone());
        let mut head_action = None;
        self.page_head_folding(
            &mut page_ui,
            "",
            |app, ui, tight| {
                if !wide_page {
                    if let Some(a) = app.narrow_filter_menu(ui, &title, &games_c, &cols_c) {
                        head_action = Some(a);
                    }
                } else {
                    let r = ui.add(egui::Label::new(theme::text(&title, 15.0, W::Semibold).color(theme::pal(ui).text)).sense(Sense::click()));
                    if let Filter::Collection(_) | Filter::Folder(_) = &app.library_filter {
                        let f = app.library_filter.clone();
                        ui_kit::context_menu(&r, 220.0, |ui| {
                            if let Some(a) = view_menu(ui, &f) {
                                head_action = Some(a);
                            }
                        });
                    }
                }
                ui.label(theme::text(&count, 13.5, W::Regular).color(theme::pal(ui).text3));
                ui.add_space(2.0);
                let sort = app.settings.library_sort;
                let all_folded = days.iter().all(|d| app.folded.contains(d));
                let fold_label = if all_folded { "Expand all" } else { "Collapse all" };
                if tight {
                    // Not enough room: the order and fold-all in a menu (the
                    // recorder keeps its words longer).
                    let r = Btn::icon(Kind::Ghost, icon::DOTS_THREE).small().id("lib_more").tip(format!("Order: {}", sort.label())).show(ui);
                    ui_kit::menu_popup(&r, true, 200.0, |ui| {
                        ui_kit::menu_head(ui, "Order");
                        for s in LibrarySort::ALL {
                            if Item::new(s.label()).check(sort == s).show(ui).clicked() {
                                app.settings.library_sort = s;
                            }
                        }
                        if days.len() > 1 {
                            ui_kit::menu_sep(ui);
                            if Item::new(fold_label).icon(if all_folded { icon::ARROWS_OUT_SIMPLE } else { icon::ARROWS_IN_SIMPLE }).show(ui).clicked() {
                                app.folded.set_all(&days, !all_folded);
                            }
                        }
                    });
                } else {
                    let r = Btn::ghost(sort.label()).with_icon(icon::ARROWS_DOWN_UP).small().id("lib_sort").show(ui);
                    ui_kit::menu_popup(&r, true, 200.0, |ui| {
                        for s in LibrarySort::ALL {
                            if Item::new(s.label()).check(sort == s).show(ui).clicked() {
                                app.settings.library_sort = s;
                            }
                        }
                    });
                    if days.len() > 1 && Btn::ghost(fold_label).small().id("lib_fold").show(ui).clicked() {
                        app.folded.set_all(&days, !all_folded);
                    }
                }
            },
        );
        if head_action.is_some() {
            action = head_action;
        }
        self.permission_banner(&mut page_ui);

        // The grid.
        let grid_rect = page_ui.available_rect_before_wrap();
        let selecting = !self.selection.is_empty();
        // The grid's empty space, under the cards (made first, so a card
        // gets its own clicks): a click deselects, right-click has its menu.
        let bg = page_ui.interact(grid_rect, egui::Id::new("lib_bg"), Sense::click());
        let mut grid_ui = page_ui.new_child(egui::UiBuilder::new().max_rect(grid_rect));
        let mut tiles: Vec<crate::glass::Tile> = Vec::new();
        let mut origin = Pos2::ZERO;
        let mut grid_view = (0.0, 0.0);
        // Another page: the cards are simply in place (and arrive with the
        // page), not sliding over from where they were.
        let arriving = ui_kit::just_arrived(page_ui.ctx());
        let now = page_ui.input(|i| i.time);
        ui_kit::arrive(&mut grid_ui, |ui| ui_kit::scroll(ui, "library_clips", |ui| {
            origin = ui.cursor().min;
            let avail = ui.available_width();
            let m = crate::library_grid::Metrics::for_width(avail);
            if self.shown.clips.is_empty() && placeholders == 0 {
                self.no_results(ui, &query);
            }
            // Fold: a group's height slides; its cards stay put, clipped.
            let sort = self.settings.library_sort as u8;
            let group_id = |day: Option<chrono::NaiveDate>| egui::Id::new(("lib_group", day.map(|d| d.to_string()).unwrap_or_default(), sort));
            let ctx = ui.ctx().clone();
            let opens: Vec<f32> = self.shown.groups.iter().map(|g| theme::toggle(&ctx, group_id(g.day), !g.day.is_some_and(|d| self.folded.contains(&d)), theme::NORMAL)).collect();
            let folding = opens.iter().any(|o| *o > 0.0 && *o < 1.0);
            // The cards' places are found once per change; the days' places
            // every frame (a day folding moves what's below).
            let clips = &self.clips;
            let groups = &self.shown.groups;
            // What's shown changing makes things slide; the window's width
            // doesn't (cards follow it exactly, never trailing): the places
            // are found again, without a slide.
            let id = crate::library_grid::LayoutId { shown: self.shown.version, metrics: (m.cols, avail.round() as i32) };
            let before = &self.shown.before;
            // Folding moves what's below by itself: nothing slides then.
            self.moves.update(
                id,
                now,
                arriving || folding,
                || crate::library_grid::Layout::new(groups, m, |g| opens[g], |it| crate::library_grid::key_for(it, clips, &render_dests)),
                |old| old.resized(groups, m, |g| opens[g]),
                |l| l.place_groups(groups, |g| opens[g]),
                || before.clone(),
            );
            let total = self.moves.layout().height;
            // The view, in the grid's content: what's on screen, and a card's
            // height around it so cards just off it are ready.
            let clip_rect = ui.clip_rect();
            let view = (clip_rect.top() - origin.y, clip_rect.bottom() - origin.y);
            self.moves.set_view(view);
            grid_view = view;
            let reach = (view.0 - m.card.y * 2.0, view.1 + m.card.y * 2.0);
            let moving = self.moves.moving(now);
            if moving {
                ui.ctx().request_repaint();
            }
            // While things move: what was in view before slides out from
            // there, so it's drawn even where it's going is off screen (found
            // by key: a screenful, however long the library).
            let mut seen_at: std::collections::HashMap<usize, Vec<usize>> = std::collections::HashMap::new();
            for (k, _) in self.moves.seen(now) {
                if let Some((g, i)) = self.moves.layout().slot(k) {
                    seen_at.entry(g).or_default().push(i);
                }
            }
            let selecting_now = selecting;
            let n_groups = self.shown.groups.len();
            for gi in 0..n_groups {
                let (day, n_clips, len) = {
                    let g = &self.shown.groups[gi];
                    (g.day, g.clips, g.items.len())
                };
                let place = self.moves.layout().groups[gi];
                let open = opens[gi];
                let folded = day.is_some_and(|d| self.folded.contains(&d));
                // The day's heading, sliding with its clips.
                if let (Some(d), Some(head)) = (day, place.head) {
                    let target = Pos2::new(0.0, head);
                    let p = self.moves.place(&Key::Day(d), target, now, view, None);
                    if (p.at.y + DAY_H >= reach.0 && p.at.y <= reach.1) || (head + DAY_H >= reach.0 && head <= reach.1) {
                        let rect = Rect::from_min_size(origin + p.at.to_vec2(), vec2(avail, DAY_H));
                        let mut hui = ui.new_child(egui::UiBuilder::new().max_rect(rect).id_salt(("day", d)));
                        if p.shown < 1.0 {
                            hui.multiply_opacity(p.shown);
                        }
                        if day_header(&mut hui, &clips::day_label(d), n_clips, folded).clicked() {
                            self.folded.set(d, !folded);
                        }
                    }
                }
                if open <= 0.0 {
                    continue;
                }
                let top = origin + vec2(0.0, place.cards);
                let area = Rect::from_min_size(top, vec2(avail, place.height));
                // The places of the clips that light the panel (its first
                // columns), drawn or not: those within the part of the grid
                // its picture may cover (the view and a screen or so around it).
                if open >= 1.0 {
                    // (One screen more than it covers: a picture made for the
                    // view at either end of what it covers has all its clips.)
                    let ahead = (view.1 - view.0) * (crate::glass::AHEAD * 2.0 + 1.0) + crate::glass::REACH;
                    let near = self.moves.layout().visible(gi, len, view.0 - ahead, view.1 + ahead);
                    for i in near {
                        let off = m.offset(i);
                        if off.x > crate::glass::REACH {
                            continue;
                        }
                        if let GridItem::Clip(ci) = self.shown.groups[gi].items[i] {
                            let c = &self.clips[ci];
                            let rect = Rect::from_min_size(Pos2::new(off.x, place.cards + off.y), vec2(m.card.x, m.card.x * 9.0 / 16.0));
                            let small = self.thumbs.small(&ctx, c);
                            tiles.push(crate::glass::Tile { rect, path: c.path.clone(), small });
                        }
                    }
                }
                // Folding: the cards are cut off at the group's edge. Open, a
                // card sliding to another day can pass over its own.
                let clip = if open < 1.0 { area.intersect(clip_rect) } else { clip_rect };
                // The cards on screen now, by row arithmetic; while things
                // move, also the ones that were in view before (wherever
                // they're going). Nothing else can cross the view.
                let visible = self.moves.layout().visible(gi, len, reach.0, reach.1);
                let mut draw: Vec<usize> = visible.clone().collect();
                if let Some(more) = seen_at.get(&gi) {
                    draw.extend(more.iter().copied().filter(|i| !visible.contains(i) && *i < len));
                }
                for i in draw {
                    let item = self.shown.groups[gi].items[i].clone();
                    let target = Pos2::new(0.0, place.cards) + m.offset(i);
                    let key = crate::library_grid::key_for(&item, &self.clips, &render_dests);
                    let p = self.moves.place(&key, target, now, view, day);
                    let rect = Rect::from_min_size(origin + p.at.to_vec2(), m.card);
                    let mut cui = ui.new_child(egui::UiBuilder::new().max_rect(rect));
                    cui.set_clip_rect(clip);
                    if open * p.shown < 1.0 {
                        cui.multiply_opacity(open * p.shown);
                    }
                    match item {
                        GridItem::Placeholder(_) => self.placeholder_card(&mut cui, rect),
                        GridItem::NewRender(r) => self.new_render_card(&mut cui, r, rect),
                        GridItem::Clip(ci) => {
                            let c = self.clips[ci].clone();
                            if let Some(a) = self.clip_card(&mut cui, &c, rect, &folders, selecting_now) {
                                action = Some(a);
                            }
                        }
                    }
                }
            }
            ui.allocate_space(vec2(avail, total));
            ui.add_space(if selecting { 90.0 } else { 28.0 });
        }));
        if bg.clicked() && selecting {
            action = Some(Action::Deselect);
        }
        if let Some(a) = ui_kit::context_menu(&bg, 230.0, |ui| self.background_menu(ui, &days)).flatten() {
            action = Some(a);
        }
        self.glass_tiles = tiles;
        self.glass_origin = origin;
        self.glass_view = grid_view;
        // The panel, after the grid: its glass follows the grid as it is now.
        if wide {
            let mut panel_ui = ui.new_child(egui::UiBuilder::new().max_rect(panel).layout(egui::Layout::top_down(egui::Align::Min)));
            if let Some(a) = self.panel(&mut panel_ui, &games, &collections) {
                action = Some(a);
            }
        }

        if selecting {
            if let Some(a) = self.selection_bar(ui, page, shown_count, &folders) {
                action = Some(a);
            }
        }
        if let Some(a) = self.finish_drag(ui.ctx()) {
            action = Some(a);
        }
        // Off every card: the clip playing in one stops.
        if !self.hover_seen {
            self.hover_play = None;
        }
        let shown: Vec<clips::Clip> = if action.is_some() { self.shown.clips.iter().map(|&i| self.clips[i].clone()).collect() } else { Vec::new() };
        self.do_action(ui.ctx(), frame, action, &shown);
    }

    /// What a filter is called.
    pub(crate) fn filter_name(&self, f: &Filter) -> String {
        match f {
            Filter::All => "All clips".to_owned(),
            Filter::Loose => "Other".to_owned(),
            Filter::Folder(name) => name.clone(),
            Filter::Collection(id) => self.collections.get(id).map(|c| c.name.clone()).unwrap_or_default(),
        }
    }

    /// The Clips panel: title and search, then the card of filters.
    fn panel(&mut self, ui: &mut egui::Ui, games: &[View], collections: &[View]) -> Option<crate::library::Action> {
        let p = theme::pal(ui);
        let rect = ui.max_rect();
        ui.painter().rect_filled(rect, 0, p.base);
        let mut action = None;
        // The card's rows line up with the page title (the page margin from
        // the rail), their own padding inside it.
        // The same gap below the card as beside it.
        let side = ui_kit::PAGE_MARGIN as f32 - 10.0;
        let inner = Rect::from_min_max(rect.min + vec2(side, 0.0), rect.max - vec2(0.0, side));
        // Title and search on the heading's row.
        let head = Rect::from_min_size(inner.min, vec2(inner.width(), crate::shell::HEAD_H));
        crate::shell::drag_window_area(ui, Rect::from_min_size(head.min, vec2(70.0, head.height())), "panel_head");
        // The title where every page's is: the page margin from the rail.
        let title_x = rect.left() + ui_kit::PAGE_MARGIN as f32;
        let mut hui = ui.new_child(egui::UiBuilder::new().max_rect(Rect::from_min_max(Pos2::new(title_x, head.top()), head.max)).layout(egui::Layout::left_to_right(egui::Align::Center)));
        hui.label(theme::text("Clips", 20.0, W::Semibold).color(p.text));
        hui.add_space(6.0);
        hui.allocate_ui(vec2(ui_kit::SEARCH_W.min(hui.available_width()), ui_kit::CONTROL_H), |ui| search_field(ui, "library_search", &mut self.library_query, "Search"));
        // The card.
        let card = Rect::from_min_max(Pos2::new(inner.left(), head.bottom()), inner.max);
        let view = self.glass_view;
        crate::glass::paint(ui, card, self, view);
        let mut cui = ui.new_child(egui::UiBuilder::new().max_rect(card.shrink(6.0)).layout(egui::Layout::top_down(egui::Align::Min)));
        cui.set_clip_rect(card.intersect(ui.clip_rect()));
        egui::ScrollArea::vertical().id_salt("library_panel_scroll").auto_shrink([false, false]).show(&mut cui, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            let all = View { filter: Filter::All, label: "All clips".into(), count: self.clips.len() };
            if let Some(a) = self.filter_row(ui, &all) {
                action = Some(a);
            }
            if !games.is_empty() {
                group_label(ui, "Games");
                for v in games {
                    if let Some(a) = self.filter_row(ui, v) {
                        action = Some(a);
                    }
                }
            }
            group_label(ui, "Collections");
            for v in collections {
                if let Some(a) = self.filter_row(ui, v) {
                    action = Some(a);
                }
            }
            let resp = new_collection_row(ui, self.card_drag.is_some());
            if let Some(drag) = &mut self.card_drag
                && resp.contains_pointer()
            {
                drag.target = Some((DropOn::NewCollection, "New collection with it".into()));
            }
            if resp.clicked() {
                action = Some(Action::NewCollection(Vec::new()));
            }
            if collections.is_empty() {
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    ui.add_space(8.0);
                    ui.add(egui::Label::new(theme::text("Keep your best clips together, like “Ace clutches”. Drag clips here.", 12.0, W::Regular).color(p.text3)).wrap());
                });
            }
        });
        action
    }

    /// One filter: its picture, name and number of clips. The chosen one is
    /// a shade darker than the card.
    fn filter_row(&mut self, ui: &mut egui::Ui, view: &View) -> Option<Action> {
        let p = theme::pal(ui);
        let mut action = None;
        let chosen = self.library_filter == view.filter;
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 36.0), Sense::click());
        let drop_here = self.drop_target(ui, rect, &view.filter);
        let hv = theme::toggle(ui.ctx(), resp.id.with("hv"), resp.hovered() || drop_here, theme::FAST);
        let on = theme::toggle(ui.ctx(), resp.id.with("on"), chosen, theme::NORMAL);
        if hv > 0.0 && !chosen {
            ui.painter().rect_filled(rect, ui_kit::RADIUS, theme::fade(p.sb_hover, hv));
        }
        if on > 0.0 {
            ui.painter().rect_filled(rect, ui_kit::RADIUS, theme::fade(p.sb_sel, on));
        }
        if drop_here {
            ui.painter().rect_stroke(rect, ui_kit::RADIUS, Stroke::new(1.5, p.text), StrokeKind::Inside);
        }
        let quiet = if p.dark { Color32::from_rgba_unmultiplied(236, 236, 238, 158) } else { Color32::from_rgba_unmultiplied(23, 24, 27, 128) };
        let text = theme::mix(theme::mix(p.text2, p.text, hv), p.text, on);
        let lead = Rect::from_center_size(Pos2::new(rect.left() + 8.0 + 11.0, rect.center().y), Vec2::splat(22.0));
        self.filter_lead(ui, &view.filter, lead, text);
        let count = ui.painter().layout_no_wrap(view.count.to_string(), theme::font(12.0, W::Regular), quiet);
        let cx = rect.right() - 10.0 - count.size().x;
        ui.painter().galley(Pos2::new(cx, rect.center().y - count.size().y / 2.0), count, quiet);
        let lx = lead.right() + 11.0;
        let w = if chosen { W::Medium } else { W::Regular };
        let label = ui.painter().layout_job(ui_kit::single_line_job(&view.label, theme::font(13.5, w), text, (cx - lx - 8.0).max(10.0)));
        ui.painter().galley(Pos2::new(lx, rect.center().y - label.size().y / 2.0), label, text);
        let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
        if resp.clicked() && !chosen {
            action = Some(Action::Filter(view.filter.clone()));
        }
        if !matches!(view.filter, Filter::All) {
            let f = view.filter.clone();
            if let Some(a) = ui_kit::context_menu(&resp, 220.0, |ui| view_menu(ui, &f)).flatten() {
                action = Some(a);
            }
        }
        action
    }

    /// A filter's picture: a game's logo, a collection's newest clip, or a
    /// bare icon (All clips, Desktop, Other).
    fn filter_lead(&mut self, ui: &mut egui::Ui, filter: &Filter, rect: Rect, color: Color32) {
        let p = ui.painter().clone();
        let bare = |glyph: &str, size: f32| {
            p.text(rect.center(), Align2::CENTER_CENTER, glyph, FontId::new(size, theme::fill_family()), color);
        };
        match filter {
            Filter::All => bare(egui_phosphor::fill::SQUARES_FOUR, 18.0),
            Filter::Loose => bare(egui_phosphor::fill::FOLDER_SIMPLE, 17.0),
            Filter::Folder(name) => match self.folder_icon(ui.ctx(), name) {
                Some(tex) => {
                    egui::Image::from_texture((tex.id(), rect.size())).corner_radius(4).paint_at(ui, rect);
                }
                None if name == clips::DESKTOP => bare(egui_phosphor::fill::MONITOR, 18.0),
                None => {
                    // A tile with the game's initials.
                    let pal = theme::pal(ui);
                    p.rect_filled(rect, 4, pal.control);
                    let initials: String = name.split_whitespace().filter_map(|w| w.chars().next()).take(2).collect::<String>().to_uppercase();
                    p.text(rect.center(), Align2::CENTER_CENTER, initials, theme::font(10.0, W::Bold), pal.text2);
                }
            },
            Filter::Collection(id) => {
                let newest = self.clips.iter().find(|c| self.collections.contains(id, &c.path)).cloned();
                let ctx = ui.ctx().clone();
                match newest.and_then(|c| self.thumbs.get(&ctx, &c).and_then(|t| t.texture.clone())) {
                    Some(tex) => {
                        let uv = crate::filmstrip::crop_uv(tex.size_vec2(), rect.size());
                        egui::Image::from_texture((tex.id(), rect.size())).uv(uv).corner_radius(4).paint_at(ui, rect);
                    }
                    None => {
                        p.text(rect.center(), Align2::CENTER_CENTER, icon::FOLDER_SIMPLE, FontId::proportional(16.0), color);
                    }
                }
            }
        }
    }

    /// While clips are dragged over a filter: whether they can go there (it's
    /// then what a drop does).
    fn drop_target(&mut self, ui: &egui::Ui, rect: Rect, filter: &Filter) -> bool {
        let Some(drag) = &mut self.card_drag else { return false };
        if !ui.rect_contains_pointer(rect) {
            return false;
        }
        let label = match filter {
            Filter::All => return false,
            Filter::Collection(id) if drag.files.iter().all(|p| self.collections.contains(id, p)) => return false,
            Filter::Collection(id) => format!("Add to {}", self.collections.get(id).map(|c| c.name.clone()).unwrap_or_default()),
            Filter::Folder(_) | Filter::Loose => {
                let dir = match filter {
                    Filter::Folder(f) => self.settings.output_dir.join(f),
                    _ => self.settings.output_dir.clone(),
                };
                if drag.files.iter().all(|p| p.parent() == Some(dir.as_path())) {
                    return false;
                }
                format!("Move to {}", match filter {
                    Filter::Folder(f) => f.clone(),
                    _ => "Other".into(),
                })
            }
        };
        drag.target = Some((DropOn::View(filter.clone()), label));
        true
    }

    /// Keep up with clips being dragged: out of the window, they go to the OS
    /// (to drop into another app); Esc cancels.
    fn follow_drag(&mut self, ctx: &egui::Context) -> Option<Action> {
        let drag = self.card_drag.as_mut()?;
        drag.target = None;
        // `hover_pos` is gone once the pointer has left the window.
        let (down, pos, esc) = ctx.input(|i| (i.pointer.primary_down(), i.pointer.hover_pos(), i.key_pressed(egui::Key::Escape)));
        if esc {
            self.card_drag = None;
            ctx.stop_dragging();
            return None;
        }
        let outside = pos.is_none_or(|p| !ctx.content_rect().shrink(2.0).contains(p));
        if down && outside && share::CAN_DRAG_OUT {
            let drag = self.card_drag.take()?;
            let preview = self.clips.iter().find(|c| c.path == drag.clip).and_then(thumbs::cached_jpeg);
            return Some(Action::DragOut(drag.files, preview));
        }
        None
    }

    /// After the panel has looked at the pointer: drop on what it's over when
    /// the button comes up, else draw the clips under the pointer.
    fn finish_drag(&mut self, ctx: &egui::Context) -> Option<Action> {
        let drag = self.card_drag.as_ref()?;
        if !ctx.input(|i| i.pointer.primary_down()) {
            let drag = self.card_drag.take()?;
            return drag.target.map(|(on, _)| Action::Drop(drag.files, on));
        }
        let pos = ctx.input(|i| i.pointer.latest_pos())?;
        ctx.set_cursor_icon(if drag.target.is_some() { egui::CursorIcon::Grabbing } else { egui::CursorIcon::Grab });
        let clip = self.clips.iter().find(|c| c.path == drag.clip).cloned();
        let tex = clip.and_then(|c| self.thumbs.get(ctx, &c).and_then(|t| t.texture.clone()));
        let (n, label) = (drag.files.len(), drag.target.as_ref().map(|(_, l)| l.clone()));
        let started = theme::born(ctx, egui::Id::new("drag_ghost"));
        let t = theme::since(ctx, started, theme::FAST);
        egui::Area::new(egui::Id::new("card_drag_ghost"))
            .order(egui::Order::Tooltip)
            .fixed_pos(pos + vec2(12.0, 12.0))
            .interactable(false)
            .show(ctx, |ui| {
                let size = vec2(128.0, 72.0) * (0.85 + 0.15 * t);
                let (rect, _) = ui.allocate_exact_size(vec2(140.0, 110.0), Sense::hover());
                let rect = Rect::from_min_size(rect.min, size);
                let p = ui.painter();
                p.rect_filled(rect.translate(vec2(0.0, 8.0)).expand(2.0), 7, Color32::from_black_alpha(70));
                if n > 1 {
                    p.rect_filled(rect.translate(vec2(5.0, -5.0)), 5, Color32::from_gray(70));
                }
                match &tex {
                    Some(tex) => {
                        let uv = crate::filmstrip::crop_uv(tex.size_vec2(), size);
                        egui::Image::from_texture((tex.id(), size)).uv(uv).corner_radius(5).paint_at(ui, rect);
                    }
                    None => {
                        p.rect_filled(rect, 5, Color32::from_gray(40));
                    }
                }
                p.rect_stroke(rect, 5, Stroke::new(1.5, Color32::WHITE), StrokeKind::Inside);
                if n > 1 {
                    let c = rect.right_top() + vec2(-2.0, 2.0);
                    p.circle_filled(c, 11.0, Color32::WHITE);
                    p.text(c, Align2::CENTER_CENTER, n.to_string(), theme::font(12.0, W::Bold), Color32::from_rgb(20, 21, 24));
                }
                if let Some(label) = &label {
                    let g = p.layout_no_wrap(label.clone(), theme::font(12.0, W::Semibold), Color32::from_rgb(20, 21, 24));
                    let r = Rect::from_min_size(rect.left_bottom() + vec2(0.0, 6.0), g.size() + vec2(16.0, 6.0));
                    p.rect_filled(r, 4, Color32::from_rgb(242, 242, 243));
                    p.galley(r.min + vec2(8.0, 3.0), g, Color32::from_rgb(20, 21, 24));
                }
            });
        ctx.request_repaint();
        None
    }

    /// The panel's list in a menu on the title, when the window is too narrow
    /// for the panel.
    fn narrow_filter_menu(&mut self, ui: &mut egui::Ui, title: &str, games: &[View], collections: &[View]) -> Option<Action> {
        let mut action = None;
        let r = Btn::ghost(title).trail(icon::CARET_DOWN).id("lib_title_menu").show(ui);
        ui_kit::menu_popup(&r, true, 240.0, |ui| {
            let all = View { filter: Filter::All, label: "All clips".into(), count: self.clips.len() };
            let mut pick = |ui: &mut egui::Ui, v: &View| {
                if Item::new(v.label.clone()).check(self.library_filter == v.filter).value(v.count.to_string()).show(ui).clicked() {
                    action = Some(Action::Filter(v.filter.clone()));
                }
            };
            pick(ui, &all);
            if !games.is_empty() {
                ui_kit::menu_sep(ui);
                ui_kit::menu_head(ui, "Games");
                games.iter().for_each(|v| pick(ui, v));
            }
            ui_kit::menu_sep(ui);
            ui_kit::menu_head(ui, "Collections");
            collections.iter().for_each(|v| pick(ui, v));
            if Item::new("New collection…").icon(icon::PLUS).show(ui).clicked() {
                action = Some(Action::NewCollection(Vec::new()));
            }
        });
        action
    }

    /// Nothing to show: a search with no matches, or an empty collection.
    fn no_results(&mut self, ui: &mut egui::Ui, query: &str) {
        let p = theme::pal(ui);
        ui.add_space(80.0);
        ui.vertical_centered(|ui| {
            if !query.is_empty() {
                ui.label(theme::text(format!("No clips match “{}”", self.library_query.trim()), 15.0, W::Semibold).color(p.text));
                ui.add_space(4.0);
                ui.label(theme::text("Try a game's name or part of a clip's name.", 13.5, W::Regular).color(p.text3));
                ui.add_space(10.0);
                if Btn::normal("Clear search").small().show(ui).clicked() {
                    self.library_query.clear();
                }
            } else {
                let name = self.filter_name(&self.library_filter);
                ui.label(theme::text(format!("Nothing in {name} yet"), 15.0, W::Semibold).color(p.text));
                ui.add_space(4.0);
                ui.label(theme::text("Drag clips here, or right-click a clip and choose Add to collection.", 13.5, W::Regular).color(p.text3));
            }
        });
    }

    /// The first launch, or a library with no clips: what to do, the button to
    /// do it, and the shortcut you'll use.
    fn empty_state(&mut self, ui: &mut egui::Ui) {
        let p = theme::pal(ui);
        self.page_head(ui, "", |_, _| {});
        let save = self.shortcut_keys(crate::settings::ShortcutAction::SaveClip);
        let len = crate::settings_ui::length_words(self.settings.replay_seconds);
        let len = if len == "1 minute" { "minute".to_owned() } else { len };
        let avail = ui.available_rect_before_wrap();
        let born = theme::born(ui.ctx(), egui::Id::new("empty_state"));
        let t = theme::since(ui.ctx(), born, theme::SLOW);
        let mut cui = ui.new_child(egui::UiBuilder::new().max_rect(avail.translate(vec2(0.0, 8.0 * (1.0 - t) - 30.0))).layout(egui::Layout::top_down(egui::Align::Center)));
        cui.multiply_opacity(t);
        cui.add_space((avail.height() * 0.5 - 140.0).max(20.0));
        // The app's glyph, one colour for the theme (white on dark, black on
        // light) with its own silhouette blurred as the shadow, so both follow
        // the horse instead of a box. The pictures have 1/3 margin around it.
        let (glyph, shadow) = self.glyph_textures(cui.ctx());
        let (r, _) = cui.allocate_exact_size(vec2(76.0, 76.0), Sense::hover());
        let canvas = Rect::from_center_size(r.center(), r.size() * 1.5);
        egui::Image::from_texture((shadow.id(), canvas.size()))
            .tint(Color32::from_black_alpha(if p.dark { 150 } else { 70 }))
            .paint_at(&cui, canvas.translate(vec2(0.0, 7.0)));
        egui::Image::from_texture((glyph.id(), canvas.size()))
            .tint(if p.dark { Color32::WHITE } else { Color32::BLACK })
            .paint_at(&cui, canvas);
        cui.add_space(22.0);
        cui.label(theme::text("Your clips will show up here", 22.0, W::Semibold).color(p.text));
        cui.add_space(8.0);
        let key = save.as_ref().map(|k| ui_kit::key_text(k)).unwrap_or_else(|| "the shortcut".into());
        let text = match self.rec_state {
            RecState::Idle => format!("Start clipping and play as usual. When something worth keeping happens, press {key} to save the last {len}."),
            RecState::Buffering => format!("Clipping. Play as usual, and when something worth keeping happens, press {key} to save the last {len}."),
            RecState::Recording => "Recording. Stop it to see your recording here.".to_owned(),
        };
        cui.allocate_ui(vec2(440.0, 60.0), |ui| {
            ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                ui.add(egui::Label::new(theme::text(text, 14.5, W::Regular).color(p.text2)).wrap().halign(egui::Align::Center));
            });
        });
        cui.add_space(18.0);
        // The row is as wide as its buttons, so it centres on them.
        let start = (self.rec_state == RecState::Idle).then(|| Btn::primary("Start clipping").min_width(130.0));
        let choose = Btn::normal("Choose what's recorded");
        let gap = 8.0;
        let row_w = choose.width(&cui) + start.as_ref().map_or(0.0, |b| b.width(&cui) + gap);
        cui.allocate_ui(vec2(row_w, 36.0), |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                if start.is_some_and(|b| b.show(ui).clicked()) {
                    self.start_replay_buffer();
                }
                if choose.show(ui).clicked() {
                    self.page = crate::Page::Sources;
                }
            });
        });
    }

    /// The glyph and its shadow (`assets/glyph*.png`, made by scripts/tray-icons.sh).
    fn glyph_textures(&mut self, ctx: &egui::Context) -> (egui::TextureHandle, egui::TextureHandle) {
        self.app_icon_tex
            .get_or_insert_with(|| {
                let load = |name: &str, png: &[u8]| {
                    let img = image::load_from_memory_with_format(png, image::ImageFormat::Png).expect("bundled glyph is a valid PNG").to_rgba8();
                    let img = egui::ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw());
                    ctx.load_texture(name, img, egui::TextureOptions::LINEAR)
                };
                (load("glyph", include_bytes!("../assets/glyph.png")), load("glyph_shadow", include_bytes!("../assets/glyph-shadow.png")))
            })
            .clone()
    }

    /// A card for something on its way: a recording, a clip being saved.
    fn progress_card(ui: &mut egui::Ui, rect: Rect, title: &str, caption: &str, color: Color32, progress: Option<f32>) {
        let p = theme::pal(ui);
        let thumb = Rect::from_min_size(rect.min, vec2(rect.width(), rect.width() * 9.0 / 16.0));
        let painter = ui.painter();
        painter.rect_filled(thumb, ui_kit::RADIUS, p.raise);
        let tg = painter.layout_no_wrap(title.to_owned(), theme::font(13.5, W::Semibold), p.text);
        let w = tg.size().x + 18.0;
        let c = thumb.center();
        crate::shell::status_dot(painter, Pos2::new(c.x - w / 2.0 + 4.0, c.y), color, ui.input(|i| i.time));
        painter.galley(Pos2::new(c.x - w / 2.0 + 18.0, c.y - tg.size().y / 2.0), tg, p.text);
        if let Some(f) = progress {
            let bar = Rect::from_min_max(Pos2::new(thumb.left(), thumb.bottom() - 3.0), thumb.max);
            painter.rect_filled(bar, 0, theme::fade(p.text, 0.15));
            painter.rect_filled(Rect::from_min_max(bar.min, Pos2::new(bar.left() + bar.width() * f.clamp(0.0, 1.0), bar.bottom())), 0, p.text);
        }
        painter.text(Pos2::new(rect.left() + 2.0, thumb.bottom() + 9.0), Align2::LEFT_TOP, caption, theme::font(13.5, W::Medium), p.text);
        painter.text(Pos2::new(rect.left() + 2.0, thumb.bottom() + 29.0), Align2::LEFT_TOP, "In progress", theme::font(12.5, W::Regular), p.text3);
        ui.ctx().request_repaint_after(Duration::from_millis(33));
    }

    fn new_render_card(&mut self, ui: &mut egui::Ui, i: usize, rect: Rect) {
        let p = theme::pal(ui);
        let job = &self.renders[i];
        let f = job.progress();
        let name = job.dest.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        // The source clip's picture under the progress, so it's clear what's being made.
        let source = self.clips.iter().find(|c| c.path == job.source).cloned();
        let thumb = Rect::from_min_size(rect.min, vec2(rect.width(), rect.width() * 9.0 / 16.0));
        let ctx = ui.ctx().clone();
        match source.and_then(|src| self.thumbs.get(&ctx, &src).and_then(|t| t.texture.clone())) {
            Some(tex) => {
                let uv = crate::filmstrip::crop_uv(tex.size_vec2(), thumb.size());
                egui::Image::from_texture((tex.id(), thumb.size())).uv(uv).corner_radius(ui_kit::RADIUS).paint_at(ui, thumb);
                ui.painter().rect_filled(thumb, ui_kit::RADIUS, Color32::from_black_alpha(150));
                saving_label(ui, thumb, &format!("Saving {:.0}%", f * 100.0), f);
                ui.painter().text(Pos2::new(rect.left() + 2.0, thumb.bottom() + 9.0), Align2::LEFT_TOP, &name, theme::font(13.5, W::Medium), p.text);
                ui.ctx().request_repaint_after(Duration::from_millis(50));
            }
            None => Self::progress_card(ui, rect, &format!("Saving {:.0}%", f * 100.0), &name, p.live, Some(f)),
        }
    }

    fn placeholder_card(&mut self, ui: &mut egui::Ui, rect: Rect) {
        let p = theme::pal(ui);
        if self.rec_state == RecState::Recording {
            let secs = self.rec_started.map_or(0, |t| t.elapsed().as_secs());
            Self::progress_card(ui, rect, &format!("Recording {}", thumbs::format_duration(Duration::from_secs(secs))), "Recording", p.rec, None);
        } else {
            let len = crate::settings_ui::length_words(self.settings.replay_seconds);
            Self::progress_card(ui, rect, "Saving clip…", &format!("The last {len}"), p.live, None);
        }
    }

    /// A clip: its picture (cropped to 16:9 so every card lines up), its
    /// length or edited length, and its name and game · size. Returns what was
    /// asked for.
    fn clip_card(&mut self, ui: &mut egui::Ui, clip: &clips::Clip, rect: Rect, folders: &[String], selecting: bool) -> Option<Action> {
        let p = theme::pal(ui);
        let thumb_rect = Rect::from_min_size(rect.min, vec2(rect.width(), rect.width() * 9.0 / 16.0));
        // click_and_drag: a click plays, a drag pulls the clip out.
        let resp = ui.interact(rect, egui::Id::new(("clip_card", &clip.path)), Sense::click_and_drag());
        if self.reveal_clip.as_ref() == Some(&clip.path) {
            ui.scroll_to_rect(rect, Some(egui::Align::Center));
            self.reveal_clip = None;
            self.flash_clip = Some((clip.path.clone(), ui.input(|i| i.time)));
        }
        let hovered = ui.rect_contains_pointer(rect) && !ui.ctx().is_being_dragged(resp.id);
        let is_new = self.last_saved.as_ref().is_some_and(|(path, at)| *path == clip.path && at.elapsed() < NEW_FOR) && !self.seen_new.contains(&clip.path);
        let selected = self.selection.paths.contains(&clip.path);
        let ctx = ui.ctx().clone();
        let thumb = self.thumbs.get(&ctx, clip);
        let tex = thumb.and_then(|t| t.texture.clone());
        let duration = thumb.and_then(|t| t.duration);
        let sel = theme::toggle(&ctx, resp.id.with("sel"), selected, theme::FAST);
        let render = self.renders.iter().find(|j| !j.as_new && j.source == clip.path).map(|j| j.progress());

        // Picture.
        let painter = ui.painter().clone();
        match &tex {
            Some(tex) => {
                let uv = crate::filmstrip::crop_uv(tex.size_vec2(), thumb_rect.size());
                egui::Image::from_texture((tex.id(), thumb_rect.size())).uv(uv).corner_radius(ui_kit::RADIUS).paint_at(ui, thumb_rect);
            }
            None => {
                painter.rect_filled(thumb_rect, ui_kit::RADIUS, p.raise2);
            }
        }
        // Rested on: it plays, after a moment (while nothing's dragged or saved).
        if hovered && render.is_none() && self.card_drag.is_none() {
            crate::hover_play::HoverPlay::hovering(&mut self.hover_play, &clip.path);
            self.hover_seen = true;
        }
        if let Some(h) = self.hover_play.as_mut().filter(|h| h.clip() == clip.path) {
            if let Some(size) = h.update(&ctx) {
                if h.paint(ui, thumb_rect, size) {
                    // The card's corners, round like the thumbnail's.
                    ui_kit::round_corners(&ui.painter_at(thumb_rect), thumb_rect, ui_kit::RADIUS as f32, p.base);
                }
            }
        }
        // Selected: an outline a little outside the picture.
        if sel > 0.0 {
            painter.rect_stroke(thumb_rect.expand(2.0 + 2.0 * sel), ui_kit::RADIUS + 3, Stroke::new(2.5, theme::fade(p.text, sel)), StrokeKind::Inside);
        }
        // Just brought back into view (back from the player): a ring that fades out.
        if let Some((path, at)) = &self.flash_clip
            && path == &clip.path
        {
            let t = ((ui.input(|i| i.time) - at) / 1.2) as f32;
            if t < 1.0 {
                painter.rect_stroke(thumb_rect.expand(2.0 + 4.0 * t), ui_kit::RADIUS + 3, Stroke::new(2.5, theme::fade(p.text, 1.0 - t)), StrokeKind::Inside);
                ui.ctx().request_repaint();
            }
        }
        // Length: the edited length with a scissors on an edited clip.
        let edited = clip.original.is_some();
        if let Some(d) = duration {
            let text = if edited { format!("{} {}", icon::SCISSORS, thumbs::format_duration(d)) } else { thumbs::format_duration(d) };
            pill(&painter, thumb_rect.right_bottom() - vec2(7.0, 7.0), Align2::RIGHT_BOTTOM, &text);
        }
        if let Some(f) = render {
            painter.rect_filled(thumb_rect, ui_kit::RADIUS, Color32::from_black_alpha(150));
            saving_label(ui, thumb_rect, &format!("Saving edit {:.0}%", f * 100.0), f);
            ui.ctx().request_repaint_after(Duration::from_millis(50));
        } else if let Some(up) = self.cloud.upload_for(&clip.path) {
            let f = up.progress();
            let label = if up.merging() { "Finishing upload…".to_owned() } else { format!("Uploading {:.0}%", f * 100.0) };
            pill(&painter, thumb_rect.left_bottom() + vec2(7.0, -7.0), Align2::LEFT_BOTTOM, &label);
            let bar = Rect::from_min_max(Pos2::new(thumb_rect.left(), thumb_rect.bottom() - 3.0), thumb_rect.max);
            painter.rect_filled(bar, 0, Color32::from_white_alpha(46));
            painter.rect_filled(Rect::from_min_max(bar.min, Pos2::new(bar.left() + bar.width() * f, bar.bottom())), 0, Color32::WHITE);
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }

        let mut action = None;
        let modifiers = ui.input(|i| i.modifiers);
        // Selection circle: always while selecting, on hover otherwise.
        let show_check = theme::toggle(&ctx, resp.id.with("chk"), (hovered || selecting) && render.is_none(), theme::FAST);
        if show_check > 0.0 {
            let c = thumb_rect.left_top() + vec2(19.0, 19.0);
            let r = ui.interact(Rect::from_center_size(c, Vec2::splat(30.0)), resp.id.with("check"), Sense::click());
            check_circle(&painter, c, sel, r.hovered(), show_check);
            if r.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(if selected { "Deselect" } else { "Select" }).clicked() {
                action = Some(Action::Select { path: clip.path.clone(), range: modifiers.shift });
            }
        }
        // Edit and Share, top right, on hover.
        let share_id = resp.id.with("share_menu");
        let menu_open = egui::Popup::is_id_open(&ctx, share_id.with("menu"));
        let qa = theme::toggle(&ctx, resp.id.with("qa"), (hovered || menu_open) && render.is_none() && !selecting, theme::FAST);
        if qa > 0.0 {
            let share_r = Rect::from_min_size(thumb_rect.right_top() + vec2(-38.0, 8.0), Vec2::splat(30.0));
            let edit_r = share_r.translate(vec2(-36.0, 0.0));
            let mut qui = ui.new_child(egui::UiBuilder::new().max_rect(edit_r.union(share_r)).layout(egui::Layout::left_to_right(egui::Align::Center)));
            qui.multiply_opacity(qa);
            qui.spacing_mut().item_spacing.x = 6.0;
            if overlay_icon(&mut qui, icon::SCISSORS, false, "Edit").clicked() {
                action = Some(Action::Edit(clip.path.clone()));
            }
            let sr = overlay_icon(&mut qui, icon::EXPORT, true, "Share");
            let sr = egui::Response { id: share_id, ..sr };
            if let Some(c) = ui_kit::menu_popup(&sr, false, 250.0, |ui| share_items(ui, sr.rect.left_bottom(), false)).flatten() {
                action = Some(Action::Share(clip.path.clone(), c));
            }
        }

        // Caption: name (a dot first while it's new), then game · size.
        let cap_y = thumb_rect.bottom() + 9.0;
        let mut x = rect.left() + 2.0;
        if is_new {
            painter.circle_filled(Pos2::new(x + 3.5, cap_y + 9.0), 3.5, p.text);
            x += 14.0;
        }
        let title = painter.layout_job(ui_kit::single_line_job(&clip.title(), theme::font(13.5, W::Medium), p.text, rect.right() - x - 2.0));
        painter.galley(Pos2::new(x, cap_y), title, p.text);
        let meta_y = cap_y + 21.0;
        let mut mx = rect.left() + 2.0;
        let game = clip.folder.clone().unwrap_or_else(|| "Other".into());
        let lead = Rect::from_min_size(Pos2::new(mx, meta_y), Vec2::splat(15.0));
        self.filter_lead(ui, &clip.folder.clone().map_or(Filter::Loose, Filter::Folder), lead, p.text3);
        mx += 21.0;
        let size = clip.human_size();
        let size_g = painter.layout_no_wrap(size.clone(), theme::font(12.5, W::Regular), p.text3);
        let game_w = (rect.right() - mx - size_g.size().x - 18.0).max(20.0);
        let game_g = painter.layout_job(ui_kit::single_line_job(&game, theme::font(12.5, W::Regular), p.text3, game_w));
        let gy = meta_y + 7.5 - game_g.size().y / 2.0;
        let gw = game_g.size().x;
        painter.galley(Pos2::new(mx, gy), game_g, p.text3);
        mx += gw + 7.0;
        painter.circle_filled(Pos2::new(mx, meta_y + 8.0), 1.5, theme::fade(p.text3, 0.6));
        mx += 7.0;
        let size_rect = Rect::from_min_size(Pos2::new(mx, gy), size_g.size());
        painter.galley(size_rect.min, size_g, p.text3);
        let when = clip.captured_at().format("Saved %-d %B at %H:%M").to_string();
        let size_tip = match (edited, &clip.original) {
            (true, Some(orig)) => {
                let orig_size = std::fs::metadata(orig).map(|m| m.len()).unwrap_or(0);
                format!("{when}\nEdited: the original recording is {} more", crate::export_ui::human_bytes(orig_size))
            }
            _ => when,
        };
        ui.interact(size_rect, resp.id.with("size"), Sense::hover()).on_hover_text(size_tip);

        let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text_at_pointer(clip.title());
        if hovered && is_new {
            self.seen_new.insert(clip.path.clone());
        }
        // Dragging one of the selected clips takes them all, in library order;
        // any other clip goes alone, as in a file manager.
        if resp.drag_started() && action.is_none() && self.card_drag.is_none() {
            let files = if selected { self.clips.iter().filter(|c| self.selection.paths.contains(&c.path)).map(|c| c.path.clone()).collect() } else { vec![clip.path.clone()] };
            action = Some(Action::StartDrag(files, clip.path.clone()));
        }
        if resp.clicked() && action.is_none() && render.is_none() {
            action = Some(if modifiers.shift && (selecting || modifiers.command) {
                Action::Select { path: clip.path.clone(), range: true }
            } else if selecting || modifiers.command {
                Action::Select { path: clip.path.clone(), range: false }
            } else {
                Action::Open(clip.path.clone())
            });
        }
        if let Some(a) = ui_kit::context_menu(&resp, 240.0, |ui| self.clip_menu(ui, clip, selected, folders)).flatten() {
            action = Some(a);
        }
        action
    }

    /// A clip's right-click menu (or, on one of several selected clips, the
    /// selection's).
    fn clip_menu(&mut self, ui: &mut egui::Ui, clip: &clips::Clip, selected: bool, folders: &[String]) -> Option<Action> {
        let mut action = None;
        let n = self.selection.paths.len();
        if selected && n > 1 {
            let paths: Vec<PathBuf> = self.clips.iter().filter(|c| self.selection.paths.contains(&c.path)).map(|c| c.path.clone()).collect();
            ui_kit::submenu(ui, Item::new(format!("Add {n} clips to collection")).icon(icon::FOLDER_PLUS), |ui| {
                if let Some(a) = collection_items(ui, &self.collections, &paths) {
                    action = Some(a);
                }
            });
            if let Filter::Collection(id) = &self.library_filter
                && Item::new(format!("Remove {n} clips from {}", self.filter_name(&self.library_filter))).icon(icon::MINUS_CIRCLE).show(ui).clicked()
            {
                action = Some(Action::InCollection(paths.clone(), id.clone(), false));
            }
            if !folders.is_empty() {
                ui_kit::submenu(ui, Item::new(format!("Move {n} clips to")).icon(icon::FOLDERS), |ui| {
                    if let Some(f) = move_items(ui, folders, None, true) {
                        action = Some(Action::MoveSelectedTo(f));
                    }
                });
            }
            ui_kit::menu_sep(ui);
            if Item::new("Deselect all").icon(icon::X_CIRCLE).show(ui).clicked() {
                action = Some(Action::Deselect);
            }
            ui_kit::menu_sep(ui);
            if Item::new(self.delete_label(Some(n))).icon(icon::TRASH).danger().show(ui).clicked() {
                action = Some(Action::TrashSelected);
            }
            return action;
        }
        if Item::new("Play").icon(icon::PLAY).show(ui).clicked() {
            action = Some(Action::Open(clip.path.clone()));
        }
        if Item::new("Edit").icon(icon::SCISSORS).show(ui).clicked() {
            action = Some(Action::Edit(clip.path.clone()));
        }
        if Item::new("Rename…").icon(icon::PENCIL_SIMPLE).keys(Some(vec!["F2".into()])).show(ui).clicked() {
            action = Some(Action::Rename(clip.path.clone()));
        }
        ui_kit::menu_sep(ui);
        let uploading = self.cloud.upload_for(&clip.path).is_some();
        if uploading {
            if Item::new("Cancel upload").icon(icon::X_CIRCLE).show(ui).clicked() {
                action = Some(Action::CancelUpload(clip.path.clone()));
            }
        } else {
            let at = ui.ctx().pointer_latest_pos().unwrap_or_default();
            ui_kit::submenu(ui, Item::new("Share").icon(icon::EXPORT), |ui| {
                if let Some(c) = share_items(ui, at, true) {
                    action = Some(Action::Share(clip.path.clone(), c));
                }
            });
        }
        ui_kit::submenu(ui, Item::new("Add to collection").icon(icon::FOLDER_PLUS), |ui| {
            if let Some(a) = collection_items(ui, &self.collections, std::slice::from_ref(&clip.path)) {
                action = Some(a);
            }
        });
        if let Filter::Collection(id) = &self.library_filter
            && Item::new(format!("Remove from {}", self.filter_name(&self.library_filter))).icon(icon::MINUS_CIRCLE).show(ui).clicked()
        {
            action = Some(Action::InCollection(vec![clip.path.clone()], id.clone(), false));
        }
        if !folders.is_empty() {
            ui_kit::submenu(ui, Item::new("Move to").icon(icon::FOLDERS), |ui| {
                if let Some(f) = move_items(ui, folders, clip.folder.as_deref(), clip.folder.is_some()) {
                    action = Some(Action::MoveTo(vec![clip.path.clone()], f));
                }
            });
        }
        ui_kit::menu_sep(ui);
        if Item::new("Open in another player").icon(icon::ARROW_SQUARE_OUT).show(ui).clicked() {
            action = Some(Action::OpenExternal(clip.path.clone()));
        }
        let selected = self.selection.paths.contains(&clip.path);
        if Item::new(if selected { "Deselect" } else { "Select" }).icon(icon::CHECK_CIRCLE).show(ui).clicked() {
            action = Some(Action::Select { path: clip.path.clone(), range: false });
        }
        ui_kit::menu_sep(ui);
        let del_key = Some(vec![if cfg!(target_os = "macos") { icon::BACKSPACE.to_owned() } else { "Del".to_owned() }]);
        if Item::new(self.delete_label(None)).icon(icon::TRASH).danger().keys(del_key).show(ui).clicked() {
            action = Some(Action::Trash(clip.path.clone()));
        }
        action
    }

    /// The grid's empty space, right-clicked.
    fn background_menu(&mut self, ui: &mut egui::Ui, days: &[chrono::NaiveDate]) -> Option<Action> {
        let mut action = None;
        if Item::new("Select all").icon(icon::CHECK_CIRCLE).keys(Some(crate::shortcuts::command("A"))).show(ui).clicked() {
            action = Some(Action::SelectAll);
        }
        let sort = self.settings.library_sort;
        ui_kit::submenu(ui, Item::new("Sort by").icon(icon::ARROWS_DOWN_UP), |ui| {
            for s in LibrarySort::ALL {
                if Item::new(s.label()).check(sort == s).show(ui).clicked() {
                    self.settings.library_sort = s;
                }
            }
        });
        if days.len() > 1 {
            let all_folded = days.iter().all(|d| self.folded.contains(d));
            if Item::new(if all_folded { "Expand all days" } else { "Collapse all days" }).icon(icon::LIST_CHECKS).show(ui).clicked() {
                self.folded.set_all(days, !all_folded);
            }
        }
        ui_kit::menu_sep(ui);
        if Item::new(format!("Show clips folder in {}", crate::file_manager())).icon(icon::FOLDER_OPEN).show(ui).clicked()
            && let Err(e) = clips::open_in_default_app(&self.settings.output_dir)
        {
            self.toast_error(format!("Couldn't open the folder: {e}"));
        }
        action
    }

    /// ⌘A selects every clip, ⌫ / Delete trashes the selection, Esc clears it
    /// (or the search).
    fn selection_keys(&mut self, ui: &egui::Ui) -> Option<Action> {
        if ui_kit::overlay_open(ui.ctx()) || ui.ctx().egui_wants_keyboard_input() {
            return None;
        }
        let cmd = egui::Modifiers::COMMAND;
        ui.ctx().input_mut(|i| {
            if i.consume_key(cmd, egui::Key::A) {
                Some(Action::SelectAll)
            } else if self.selection.is_empty() {
                None
            } else if i.consume_key(cmd, egui::Key::Backspace) || i.consume_key(egui::Modifiers::NONE, egui::Key::Backspace) || i.consume_key(egui::Modifiers::NONE, egui::Key::Delete) {
                Some(Action::TrashSelected)
            } else if i.consume_key(egui::Modifiers::NONE, egui::Key::Escape) {
                Some(Action::Deselect)
            } else {
                None
            }
        })
    }

    /// The bar that rises at the bottom while clips are selected: how many,
    /// and what to do with them. `shown`: how many clips are shown.
    fn selection_bar(&mut self, ui: &mut egui::Ui, page: Rect, shown: usize, folders: &[String]) -> Option<Action> {
        let mut action = None;
        let n = self.selection.paths.len();
        let p = theme::pal(ui);
        let born = theme::born(ui.ctx(), egui::Id::new("selection_bar"));
        let t = theme::since(ui.ctx(), born, theme::NORMAL);
        let selected: Vec<PathBuf> = self.clips.iter().filter(|c| self.selection.paths.contains(&c.path)).map(|c| c.path.clone()).collect();
        egui::Area::new(egui::Id::new("selection_bar"))
            .order(egui::Order::Foreground)
            .pivot(Align2::CENTER_BOTTOM)
            .fixed_pos(Pos2::new(page.center().x, page.bottom() - 22.0 + 12.0 * (1.0 - t)))
            .fade_in(false)
            .show(ui.ctx(), |ui| {
                ui.multiply_opacity(t);
                egui::Frame::new()
                    .fill(p.pop)
                    .corner_radius(ui_kit::CARD_RADIUS)
                    .stroke(Stroke::new(1.0, if p.dark { Color32::from_white_alpha(18) } else { Color32::from_black_alpha(20) }))
                    .shadow(ui.visuals().popup_shadow)
                    .inner_margin(egui::Margin::same(6))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 4.0;
                            if Btn::icon(Kind::Ghost, icon::X).small().tip("Deselect  (Esc)").show(ui).clicked() {
                                action = Some(Action::Deselect);
                            }
                            ui.label(theme::text(format!("{n} selected"), 13.5, W::Semibold).color(p.text));
                            ui.add_space(6.0);
                            if n < shown && Btn::ghost("Select all").small().show(ui).clicked() {
                                action = Some(Action::SelectAll);
                            }
                            vrule(ui);
                            let r = Btn::ghost("Add to collection").with_icon(icon::FOLDER_PLUS).small().show(ui);
                            ui_kit::menu_popup(&r, true, 220.0, |ui| {
                                if let Some(a) = collection_items(ui, &self.collections, &selected) {
                                    action = Some(a);
                                }
                            });
                            if let Filter::Collection(id) = &self.library_filter
                                && Btn::ghost("Remove from collection").with_icon(icon::MINUS_CIRCLE).small().show(ui).clicked()
                            {
                                action = Some(Action::InCollection(selected.clone(), id.clone(), false));
                            }
                            if !folders.is_empty() {
                                let r = Btn::ghost("Move to").with_icon(icon::FOLDERS).small().show(ui);
                                ui_kit::menu_popup(&r, true, 220.0, |ui| {
                                    if let Some(f) = move_items(ui, folders, None, true) {
                                        action = Some(Action::MoveSelectedTo(f));
                                    }
                                });
                            }
                            let label = if self.settings.delete_permanently { format!("Delete {n}") } else { format!("Move {n} to {}", crate::store::bin_name()) };
                            if Btn::new(Kind::DangerText, label).with_icon(icon::TRASH).small().show(ui).clicked() {
                                action = Some(Action::TrashSelected);
                            }
                        });
                    });
            });
        action
    }

    /// Do what the page asked for.
    fn do_action(&mut self, ctx: &egui::Context, frame: &eframe::Frame, action: Option<Action>, shown: &[clips::Clip]) {
        let Some(action) = action else {
            if self.selection.is_empty() {
                theme::unborn(ctx, egui::Id::new("selection_bar"));
            }
            return;
        };
        match action {
            Action::Open(p) => self.open_viewer(p),
            Action::OpenExternal(p) => {
                if let Err(e) = clips::open_in_default_app(&p) {
                    self.toast_error(format!("Couldn't open the clip: {e}"));
                }
            }
            Action::Share(p, choice) => self.share(frame, p, choice),
            Action::DragOut(files, preview) => self.drag_out(ctx, frame, files, preview),
            Action::StartDrag(files, clip) => {
                theme::unborn(ctx, egui::Id::new("drag_ghost"));
                self.card_drag = Some(CardDrag { files, clip, target: None });
            }
            Action::Drop(files, on) => match on {
                DropOn::View(Filter::Folder(f)) => self.move_clips(ctx, &files, Some(f)),
                DropOn::View(Filter::Loose) => self.move_clips(ctx, &files, None),
                DropOn::View(Filter::Collection(id)) => self.put_in_collection(&files, &id, true),
                DropOn::View(Filter::All) => {}
                DropOn::NewCollection => self.new_collection_with(files),
            },
            Action::Edit(p) => self.open_editor(p),
            Action::Rename(p) => self.rename_clip(p),
            Action::Trash(p) => self.delete_clips(&[p]),
            Action::MoveTo(paths, folder) => self.move_clips(ctx, &paths, folder),
            Action::MoveSelectedTo(folder) => {
                let paths: Vec<PathBuf> = self.clips.iter().filter(|c| self.selection.paths.contains(&c.path)).map(|c| c.path.clone()).collect();
                self.move_clips(ctx, &paths, folder);
            }
            Action::Filter(f) => {
                self.library_filter = f;
                self.selection.clear();
            }
            Action::RenameFolder(name) => self.rename_folder(&name),
            Action::NewCollection(paths) => self.new_collection_with(paths),
            Action::InCollection(paths, id, add) => self.put_in_collection(&paths, &id, add),
            Action::RenameCollection(id) => self.rename_collection(id),
            Action::DeleteCollection(id) => self.delete_collection(id),
            Action::RevealFolder(name) => {
                if let Err(e) = clips::open_in_default_app(&self.settings.output_dir.join(name)) {
                    self.toast_error(format!("Couldn't open the folder: {e}"));
                }
            }
            Action::Select { path, range } => {
                if range {
                    self.selection.extend_to(&path, shown);
                } else {
                    self.selection.toggle(&path);
                }
            }
            Action::SelectAll => self.selection.paths = shown.iter().map(|c| c.path.clone()).collect(),
            Action::Deselect => self.selection.clear(),
            Action::CancelUpload(p) => {
                if let Some(up) = self.cloud.upload_for(&p) {
                    up.cancel();
                }
            }
            Action::TrashSelected => {
                let paths: Vec<PathBuf> = self.clips.iter().filter(|c| self.selection.paths.contains(&c.path)).map(|c| c.path.clone()).collect();
                self.delete_clips(&paths);
            }
        }
    }
}

/// The search field: right after the page title, ⌘F on it (× while there's
/// text), filtering as you type; Esc clears it.
pub fn search_field(ui: &mut egui::Ui, id: &str, text: &mut String, hint: &str) -> egui::Response {
    let p = theme::pal(ui);
    let id = egui::Id::new(id);
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(w, ui_kit::CONTROL_H), Sense::hover());
    let focused = ui.memory(|m| m.has_focus(id));
    let hovered = ui.rect_contains_pointer(rect);
    let hv = theme::toggle(ui.ctx(), id.with("hv"), hovered && !focused, theme::FAST);
    let fc = theme::toggle(ui.ctx(), id.with("fc"), focused, theme::FAST);
    ui.painter().rect_filled(rect, ui_kit::RADIUS, theme::mix(p.control, p.control_h, hv));
    if fc > 0.0 {
        ui.painter().rect_stroke(rect, ui_kit::RADIUS, Stroke::new(1.5, theme::fade(p.text2, fc)), StrokeKind::Inside);
    }
    ui.painter().text(Pos2::new(rect.left() + 16.0, rect.center().y), Align2::CENTER_CENTER, icon::MAGNIFYING_GLASS, FontId::proportional(14.0), p.text3);
    let right_w = if text.is_empty() { ui_kit::key_hint_width(ui, &crate::shortcuts::command("F"), 11.0) + 6.0 } else { 26.0 };
    let field = Rect::from_min_max(Pos2::new(rect.left() + 28.0, rect.top()), Pos2::new(rect.right() - right_w - 4.0, rect.bottom()));
    let mut fui = ui.new_child(egui::UiBuilder::new().max_rect(field).layout(egui::Layout::left_to_right(egui::Align::Center)));
    let out = egui::TextEdit::singleline(text)
        .id(id)
        .frame(egui::Frame::NONE)
        .hint_text(theme::text(hint, 13.5, W::Regular).color(p.text3))
        .font(theme::font(13.5, W::Regular))
        .text_color(p.text)
        .desired_width(field.width())
        .margin(egui::Margin::ZERO)
        .show(&mut fui);
    let out_resp = out.response.response.clone();
    if focused && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        text.clear();
        out_resp.surrender_focus();
    }
    if text.is_empty() {
        ui_kit::key_hint(ui.painter(), Pos2::new(rect.right() - right_w, rect.center().y), &crate::shortcuts::command("F"), 11.0, p.text3);
    } else {
        let x = Rect::from_center_size(Pos2::new(rect.right() - 15.0, rect.center().y), Vec2::splat(22.0));
        let xr = ui.interact(x, id.with("clear"), Sense::click());
        if xr.hovered() {
            ui.painter().rect_filled(x, 4, p.sel);
        }
        ui.painter().text(x.center(), Align2::CENTER_CENTER, icon::X, FontId::proportional(12.0), if xr.hovered() { p.text } else { p.text3 });
        if xr.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Clear  (Esc)").clicked() {
            text.clear();
        }
    }
    out_resp
}

/// A day's heading: click to fold its clips away or open them again.
fn day_header(ui: &mut egui::Ui, label: &str, clips: usize, folded: bool) -> egui::Response {
    let p = theme::pal(ui);
    let id = ui.id().with(("day", label));
    let lg = ui.painter().layout_no_wrap(label.to_owned(), theme::font(14.5, W::Semibold), p.text);
    let count = if clips == 1 { "1 clip".to_owned() } else { format!("{clips} clips") };
    let cg = ui.painter().layout_no_wrap(count, theme::font(13.0, W::Regular), p.text3);
    let w = 18.0 + lg.size().x + 8.0 + cg.size().x;
    let (rect, resp) = ui.allocate_exact_size(vec2(w, DAY_H), Sense::click());
    let turn = theme::toggle(ui.ctx(), id.with("turn"), folded, theme::NORMAL);
    let hv = theme::toggle(ui.ctx(), id.with("hv"), resp.hovered(), theme::FAST);
    // ∨ open, › folded: the chevron turns.
    let c = Pos2::new(rect.left() + 6.0, rect.center().y);
    let a = -std::f32::consts::FRAC_PI_2 * turn;
    let rot = |v: Vec2| vec2(v.x * a.cos() - v.y * a.sin(), v.x * a.sin() + v.y * a.cos());
    let pts = vec![c + rot(vec2(-4.0, -2.0)), c + rot(vec2(0.0, 2.0)), c + rot(vec2(4.0, -2.0))];
    ui.painter().add(egui::Shape::line(pts, Stroke::new(1.6, theme::mix(p.text3, p.text, hv))));
    ui.painter().galley(Pos2::new(rect.left() + 18.0, rect.center().y - lg.size().y / 2.0), lg.clone(), p.text);
    ui.painter().galley(Pos2::new(rect.left() + 18.0 + lg.size().x + 8.0, rect.center().y - cg.size().y / 2.0 + 1.0), cg, p.text3);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A group's name in the panel.
fn group_label(ui: &mut egui::Ui, text: &str) {
    let quiet = if theme::pal(ui).dark { Color32::from_rgba_unmultiplied(236, 236, 238, 158) } else { Color32::from_rgba_unmultiplied(23, 24, 27, 128) };
    ui.add_space(12.0);
    ui.horizontal(|ui| {
        ui.add_space(8.0);
        ui.label(theme::text(text, 12.0, W::Regular).color(quiet));
    });
    ui.add_space(2.0);
}

/// "+ New collection" at the end of the panel: quiet until pointed at.
fn new_collection_row(ui: &mut egui::Ui, dragging: bool) -> egui::Response {
    let p = theme::pal(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 36.0), Sense::click());
    let hv = theme::toggle(ui.ctx(), resp.id.with("hv"), resp.hovered() || (dragging && resp.contains_pointer()), theme::FAST);
    if hv > 0.0 {
        ui.painter().rect_filled(rect, ui_kit::RADIUS, theme::fade(p.sb_hover, hv));
    }
    if dragging && resp.contains_pointer() {
        ui.painter().rect_stroke(rect, ui_kit::RADIUS, Stroke::new(1.5, p.text), StrokeKind::Inside);
    }
    let quiet = if p.dark { Color32::from_rgba_unmultiplied(236, 236, 238, 158) } else { Color32::from_rgba_unmultiplied(23, 24, 27, 128) };
    let color = theme::mix(quiet, p.text, hv);
    ui.painter().text(Pos2::new(rect.left() + 19.0, rect.center().y), Align2::CENTER_CENTER, icon::PLUS, FontId::proportional(16.0), color);
    ui.painter().text(Pos2::new(rect.left() + 41.0, rect.center().y), Align2::LEFT_CENTER, "New collection", theme::font(13.5, W::Regular), color);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A small dark label on a picture (a clip's length, an upload).
fn pill(p: &egui::Painter, pos: Pos2, anchor: Align2, text: &str) {
    let g = p.layout_no_wrap(text.to_owned(), theme::font(11.5, W::Semibold), Color32::WHITE);
    let r = anchor.anchor_size(pos, g.size() + vec2(12.0, 4.0));
    p.rect_filled(r, 4, Color32::from_black_alpha(158));
    p.galley(r.min + vec2(6.0, 2.0), g, Color32::WHITE);
}

/// "Saving 42%" over a picture, with a bar along its bottom.
fn saving_label(ui: &egui::Ui, thumb: Rect, text: &str, f: f32) {
    let p = ui.painter();
    let g = p.layout_no_wrap(text.to_owned(), theme::font(13.5, W::Semibold), Color32::WHITE);
    let w = g.size().x + 18.0;
    let c = thumb.center();
    crate::shell::status_dot(p, Pos2::new(c.x - w / 2.0 + 4.0, c.y), theme::pal(ui).live, ui.input(|i| i.time));
    p.galley(Pos2::new(c.x - w / 2.0 + 18.0, c.y - g.size().y / 2.0), g, Color32::WHITE);
    let bar = Rect::from_min_max(Pos2::new(thumb.left(), thumb.bottom() - 3.0), thumb.max);
    p.rect_filled(bar, 0, Color32::from_white_alpha(46));
    p.rect_filled(Rect::from_min_max(bar.min, Pos2::new(bar.left() + bar.width() * f.clamp(0.0, 1.0), bar.bottom())), 0, Color32::WHITE);
}

/// A card's selection circle: hollow when not selected, a filled check when
/// it is. `t`: how shown it is (it fades in on hover).
fn check_circle(p: &egui::Painter, c: Pos2, sel: f32, hovered: bool, t: f32) {
    let dark = Color32::from_rgb(20, 21, 24);
    p.circle_filled(c, 11.0, theme::fade(Color32::from_black_alpha(if hovered { 110 } else { 64 }), t * (1.0 - sel)));
    p.circle_stroke(c, 10.25, Stroke::new(1.5, theme::fade(Color32::WHITE, t * 0.92)));
    if sel > 0.0 {
        p.circle_filled(c, 11.0 * (0.6 + 0.4 * sel), theme::fade(Color32::WHITE, t * sel));
        let tick = vec![c + vec2(-4.5, 0.5), c + vec2(-1.5, 3.5), c + vec2(4.5, -3.0)];
        p.add(egui::Shape::line(tick, Stroke::new(2.0, theme::fade(dark, t * sel))));
    }
}

/// Edit or Share on a card's picture: dark glass, or white for Share.
fn overlay_icon(ui: &mut egui::Ui, glyph: &str, light: bool, tip: &str) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(30.0), Sense::click());
    let hv = theme::toggle(ui.ctx(), resp.id.with("hv"), resp.hovered(), theme::FAST);
    let (fill, fg) = if light {
        (theme::mix(Color32::WHITE, Color32::from_gray(225), hv), Color32::from_rgb(20, 21, 24))
    } else {
        (Color32::from_rgba_unmultiplied(14, 15, 18, (184.0 + 46.0 * hv) as u8), Color32::WHITE)
    };
    ui.painter().rect_filled(rect, ui_kit::RADIUS, fill);
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, glyph, FontId::proportional(15.0), fg);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(tip)
}

/// A thin vertical divider in a row of buttons.
fn vrule(ui: &mut egui::Ui) {
    let p = theme::pal(ui);
    let (r, _) = ui.allocate_exact_size(vec2(9.0, 20.0), Sense::hover());
    ui.painter().vline(r.center().x, r.y_range(), Stroke::new(1.0, p.line2));
}

/// The ways to get a clip out. `full`: the right-click menu's longer list.
pub(crate) fn share_items(ui: &mut egui::Ui, anchor: Pos2, full: bool) -> Option<crate::library::ShareChoice> {
    use crate::library::ShareChoice;
    let mut choice = None;
    if Item::new("Copy clip").icon(icon::COPY).keys(Some(crate::shortcuts::command("C"))).tip(format!("Then paste it into Discord, a chat or a folder ({})", crate::hotkey_label_cmd("V"))).show(ui).clicked() {
        choice = Some(ShareChoice::Copy);
    }
    if share::HAS_SHARE_SHEET && Item::new(share::SHARE_SHEET_LABEL).icon(icon::PAPER_PLANE_TILT).show(ui).clicked() {
        choice = Some(ShareChoice::Sheet(anchor));
    }
    if Item::new("Upload to HesteFiles…").icon(icon::CLOUD_ARROW_UP).show(ui).clicked() {
        choice = Some(ShareChoice::Upload);
    }
    ui_kit::menu_sep(ui);
    if Item::new("Save the sound as MP3…").icon(icon::FILE_AUDIO).show(ui).clicked() {
        choice = Some(ShareChoice::Mp3);
    }
    if Item::new(crate::reveal_label()).icon(icon::FOLDER_OPEN).show(ui).clicked() {
        choice = Some(ShareChoice::Reveal);
    }
    if !full && share::CAN_DRAG_OUT {
        ui_kit::menu_sep(ui);
        ui_kit::menu_head(ui, "Or drag the clip into any app");
    }
    choice
}

/// Collections to put clips in: each one, ticked when they're all in it
/// already (picking it then takes them out), and a new one.
fn collection_items(ui: &mut egui::Ui, cols: &crate::collections::Collections, paths: &[PathBuf]) -> Option<Action> {
    let mut action = None;
    let mut list: Vec<_> = cols.list().iter().collect();
    list.sort_by_key(|c| c.name.to_lowercase());
    for c in list {
        let all_in = !paths.is_empty() && paths.iter().all(|p| cols.contains(&c.id, p));
        let tip = if all_in { "Take out of this collection" } else { "Add to this collection" };
        if Item::new(c.name.clone()).check(all_in).tip(tip).show(ui).clicked() {
            action = Some(Action::InCollection(paths.to_vec(), c.id.clone(), !all_in));
        }
    }
    if !cols.list().is_empty() {
        ui_kit::menu_sep(ui);
    }
    if Item::new("New collection…").icon(icon::PLUS).show(ui).clicked() {
        action = Some(Action::NewCollection(paths.to_vec()));
    }
    action
}

/// The game folders to move clips to: each of the library's, ticked where
/// the clip is, and Other (out of any folder) when `offer_other`.
fn move_items(ui: &mut egui::Ui, folders: &[String], current: Option<&str>, offer_other: bool) -> Option<Option<String>> {
    let mut picked = None;
    ui_kit::menu_head(ui, "Game folder");
    for f in folders {
        let here = Some(f.as_str()) == current;
        if Item::new(f.clone()).check(here).enabled(!here).show(ui).clicked() {
            picked = Some(Some(f.clone()));
        }
    }
    if offer_other {
        ui_kit::menu_sep(ui);
        if Item::new("Other").check(current.is_none() && !folders.is_empty() && false).tip("Out of any game's folder").show(ui).clicked() {
            picked = Some(None);
        }
    }
    picked
}

/// What can be done with a game's folder or a collection (right-click).
fn view_menu(ui: &mut egui::Ui, filter: &Filter) -> Option<Action> {
    let mut action = None;
    match filter {
        Filter::Folder(name) => {
            if Item::new(crate::reveal_label()).icon(icon::FOLDER_OPEN).show(ui).clicked() {
                action = Some(Action::RevealFolder(name.clone()));
            }
            let desktop = name == clips::DESKTOP;
            let mut item = Item::new("Rename folder…").icon(icon::PENCIL_SIMPLE).enabled(!desktop);
            if desktop {
                item = item.tip("Clips with no game always go here");
            }
            if item.show(ui).clicked() {
                action = Some(Action::RenameFolder(name.clone()));
            }
        }
        Filter::Collection(id) => {
            if Item::new("Rename collection…").icon(icon::PENCIL_SIMPLE).show(ui).clicked() {
                action = Some(Action::RenameCollection(id.clone()));
            }
            if Item::new("Delete collection…").icon(icon::TRASH).danger().show(ui).clicked() {
                action = Some(Action::DeleteCollection(id.clone()));
            }
        }
        Filter::All | Filter::Loose => {}
    }
    action
}
