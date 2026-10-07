//! osu!: what was played in a clip, for its name — "412pp · 98.52% FC ·
//! +HDDT · Freedom Dive [FOUR DIMENSIONS]".
//!
//! osu! has no API of its own; tosu (or the older gosumemory) reads it and
//! serves what it's doing on `127.0.0.1:24050`. We read tosu's v2 data, else
//! the gosumemory-style `/json` both of them serve. Without either, osu!
//! stable's window title still names the map while playing.
//!
//! The state is read twice a second into a history of **plays**: the map, when
//! it started and ended (its results screen counts as part of it), and how it
//! went. A clip is named after the play that was on screen longest in it — not
//! what osu! shows when it's saved, which may already be the song list.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::settings::GameTitles;

/// Where tosu and gosumemory serve osu!'s state.
const TOSU: &str = "http://127.0.0.1:24050";
/// Plays older than this are forgotten.
const KEEP: Duration = Duration::from_secs(4 * 3600);
/// Longest name, in characters; the map's name is cut first (it comes last).
const MAX_TITLE: usize = 110;

#[derive(Debug, Clone, PartialEq)]
pub struct Map {
    pub artist: String,
    pub title: String,
    pub version: String,
    pub stars: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stats {
    /// 0..=100.
    pub accuracy: f64,
    pub misses: u64,
    pub slider_breaks: u64,
    pub combo: u64,
    pub pp: f64,
    /// "HDDT"; empty or "NM" without mods.
    pub mods: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Screen {
    Playing,
    Results,
    Other,
}

/// One look at osu!.
#[derive(Debug, Clone)]
pub struct Reading {
    pub screen: Screen,
    pub map: Option<Map>,
    /// Who's playing, when it isn't you (a replay, spectating).
    pub other_player: Option<String>,
    pub stats: Option<Stats>,
    /// How far into the map, 0..=1, if known.
    pub progress: Option<f64>,
    pub failed: bool,
    /// The results screen's final numbers.
    pub results: Option<Stats>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Outcome {
    Playing,
    Passed,
    Failed,
    Quit,
}

#[derive(Debug, Clone)]
struct Play {
    map: Map,
    other_player: Option<String>,
    start: Instant,
    end: Instant,
    stats: Option<Stats>,
    progress: Option<f64>,
    failed: bool,
    outcome: Outcome,
    /// Its results screen is up (the play is still what's on screen).
    on_results: bool,
}

#[derive(Default)]
pub struct Tracker {
    plays: VecDeque<Play>,
}

impl Tracker {
    /// Take in a reading (`None`: osu! isn't running or can't be read).
    pub fn update(&mut self, now: Instant, reading: Option<Reading>) {
        let Some(r) = reading else {
            self.close(now);
            return;
        };
        match r.screen {
            Screen::Playing => {
                let Some(map) = r.map.clone() else { return };
                let same = self.plays.back().is_some_and(|p| {
                    p.outcome == Outcome::Playing
                        && p.map == map
                        // A retry starts over from the beginning.
                        && !matches!((p.progress, r.progress), (Some(a), Some(b)) if b + 0.02 < a)
                });
                if !same {
                    self.close(now);
                    self.plays.push_back(Play {
                        map,
                        other_player: r.other_player.clone(),
                        start: now,
                        end: now,
                        stats: None,
                        progress: None,
                        failed: false,
                        outcome: Outcome::Playing,
                        on_results: false,
                    });
                    while self.plays.front().is_some_and(|p| now.saturating_duration_since(p.end) > KEEP) {
                        self.plays.pop_front();
                    }
                }
                let p = self.plays.back_mut().unwrap();
                p.end = now;
                p.stats = r.stats.or(p.stats.take());
                p.progress = r.progress.or(p.progress);
                p.failed |= r.failed;
            }
            Screen::Results => {
                let Some(p) = self.plays.back_mut() else { return };
                if p.outcome == Outcome::Playing && !p.failed {
                    // Just finished: the results screen has the final numbers;
                    // slider breaks only the play had.
                    let breaks = p.stats.as_ref().map_or(0, |s| s.slider_breaks);
                    if let Some(mut results) = r.results {
                        results.slider_breaks = breaks;
                        if results.pp <= 0.0 {
                            results.pp = p.stats.as_ref().map_or(0.0, |s| s.pp);
                        }
                        p.stats = Some(results);
                    }
                    p.outcome = Outcome::Passed;
                    p.progress = Some(1.0);
                    p.on_results = true;
                }
                if p.on_results {
                    p.end = now;
                }
            }
            Screen::Other => self.close(now),
        }
    }

    /// The play that's open, if any, ends now.
    fn close(&mut self, now: Instant) {
        let Some(p) = self.plays.back_mut() else { return };
        if p.outcome == Outcome::Playing {
            p.outcome = if p.failed { Outcome::Failed } else { Outcome::Quit };
            p.end = now;
        }
        p.on_results = false;
    }

    /// The name for a clip from `since` to `now`: the play on screen longest
    /// in it.
    pub fn title(&self, since: Instant, now: Instant, fields: &GameTitles) -> Option<String> {
        let overlap = |p: &Play| p.end.min(now).saturating_duration_since(p.start.max(since));
        let play = self.plays.iter().filter(|p| !overlap(p).is_zero()).max_by_key(|p| overlap(p))?;
        Some(name(play, fields))
    }
}

fn name(play: &Play, f: &GameTitles) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(player) = &play.other_player {
        parts.push(player.clone());
    }
    let percent = |p: Option<f64>| p.map(|p| format!(" at {:.0}%", (p * 100.0).clamp(0.0, 99.0))).unwrap_or_default();
    let stats = play.stats.as_ref();
    match play.outcome {
        Outcome::Passed => {
            // pp, then misses, then accuracy.
            if let Some(s) = stats {
                if f.osu_pp && s.pp > 0.0 {
                    parts.push(format!("{:.0}pp", s.pp));
                }
                if f.osu_accuracy {
                    if s.misses > 0 {
                        parts.push(if s.misses == 1 { "1 miss".into() } else { format!("{} misses", s.misses) });
                    }
                    let fc = s.misses == 0 && s.slider_breaks == 0;
                    parts.push(format!("{}{}", accuracy(s.accuracy), if fc { " FC" } else { "" }));
                }
            }
        }
        Outcome::Failed => parts.push(format!("Failed{}", percent(play.progress))),
        // Without tosu nothing tells a quit from a pass: say nothing.
        Outcome::Quit if play.progress.is_some() => parts.push(format!("Quit{}", percent(play.progress))),
        Outcome::Quit => {}
        Outcome::Playing => {
            if let Some(s) = stats
                && f.osu_accuracy
            {
                parts.push(accuracy(s.accuracy));
                parts.push(format!("{}x", s.combo));
            }
        }
    }
    if f.osu_mods
        && let Some(mods) = stats.map(|s| s.mods.trim()).filter(|m| !m.is_empty() && *m != "NM")
    {
        parts.push(format!("+{mods}"));
    }
    if f.osu_stars
        && let Some(stars) = play.map.stars.filter(|s| *s > 0.0)
    {
        parts.push(format!("{stars:.2}★"));
    }
    let m = &play.map;
    let map = match (f.osu_artist && !m.artist.is_empty(), m.version.is_empty()) {
        (true, false) => format!("{} - {} [{}]", m.artist, m.title, m.version),
        (true, true) => format!("{} - {}", m.artist, m.title),
        (false, false) => format!("{} [{}]", m.title, m.version),
        (false, true) => m.title.clone(),
    };
    parts.push(map);
    let full = parts.join(" · ");
    if full.chars().count() <= MAX_TITLE {
        return full;
    }
    let mut cut: String = full.chars().take(MAX_TITLE - 1).collect();
    cut.truncate(cut.trim_end().len());
    cut + "…"
}

/// "98.52%", "100%".
fn accuracy(a: f64) -> String {
    if a >= 99.995 { "100%".into() } else { format!("{a:.2}%") }
}

// ---------------------------------------------------------------------------
// Reading osu!
// ---------------------------------------------------------------------------

/// Reads osu! through tosu or gosumemory, else its window title.
pub struct Reader {
    agent: ureq::Agent,
    /// tosu's v2 answered last time (gosumemory only has `/json`).
    v2: bool,
}

impl Reader {
    pub fn new() -> Self {
        let agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(1))).build().into();
        Self { agent, v2: true }
    }

