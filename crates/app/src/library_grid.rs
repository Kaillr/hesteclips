//! The clip grid's model, made to stay quick with thousands of clips.
//!
//! - **What's shown** ([`Shown`]): the filter, the search, the order, the
//!   days and the panel's counts, worked out only when one of them (or the
//!   library, or a collection) changes, never per frame.
//! - **Where everything is** ([`Layout`]): each day's top and each card's
//!   place, from row arithmetic. Drawing a frame looks only at the rows on
//!   screen, however long the library.
//! - **Motion** ([`Moves`]): when the layout changes (another game or
//!   collection, a search, another order, clips coming and going, another
//!   number of columns), the one before is kept. Every card still shown
//!   slides from where it was in it, on screen or not: one that was far off
//!   starts just past the edge of the view on its side, so it comes in from
//!   the right direction without streaking across thousands of points. New
//!   ones fade in where they land; gone ones go at once.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use egui::{Pos2, Vec2, vec2};

use crate::clips::Clip;
use crate::collections::Collections;
use crate::library::{Filter, View};
use crate::settings::LibrarySort;
use crate::theme;

pub const GAP_X: f32 = 16.0;
pub const GAP_Y: f32 = 22.0;
pub const CAPTION_H: f32 = 48.0;
const MIN_CARD_W: f32 = 190.0;
/// A day's heading's height, and the space around it.
pub const DAY_H: f32 = 22.0;
const DAY_ABOVE_FIRST: f32 = 6.0;
const DAY_ABOVE: f32 = 14.0;
const DAY_BELOW: f32 = 12.0;
/// Above the cards when they aren't grouped by day.
const NO_DAY_ABOVE: f32 = 6.0;

/// How long things slide and fade when the grid is laid out again.
pub const SLIDE: f32 = theme::NORMAL * 1.4;

/// A card on the grid.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum GridItem {
    /// A clip, by its index in the library's list.
    Clip(usize),
    /// A clip still being recorded or saved (the n-th).
    Placeholder(usize),
    /// A "Save as new clip" render that has no file yet (its index in the
    /// renders).
    NewRender(usize),
}

/// A day's (or the one undated group's) cards.
pub struct Group {
    pub day: Option<chrono::NaiveDate>,
    pub items: Vec<GridItem>,
    /// How many of them are clips (not ones being made).
    pub clips: usize,
}

/// What the library shows, worked out once per change.
#[derive(Default)]
pub struct Shown {
    key: Option<ShownKey>,
    /// The clips in the filter, before the search.
    pub in_filter: usize,
    /// The clips shown, in order (indices into the library's list).
    pub clips: Vec<usize>,
    pub groups: Vec<Group>,
    pub days: Vec<chrono::NaiveDate>,
    /// The panel's games and collections, with their counts.
    pub games: Vec<View>,
    pub collections: Vec<View>,
    /// Goes up whenever any of the above changes.
    pub version: u64,
    /// The cards as they were before the last change, by group, with what
    /// the grid followed them by: to find what was in view then.
    pub before: Vec<Vec<(Key, GridItem)>>,
}

/// Everything [`Shown`] is worked out from.
#[derive(Clone, PartialEq)]
struct ShownKey {
    library: u64,
    collections: u64,
    filter: Filter,
    query: String,
    sort: LibrarySort,
    lengths: u64,
    folded_today: bool,
    live: (usize, Vec<usize>),
    today: chrono::NaiveDate,
}

/// What [`Shown::update`] needs from the app.
pub struct Inputs<'a> {
    pub clips: &'a [Clip],
    /// Goes up whenever the library's list changes.
    pub library_version: u64,
    pub collections: &'a Collections,
    pub filter: &'a Filter,
    pub query: &'a str,
    pub sort: LibrarySort,
    pub lengths_version: u64,
    /// Clips being recorded or saved, and new clips being rendered (their
    /// indices in `renders`, and where each will be saved).
    pub placeholders: usize,
    pub new_renders: Vec<usize>,
    pub renders: &'a [PathBuf],
}