    pub fn read(&mut self) -> Option<Reading> {
        match self.get("/json/v2") {
            Some(Ok(v)) => {
                self.v2 = true;
                if let Some(r) = from_v2(&v) {
                    return Some(r);
                }
            }
            // Nothing listening: neither tosu nor gosumemory runs.
            None => return window_title_reading(),
            Some(Err(())) => self.v2 = false,
        }
        if !self.v2
            && let Some(Ok(v)) = self.get("/json")
            && let Some(r) = from_v1(&v)
        {
            return Some(r);
        }
        window_title_reading()
    }

    /// `None`: nothing answered; `Err`: it answered with something else.
    fn get(&self, path: &str) -> Option<Result<Value, ()>> {
        match self.agent.get(format!("{TOSU}{path}")).call() {
            Ok(mut r) => Some(r.body_mut().read_json::<Value>().map_err(|_| ())),
            Err(ureq::Error::StatusCode(_)) => Some(Err(())),
            Err(_) => None,
        }
    }
}

fn screen(state: Option<u64>) -> Screen {
    match state {
        Some(2) => Screen::Playing,
        Some(7) => Screen::Results,
        _ => Screen::Other,
    }
}

fn progress(now: Option<f64>, first: Option<f64>, last: Option<f64>) -> Option<f64> {
    let (now, first, last) = (now?, first?, last?);
    (last > first).then(|| ((now - first) / (last - first)).clamp(0.0, 1.0))
}