impl Shown {
    /// Worked out again if anything it's made from changed. `length`: a
    /// clip's length, if known (for the Longest order).
    pub fn update(&mut self, inp: Inputs, folded_today: bool, mut length: impl FnMut(&Clip) -> Option<std::time::Duration>) -> bool {
        let today = chrono::Local::now().date_naive();
        let query = inp.query.trim().to_lowercase();
        let key = ShownKey {
            library: inp.library_version,
            collections: inp.collections.version(),
            filter: inp.filter.clone(),
            query: query.clone(),
            sort: inp.sort,
            lengths: if inp.sort == LibrarySort::Longest { inp.lengths_version } else { 0 },
            folded_today,
            live: (inp.placeholders, inp.new_renders.clone()),
            today,
        };
        if self.key.as_ref() == Some(&key) {
            return false;
        }
        let clips = inp.clips;
        let sets = inp.collections.sets();
        let in_filter = |c: &Clip| match inp.filter {
            Filter::Collection(id) => sets.get(id).is_some_and(|s| s.contains(&c.path)),
            f => f.matches(c, inp.collections),
        };
        let matches = |c: &Clip| query.is_empty() || c.title().to_lowercase().contains(&query) || c.folder.as_ref().is_some_and(|f| f.to_lowercase().contains(&query));
        let filtered: Vec<usize> = (0..clips.len()).filter(|&i| in_filter(&clips[i])).collect();
        let mut shown: Vec<usize> = filtered.iter().copied().filter(|&i| matches(&clips[i])).collect();
        match inp.sort {
            LibrarySort::Newest => {}
            LibrarySort::Oldest => shown.reverse(),
            LibrarySort::Longest => {
                let lengths: HashMap<usize, Option<std::time::Duration>> = shown.iter().map(|&i| (i, length(&clips[i]))).collect();
                shown.sort_by(|a, b| lengths[b].cmp(&lengths[a]));
            }
            LibrarySort::Largest => shown.sort_by(|&a, &b| clips[b].size_bytes.cmp(&clips[a].size_bytes)),
        }
        let old = std::mem::take(&mut self.groups);
        self.before = old.iter().map(|g| g.items.iter().map(|it| (key_for(it, clips, inp.renders), it.clone())).collect()).collect();
        self.groups = group(clips, &shown, inp.sort, &query, inp.placeholders, &inp.new_renders, today);
        self.days = self.groups.iter().filter_map(|g| g.day).collect();
        self.in_filter = filtered.len();
        self.clips = shown;
        let (games, collections) = views(clips, inp.collections, &sets);
        self.games = games;
        self.collections = collections;
        self.key = Some(key);
        self.version += 1;
        true
    }
}

/// The panel's games and collections, as [`crate::library::views`], with each
/// collection counted through its set.
fn views(clips: &[Clip], cols: &Collections, sets: &HashMap<String, HashSet<PathBuf>>) -> (Vec<View>, Vec<View>) {
    let (games, mut collections) = crate::library::views_without_counts(clips, cols);
    for v in &mut collections {
        if let Filter::Collection(id) = &v.filter {
            v.count = sets.get(id).map_or(0, |s| clips.iter().filter(|c| s.contains(&c.path)).count());
        }
    }
    (games, collections)
}

/// Into days (or one group when sorted by length or size). New cards (a
/// recording, a clip being saved) belong to today.
fn group(clips: &[Clip], shown: &[usize], sort: LibrarySort, query: &str, placeholders: usize, new_renders: &[usize], today: chrono::NaiveDate) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();
    let mut live: Vec<GridItem> = (0..placeholders).map(GridItem::Placeholder).chain(new_renders.iter().map(|&i| GridItem::NewRender(i))).collect();
    let by_day = sort.by_day();
    if !live.is_empty() && query.is_empty() && by_day && sort == LibrarySort::Newest {
        groups.push(Group { day: Some(today), items: std::mem::take(&mut live), clips: 0 });
    }
    for &i in shown {
        let day = by_day.then(|| clips[i].day());
        match groups.last_mut() {
            Some(g) if g.day == day => {
                g.items.push(GridItem::Clip(i));
                g.clips += 1;
            }
            _ => groups.push(Group { day, items: vec![GridItem::Clip(i)], clips: 1 }),
        }
    }
    if !live.is_empty() {
        // Oldest first, or not by day: the new ones go at the end of today
        // or the start of the list.
        match groups.last_mut() {
            Some(g) if g.day == Some(today) => g.items.extend(live),
            _ if !by_day => match groups.first_mut() {
                Some(g) => {
                    live.append(&mut g.items);
                    g.items = live;
                }
                None => groups.push(Group { day: None, items: live, clips: 0 }),
            },
            _ => groups.push(Group { day: Some(today), items: live, clips: 0 }),
        }
    }
    groups
}

/// What the grid follows a card by from one layout to the next. A new clip
/// being made has the key of the clip it becomes, so its card simply stays.
/// `renders`: where each render will be saved.
pub fn key_for(item: &GridItem, clips: &[Clip], renders: &[PathBuf]) -> Key {
    match item {
        // (The library may have changed since: a clip that's gone is no one's.)
        GridItem::Clip(i) => clips.get(*i).map_or(Key::Placeholder(usize::MAX), |c| Key::Clip(c.path.clone())),
        GridItem::NewRender(i) => renders.get(*i).map_or(Key::Placeholder(usize::MAX - 1 - i), |p| Key::Clip(p.clone())),
        GridItem::Placeholder(n) => Key::Placeholder(*n),
    }
}

/// The grid's columns and card size for a width.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Metrics {
    pub cols: usize,
    pub card: Vec2,
}

impl Metrics {
    pub fn for_width(avail: f32) -> Self {
        let cols = (((avail + GAP_X) / (MIN_CARD_W + GAP_X)).floor() as usize).clamp(1, 6);
        let card_w = (avail - GAP_X * (cols - 1) as f32) / cols as f32;
        Metrics { cols, card: vec2(card_w, card_w * 9.0 / 16.0 + CAPTION_H) }
    }

    /// A card's place in its group, from the group's first row.
    pub fn offset(&self, i: usize) -> Vec2 {
        vec2((i % self.cols) as f32 * (self.card.x + GAP_X), (i / self.cols) as f32 * (self.card.y + GAP_Y))
    }

    fn rows_height(&self, n: usize) -> f32 {
        let rows = n.div_ceil(self.cols);
        rows as f32 * self.card.y + rows.saturating_sub(1) as f32 * GAP_Y
    }
}

/// A group's place: its heading's top (if it has one) and its cards' top,
/// in the grid's content (from its top-left, scrolling not applied).
#[derive(Clone, Copy, Debug)]
pub struct GroupPlace {
    pub head: Option<f32>,
    pub cards: f32,
    /// The cards' full height (open), and how open the group is now.
    pub height: f32,
}

/// Where everything on the grid is, in the grid's content.
#[derive(Clone)]
pub struct Layout {
    pub metrics: Metrics,
    pub groups: Vec<GroupPlace>,
    /// Each card's group and place in it, by its key; each day's group.
    /// Made once per change of what's shown (shared, never copied).
    index: std::sync::Arc<Index>,
    pub height: f32,
}

#[derive(Default)]
struct Index {
    places: HashMap<Key, (usize, usize)>,
    days: HashMap<chrono::NaiveDate, usize>,
}

/// What a card or heading is followed by from one layout to the next.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum Key {
    Clip(PathBuf),
    Placeholder(usize),
    Day(chrono::NaiveDate),
}

impl Layout {
    /// `open`: how open each group is (folding slides its height).
    /// `key`: what each card is followed by.
    pub fn new(groups: &[Group], metrics: Metrics, open: impl Fn(usize) -> f32, key: impl Fn(&GridItem) -> Key) -> Self {
        let mut index = Index::default();
        for (gi, g) in groups.iter().enumerate() {
            if let Some(d) = g.day {
                index.days.insert(d, gi);
            }
            for (i, item) in g.items.iter().enumerate() {
                index.places.insert(key(item), (gi, i));
            }
        }
        let mut l = Layout { metrics, groups: Vec::new(), index: std::sync::Arc::new(index), height: 0.0 };
        l.place_groups(groups, open);
        l
    }

    /// The same cards at another width (the window resizing): the index of
    /// cards is kept (it doesn't depend on the width), only the places are
    /// found again: cheap, however many clips.
    pub fn resized(&self, groups: &[Group], metrics: Metrics, open: impl Fn(usize) -> f32) -> Self {
        let mut l = Layout { metrics, groups: Vec::new(), index: self.index.clone(), height: 0.0 };
        l.place_groups(groups, open);
        l
    }