fn text(v: &Value) -> String {
    v.as_str().unwrap_or_default().trim().to_owned()
}

/// tosu's `/json/v2`.
fn from_v2(v: &Value) -> Option<Reading> {
    if v.get("error").is_some() || v.get("state").is_none() {
        return None;
    }
    let me = text(&v["profile"]["name"]);
    let someone_else = |name: &str| (!name.is_empty() && !me.is_empty() && !name.eq_ignore_ascii_case(&me)).then(|| name.to_owned());
    let b = &v["beatmap"];
    let map = Some(Map { artist: text(&b["artist"]), title: text(&b["title"]), version: text(&b["version"]), stars: b["stats"]["stars"]["total"].as_f64() })
        .filter(|m| !m.title.is_empty());
    let p = &v["play"];
    let stats = Stats {
        accuracy: p["accuracy"].as_f64().unwrap_or(0.0),
        misses: p["hits"]["0"].as_u64().unwrap_or(0),
        slider_breaks: p["hits"]["sliderBreaks"].as_u64().unwrap_or(0),
        combo: p["combo"]["current"].as_u64().unwrap_or(0),
        pp: p["pp"]["current"].as_f64().unwrap_or(0.0),
        mods: text(&p["mods"]["name"]),
    };
    let rs = &v["resultsScreen"];
    let results = Stats {
        accuracy: rs["accuracy"].as_f64().unwrap_or(0.0),
        misses: rs["hits"]["0"].as_u64().unwrap_or(0),
        slider_breaks: 0,
        combo: rs["maxCombo"].as_u64().unwrap_or(0),
        pp: rs["pp"]["current"].as_f64().unwrap_or(0.0),
        mods: text(&rs["mods"]["name"]),
    };
    let t = &b["time"];
    Some(Reading {
        screen: screen(v["state"]["number"].as_u64()),
        map,
        other_player: someone_else(&text(&p["playerName"])),
        stats: Some(stats),
        progress: progress(t["live"].as_f64(), t["firstObject"].as_f64(), t["lastObject"].as_f64()),
        failed: p["failed"].as_bool().unwrap_or(false),
        results: (results.accuracy > 0.0).then_some(results),
    })
}

/// gosumemory's `/json` (tosu serves it too).
fn from_v1(v: &Value) -> Option<Reading> {
    let menu = v.get("menu")?;
    let bm = &menu["bm"];
    let meta = &bm["metadata"];
    let map = Some(Map { artist: text(&meta["artist"]), title: text(&meta["title"]), version: text(&meta["difficulty"]), stars: bm["stats"]["fullSR"].as_f64() })
        .filter(|m| !m.title.is_empty());
    let g = &v["gameplay"];
    let stats = Stats {
        accuracy: g["accuracy"].as_f64().unwrap_or(0.0),
        misses: g["hits"]["0"].as_u64().unwrap_or(0),
        slider_breaks: g["hits"]["sliderBreaks"].as_u64().unwrap_or(0),
        combo: g["combo"]["current"].as_u64().unwrap_or(0),
        pp: g["pp"]["current"].as_f64().unwrap_or(0.0),
        mods: text(&menu["mods"]["str"]),
    };
    let t = &bm["time"];
    let progress = progress(t["current"].as_f64(), t["firstObj"].as_f64(), t["full"].as_f64());
    // No fail flag here: health at zero once the map has started.
    let failed = g["hp"]["normal"].as_f64().is_some_and(|hp| hp <= 0.0) && progress.is_some_and(|p| p > 0.01);
    let rs = &v["resultsScreen"];
    let results = rs["accuracy"].as_f64().filter(|a| *a > 0.0).map(|accuracy| Stats {
        accuracy,
        misses: rs["0"].as_u64().unwrap_or(0),
        slider_breaks: 0,
        combo: rs["maxCombo"].as_u64().unwrap_or(0),
        pp: 0.0,
        mods: text(&rs["mods"]["str"]),
    });
    Some(Reading { screen: screen(menu["state"].as_u64()), map, other_player: None, stats: Some(stats), progress, failed, results })
}