    /// Where each group is, with how open each is now: cheap (one step per
    /// day), so a day folding moves what's below every frame.
    pub fn place_groups(&mut self, groups: &[Group], open: impl Fn(usize) -> f32) {
        let mut y = 0.0;
        self.groups.clear();
        for (gi, g) in groups.iter().enumerate() {
            let head = g.day.map(|_| {
                y += if gi == 0 { DAY_ABOVE_FIRST } else { DAY_ABOVE };
                let at = y;
                y += DAY_H + DAY_BELOW;
                at
            });
            if head.is_none() {
                y += NO_DAY_ABOVE;
            }
            let cards = y;
            let height = self.metrics.rows_height(g.items.len()) * open(gi);
            y += height;
            self.groups.push(GroupPlace { head, cards, height });
        }
        self.height = y;
    }

    /// Where a card is, by its key, in the grid's content.
    pub fn card(&self, key: &Key) -> Option<Pos2> {
        let (g, i) = *self.index.places.get(key)?;
        Some(Pos2::new(0.0, self.groups.get(g)?.cards) + self.metrics.offset(i))
    }

    /// A card's group and place in it, by its key.
    pub fn slot(&self, key: &Key) -> Option<(usize, usize)> {
        self.index.places.get(key).copied()
    }

    /// Where a day's heading is.
    pub fn day(&self, d: chrono::NaiveDate) -> Option<Pos2> {
        let g = *self.index.days.get(&d)?;
        self.groups.get(g)?.head.map(|y| Pos2::new(0.0, y))
    }

    /// The cards of group `g` whose rows are within `top..bottom` (content
    /// coordinates): row arithmetic, so a frame looks only at what's on
    /// screen.
    pub fn visible(&self, g: usize, len: usize, top: f32, bottom: f32) -> std::ops::Range<usize> {
        let place = self.groups[g];
        let row_h = self.metrics.card.y + GAP_Y;
        let cols = self.metrics.cols;
        if place.cards + place.height < top || place.cards > bottom {
            return 0..0;
        }
        let first = (((top - place.cards) / row_h).floor().max(0.0)) as usize;
        let last = (((bottom - place.cards) / row_h).ceil().max(0.0)) as usize;
        (first * cols).min(len)..((last + 1) * cols).min(len)
    }
}

/// The grid's motion: the layout before a change, and when it changed.
#[derive(Default)]
pub struct Moves {
    /// The layout now and what it was made from.
    now: Option<(Layout, LayoutId)>,
    /// The one before, while things move from it.
    before: Option<Before>,
    /// The view (content coordinates) last frame: where things were seen.
    view: (f32, f32),
}

struct Before {
    layout: Layout,
    changed: f64,
    /// The view as it was: things far outside it start just past its edge.
    view: (f32, f32),
    /// What was in view then: it slides out from there, so it's drawn even
    /// where it's going is off screen. (Anything else that's off screen now
    /// was off screen then too, so its slide never crosses the view.)
    seen: Vec<(Key, GridItem)>,
}

/// What a layout was made from: when it changes, things move.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct LayoutId {
    pub shown: u64,
    pub metrics: (usize, i32),
}

/// Where something is drawn now and how visible it is.
pub struct Placed {
    pub at: Pos2,
    pub shown: f32,
}

impl Moves {
    /// The layout for this frame. `id`: what it's made from; `still`: a
    /// change that shouldn't move anything (arriving from another page, or a
    /// group folding, which moves what's below by itself). `items`: the
    /// cards before the change, by group (to find what was in view).
    /// `make`: the new layout, called only when `id` changed; `place`: each
    /// frame, the groups placed with how open they are now.
    /// `resize`: the same cards at the new width, from the layout now (only
    /// called when just the width changed).
    #[allow(clippy::too_many_arguments)]
    pub fn update(
        &mut self,
        id: LayoutId,
        now: f64,
        still: bool,
        make: impl FnOnce() -> Layout,
        resize: impl FnOnce(&Layout) -> Layout,
        place: impl FnOnce(&mut Layout),
        before_items: impl FnOnce() -> Vec<Vec<(Key, GridItem)>>,
    ) {
        if let Some((layout, was)) = &mut self.now
            && *was == id
        {
            place(layout);
            return;
        }
        let layout = match &self.now {
            // Only the width: the cards are where they were in the list.
            Some((old, was)) if was.shown == id.shown => resize(old),
            _ => make(),
        };
        match &self.now {
            // Only what's shown changing moves things; another width (the
            // window resizing) or column count puts them in place at once.
            Some((old, was)) if !still && was.shown != id.shown => {
                let (top, bottom) = self.view;
                let items = before_items();
                let mut seen = Vec::new();
                for (g, group) in items.iter().enumerate().take(old.groups.len()) {
                    for i in old.visible(g, group.len(), top, bottom) {
                        seen.push(group[i].clone());
                    }
                }
                self.before = Some(Before { layout: old.clone(), changed: now, view: self.view, seen });
            }
            _ => self.before = None,
        }
        self.now = Some((layout, id));
    }