/// osu! stable, focused, shows the map in its title while playing:
/// "osu!  - Artist - Title [Version]".
fn window_title_reading() -> Option<Reading> {
    let (app, _) = capture::foreground_app_path()?;
    if !app.eq_ignore_ascii_case("osu!.exe") {
        return None;
    }
    let title = foreground_window_title()?;
    Some(Reading { screen: Screen::Other, map: None, other_player: None, stats: None, progress: None, failed: false, results: None })
        .map(|mut r| {
            if let Some(map) = map_from_title(&title) {
                r.screen = Screen::Playing;
                r.map = Some(map);
            }
            r
        })
}

fn map_from_title(title: &str) -> Option<Map> {
    let rest = title.strip_prefix("osu!")?.trim_start().strip_prefix('-')?.trim();
    // The editor shows the file instead.
    if rest.is_empty() || rest.ends_with(".osu") {
        return None;
    }
    let (artist, song) = rest.split_once(" - ").unwrap_or(("", rest));
    let (title, version) = match (song.rfind(" ["), song.ends_with(']')) {
        (Some(i), true) => (&song[..i], &song[i + 2..song.len() - 1]),
        _ => (song, ""),
    };
    Some(Map { artist: artist.trim().into(), title: title.trim().into(), version: version.into(), stars: None })
}

#[cfg(windows)]
fn foreground_window_title() -> Option<String> {
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowTextW};
    let mut buf = [0u16; 512];
    // SAFETY: the buffer is ours and its length is passed.
    let n = unsafe { GetWindowTextW(GetForegroundWindow(), &mut buf) };
    (n > 0).then(|| String::from_utf16_lossy(&buf[..n as usize]))
}