    /// The view this frame, in content coordinates (top, bottom).
    pub fn set_view(&mut self, view: (f32, f32)) {
        self.view = view;
    }

    /// The layout this frame.
    pub fn layout(&self) -> &Layout {
        &self.now.as_ref().expect("update first").0
    }

    /// Whether anything's still moving (then repaint).
    pub fn moving(&self, now: f64) -> bool {
        self.before.as_ref().is_some_and(|b| ((now - b.changed) as f32) < SLIDE)
    }

    /// What was in view before the change, while things still move.
    pub fn seen(&self, now: f64) -> &[(Key, GridItem)] {
        match &self.before {
            Some(b) if ((now - b.changed) as f32) < SLIDE => &b.seen,
            _ => &[],
        }
    }

    /// Where `key` (now at `target`, content coordinates) is drawn, given
    /// the view now (top, bottom, content coordinates). `day`: the day the
    /// card is under (its heading's key): a card that wasn't shown before
    /// moves with its day's heading as it fades in, so a day's cards and its
    /// heading arrive together (switching between games with no clip in
    /// common, every card is new).
    pub fn place(&self, key: &Key, target: Pos2, now: f64, view: (f32, f32), day: Option<chrono::NaiveDate>) -> Placed {
        let Some(b) = &self.before else { return Placed { at: target, shown: 1.0 } };
        let t = ((now - b.changed) as f32 / SLIDE).clamp(0.0, 1.0);
        if t >= 1.0 {
            return Placed { at: target, shown: 1.0 };
        }
        let e = theme::ease_out(t);
        let from = match key {
            Key::Day(d) => b.layout.day(*d),
            k => b.layout.card(k),
        };
        if let Some(from) = from {
            let from = b.start(from, target, view);
            return Placed { at: from + (target - from) * e, shown: 1.0 };
        }
        // New. Under a day that was shown: it starts as far from its place as
        // the heading does from its own, and travels with it, fading in.
        let heading = day.and_then(|d| Some((b.layout.day(d)?, self.layout().day(d)?)));
        let Some((was, is)) = heading else {
            return Placed { at: target, shown: e };
        };
        let start = b.start(was, is, view);
        Placed { at: target + (start - is) * (1.0 - e), shown: e }
    }
}