#[cfg(not(windows))]
fn foreground_window_title() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map() -> Map {
        Map { artist: "xi".into(), title: "FREEDOM DiVE".into(), version: "FOUR DIMENSIONS".into(), stars: Some(7.83) }
    }

    fn playing(progress: f64, accuracy: f64, misses: u64) -> Option<Reading> {
        Some(Reading {
            screen: Screen::Playing,
            map: Some(map()),
            other_player: None,
            stats: Some(Stats { accuracy, misses, slider_breaks: 0, combo: 500, pp: 300.0, mods: "HDDT".into() }),
            progress: Some(progress),
            failed: false,
            results: None,
        })
    }

    fn other() -> Option<Reading> {
        Some(Reading { screen: Screen::Other, map: None, other_player: None, stats: None, progress: None, failed: false, results: None })
    }

    fn fields() -> GameTitles {
        GameTitles::default()
    }

    /// The clip is saved from the song list after a pass: it's still named
    /// after the play, with the results screen's numbers.
    #[test]
    fn named_after_the_play_not_the_menu() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut t = Tracker::default();
        t.update(at(0), other());
        for s in 1..=40 {
            t.update(at(s), playing(s as f64 / 40.0, 98.0, 0));
        }
        let mut results = playing(1.0, 0.0, 0).unwrap();
        results.screen = Screen::Results;
        results.results = Some(Stats { accuracy: 98.52, misses: 0, slider_breaks: 0, combo: 2000, pp: 412.3, mods: "HDDT".into() });
        t.update(at(41), Some(results.clone()));
        t.update(at(45), Some(results));
        for s in 46..=55 {
            t.update(at(s), other());
        }
        let title = t.title(at(0), at(55), &fields());
        assert_eq!(title.as_deref(), Some("412pp · 98.52% FC · +HDDT · FREEDOM DiVE [FOUR DIMENSIONS]"));
        // Long after, nothing of it is in the clip.
        assert_eq!(t.title(at(100), at(130), &fields()), None);
    }

    #[test]
    fn retries_fails_and_quits() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut t = Tracker::default();
        // A short try, a retry (progress starts over), then quit at 40%.
        for s in 0..5 {
            t.update(at(s), playing(0.05 * s as f64, 90.0, 3));
        }
        for s in 5..=21 {
            t.update(at(s), playing(0.025 * (s - 5) as f64, 95.0, 1));
        }
        t.update(at(22), other());
        assert_eq!(t.title(at(0), at(25), &fields()).as_deref(), Some("Quit at 40% · +HDDT · FREEDOM DiVE [FOUR DIMENSIONS]"));
        // A fail.
        let mut failing = playing(0.64, 80.0, 30).unwrap();
        failing.failed = true;
        t.update(at(30), playing(0.6, 80.0, 30));
        t.update(at(40), Some(failing));
        t.update(at(42), other());
        assert_eq!(t.title(at(29), at(45), &fields()).as_deref(), Some("Failed at 64% · +HDDT · FREEDOM DiVE [FOUR DIMENSIONS]"));
    }

    #[test]
    fn mid_play_and_fields() {
        let t0 = Instant::now();
        let mut t = Tracker::default();
        t.update(t0, playing(0.1, 99.1, 0));
        t.update(t0 + Duration::from_secs(5), playing(0.2, 98.93, 0));
        let f = GameTitles { osu_mods: false, osu_stars: true, osu_artist: true, ..GameTitles::default() };
        assert_eq!(t.title(t0, t0 + Duration::from_secs(5), &f).as_deref(), Some("98.93% · 500x · 7.83★ · xi - FREEDOM DiVE [FOUR DIMENSIONS]"));
    }

    #[test]
    fn long_maps_are_cut_not_the_stats() {
        let mut m = map();
        m.title = "a".repeat(200);
        let play = Play {
            map: m,
            other_player: None,
            start: Instant::now(),
            end: Instant::now(),
            stats: Some(Stats { accuracy: 97.1, misses: 3, slider_breaks: 0, combo: 1, pp: 350.0, mods: String::new() }),
            progress: Some(1.0),
            failed: false,
            outcome: Outcome::Passed,
            on_results: false,
        };
        let title = name(&play, &fields());
        assert!(title.starts_with("350pp · 3 misses · 97.10% · aaa"), "{title}");
        assert_eq!(title.chars().count(), MAX_TITLE);
        assert!(title.ends_with('…'));
    }

    #[test]
    fn window_titles() {
        assert_eq!(map_from_title("osu!  - xi - FREEDOM DiVE [FOUR DIMENSIONS]"), Some(Map { stars: None, ..map() }));
        assert_eq!(map_from_title("osu!"), None);
        assert_eq!(map_from_title("osu!  - xi - FREEDOM DiVE [FOUR DIMENSIONS].osu"), None);
    }

    #[test]
    fn tosu_v2() {
        let v = serde_json::json!({
            "state": {"number": 2, "name": "play"},
            "profile": {"name": "Kaillr"},
            "beatmap": {"artist": "xi", "title": "FREEDOM DiVE", "version": "FOUR DIMENSIONS", "stats": {"stars": {"total": 7.83}},
                        "time": {"live": 50000.0, "firstObject": 0.0, "lastObject": 100000.0}},
            "play": {"playerName": "Kaillr", "failed": false, "accuracy": 98.5, "hits": {"0": 2, "sliderBreaks": 1},
                     "combo": {"current": 321, "max": 400}, "pp": {"current": 200.0}, "mods": {"name": "HD"}},
            "resultsScreen": {"accuracy": 0.0}
        });
        let r = from_v2(&v).unwrap();
        assert_eq!(r.screen, Screen::Playing);
        assert_eq!(r.other_player, None);
        assert_eq!(r.progress, Some(0.5));
        assert_eq!(r.stats.unwrap(), Stats { accuracy: 98.5, misses: 2, slider_breaks: 1, combo: 321, pp: 200.0, mods: "HD".into() });
        let mut replay = v.clone();
        replay["play"]["playerName"] = "WhiteCat".into();
        assert_eq!(from_v2(&replay).unwrap().other_player.as_deref(), Some("WhiteCat"));
    }
}