impl Before {
    /// Where something that was at `from` (content coordinates, before the
    /// change) starts sliding to `target` from: where it was on screen then,
    /// kept on screen now (the view may have scrolled to the top); from far
    /// off, just past the view's edge on its side, so it arrives from the
    /// right direction without crossing the whole library.
    fn start(&self, from: Pos2, target: Pos2, view: (f32, f32)) -> Pos2 {
        let margin = self.layout.metrics.card.y;
        let on_screen = from.y - self.view.0 + view.0;
        let was_seen = from.y + margin >= self.view.0 && from.y <= self.view.1;
        let y = if was_seen { on_screen } else if from.y < self.view.0 { (view.0 - margin).min(target.y) } else { (view.1 + margin).max(target.y) };
        Pos2::new(from.x, y)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clip(i: usize, folder: &str) -> Clip {
        let day = 1 + i / 10;
        Clip {
            path: PathBuf::from(format!("/lib/{folder}/clip_2026-01-{day:02}_12-00-{:02}.mp4", 59 - i % 10)),
            name: String::new(),
            modified: std::time::SystemTime::UNIX_EPOCH,
            created: None,
            size_bytes: i as u64,
            id: None,
            original: None,
            folder: Some(folder.into()),
        }
    }

    fn layout_of(clips: &[Clip], filter: &Filter, width: f32) -> (Shown, Layout) {
        let cols = Collections::default();
        let mut shown = Shown::default();
        shown.update(Inputs { clips, library_version: 1, collections: &cols, filter, query: "", sort: LibrarySort::Newest, lengths_version: 0, placeholders: 0, new_renders: Vec::new(), renders: &[] }, false, |_| None);
        let l = Layout::new(&shown.groups, Metrics::for_width(width), |_| 1.0, |it| key_for(it, clips, &[]));
        (shown, l)
    }

    /// The rows on screen are found by arithmetic; every card in them, and
    /// none far outside.
    #[test]
    fn visible_rows_only() {
        let clips: Vec<Clip> = (0..300).map(|i| clip(i, "osu!")).collect();
        let (shown, l) = layout_of(&clips, &Filter::All, 900.0);
        let (top, bottom) = (1500.0, 2200.0);
        let mut seen = 0;
        for (g, group) in shown.groups.iter().enumerate() {
            let r = l.visible(g, group.items.len(), top, bottom);
            for i in 0..group.items.len() {
                let y = l.groups[g].cards + l.metrics.offset(i).y;
                let on = y + l.metrics.card.y >= top && y <= bottom;
                if on {
                    assert!(r.contains(&i), "group {g} card {i} at {y} on screen but not visited");
                }
            }
            seen += r.len();
        }
        assert!(seen < 60, "visited {seen}");
    }

    /// A filter: a card that was far off screen slides in from just past
    /// the view's edge on its side, not from thousands of points away and
    /// not popping in; a card that wasn't shown fades in where it belongs.
    #[test]
    fn off_screen_cards_slide_in() {
        let mut clips: Vec<Clip> = (0..200).map(|i| clip(i, if i % 2 == 0 { "osu!" } else { "Dota 2" })).collect();
        clips.push(clip(500, "CS2"));
        let (_, all) = layout_of(&clips, &Filter::All, 900.0);
        let (_, osu) = layout_of(&clips, &Filter::Folder("osu!".into()), 900.0);
        let mut m = Moves::default();
        let id = |s| LayoutId { shown: s, metrics: (4, 900) };
        let view = (0.0, 800.0);
        m.update(id(1), 0.0, false, || all.clone(), |_| unreachable!(), |_| {}, Vec::new);
        m.set_view(view);
        m.update(id(2), 10.0, false, || osu.clone(), |_| unreachable!(), |_| {}, Vec::new);
        // An osu! clip that was below the view in All clips and is in view now:
        // it comes up from just past the view's bottom edge.
        let (key, was, target) = (0..200)
            .step_by(2)
            .map(|i| Key::Clip(clips[i].path.clone()))
            .map(|k| (k.clone(), all.card(&k).unwrap(), osu.card(&k).unwrap()))
            .find(|(_, was, target)| was.y > view.1 && target.y < view.1 - 100.0)
            .expect("a clip from below the view into it");
        let p = m.place(&key, target, 10.0, view, None);
        assert_eq!(p.shown, 1.0);
        assert!(p.at.y > view.1 && p.at.y <= view.1 + all.metrics.card.y + 0.5, "starts just past the view's bottom (was {}): {}", was.y, p.at.y);
        let p = m.place(&key, target, 10.0 + f64::from(SLIDE), view, None);
        assert_eq!(p.at, target);
        // Far below before and after: never seen moving, so it's simply there.
        let far = Key::Clip(clips[198].path.clone());
        let (fwas, ftarget) = (all.card(&far).unwrap(), osu.card(&far).unwrap());
        assert!(fwas.y > view.1 && ftarget.y > view.1);
        assert_eq!(m.place(&far, ftarget, 10.0, view, None).at, ftarget);
        // In view before and after: slides all the way from where it was.
        let near = Key::Clip(clips[2].path.clone());
        let p = m.place(&near, osu.card(&near).unwrap(), 10.0, view, None);
        assert_eq!(p.at, all.card(&near).unwrap());
    }

    /// The window resizing: cards are in place at once, every frame (they
    /// follow the window, never trail it); only what's shown changing slides.
    #[test]
    fn resizing_never_slides() {
        let clips: Vec<Clip> = (0..40).map(|i| clip(i, "osu!")).collect();
        let (_, wide) = layout_of(&clips, &Filter::All, 1200.0);
        let (_, narrow) = layout_of(&clips, &Filter::All, 700.0);
        let mut m = Moves::default();
        let view = (0.0, 800.0);
        m.update(LayoutId { shown: 1, metrics: (wide.metrics.cols, 1200) }, 0.0, false, || wide.clone(), |_| unreachable!(), |_| {}, Vec::new);
        m.set_view(view);
        let groups = layout_of(&clips, &Filter::All, 700.0).0.groups;
        m.update(LayoutId { shown: 1, metrics: (narrow.metrics.cols, 700) }, 1.0, false, || unreachable!("only the width changed"), |old| old.resized(&groups, narrow.metrics, |_| 1.0), |_| {}, Vec::new);
        assert!(!m.moving(1.0));
        let key = Key::Clip(clips[7].path.clone());
        let target = narrow.card(&key).unwrap();
        assert_ne!(wide.card(&key).unwrap(), target, "it did move");
        let p = m.place(&key, target, 1.0, view, None);
        assert_eq!((p.at, p.shown), (target, 1.0));
    }

    /// Two games with no clip in common: every card is new, and each moves
    /// with its day's heading (from where the heading was) while it fades
    /// in, so a day arrives together instead of its heading sliding past
    /// cards that are already in place.
    #[test]
    fn new_cards_move_with_their_day() {
        // Days 1-3: osu! has one clip a day, Dota 2 has ten a day, so in Dota 2
        // day 2's heading is far lower than in osu!.
        let mut clips: Vec<Clip> = (0..30).map(|i| clip(i, "Dota 2")).collect();
        for d in 0..3 {
            let mut c = clip(d * 10, "osu!");
            c.path = PathBuf::from(format!("/lib/osu!/clip_2026-01-{:02}_09-00-00.mp4", d + 1));
            clips.push(c);
        }
        let (_, osu) = layout_of(&clips, &Filter::Folder("osu!".into()), 900.0);
        let (_, dota) = layout_of(&clips, &Filter::Folder("Dota 2".into()), 900.0);
        let mut m = Moves::default();
        let id = |s| LayoutId { shown: s, metrics: (4, 900) };
        let view = (0.0, 2000.0);
        m.update(id(1), 0.0, false, || osu.clone(), |_| unreachable!(), |_| {}, Vec::new);
        m.set_view(view);
        m.update(id(2), 10.0, false, || dota.clone(), |_| unreachable!(), |_| {}, Vec::new);
        let day = chrono::NaiveDate::from_ymd_opt(2026, 1, 2).unwrap();
        let (was, is) = (osu.day(day).unwrap(), dota.day(day).unwrap());
        assert!(is.y - was.y > 100.0, "the heading moves down");
        let card = Key::Clip(clips[15].path.clone());
        let target = dota.card(&card).unwrap();
        let p = m.place(&card, target, 10.0, view, Some(day));
        assert_eq!(p.shown, 0.0);
        assert!((p.at.y - (target.y - (is.y - was.y))).abs() < 0.5, "starts as far up as its heading: {} vs {}", p.at.y, target.y);
        let heading = m.place(&Key::Day(day), is, 10.0 + f64::from(SLIDE) / 2.0, view, None);
        let p = m.place(&card, target, 10.0 + f64::from(SLIDE) / 2.0, view, Some(day));
        assert!(((p.at.y - target.y) - (heading.at.y - is.y)).abs() < 0.5, "travels with it");
        assert_eq!(m.place(&card, target, 10.0 + f64::from(SLIDE), view, Some(day)).at, target);
    }

    /// Worked out again only when something changed.
    #[test]
    fn shown_only_when_changed() {
        let clips: Vec<Clip> = (0..50).map(|i| clip(i, "osu!")).collect();
        let cols = Collections::default();
        let mut shown = Shown::default();
        let inp = |q: &'static str| Inputs { clips: &clips, library_version: 1, collections: &cols, filter: &Filter::All, query: q, sort: LibrarySort::Newest, lengths_version: 0, placeholders: 0, new_renders: Vec::new(), renders: &[] };
        assert!(shown.update(inp(""), false, |_| None));
        assert!(!shown.update(inp(""), false, |_| None));
        assert!(shown.update(inp("12"), false, |_| None));
        assert_eq!(shown.version, 2);
    }
}
