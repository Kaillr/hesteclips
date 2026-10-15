//! What happened in the game, for a clip's name: "3 kills on Mirage",
//! "Pentakill as Jinx".
//!
//! Only games that tell us themselves, officially:
//!
//! - **Counter-Strike 2 and Dota 2**: Valve's Game State Integration. A small
//!   config file in the game's `cfg` folder makes the game post its state (map,
//!   the player's kills…) to a local web server here as it changes. Dota 2 only
//!   reads it when started with `-gamestateintegration`.
//! - **League of Legends**: its Live Client Data API, a local web server the
//!   game runs during a match, polled once a second.
//! - **osu!**: through tosu or gosumemory (`osu_plays`).
//!
//! Each kill becomes an event with the moment it happened; a clip's name sums
//! up the events inside it (osu!: the play on screen longest in it). A clip
//! with none keeps its usual name.
//!
//! **A clip is named after its best streak**, not everything in it: kills in
//! a row without dying (CS2: and in one round; Dota 2 and League: in one
//! fight, with no long gap between kills). A clip of a deathmatch or a long
//! recording would otherwise add up kills from several lives ("9 kills"),
//! which isn't what was clipped.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::settings::GameTitles;

/// Where CS2 and Dota 2 post their state. Fixed: it's written into their
/// config files. (Picked to be unlikely to clash; nothing special about it.)
const GSI_PORT: u16 = 52130;
/// Sent back by the games with each post, so stray local posts are ignored.
const GSI_TOKEN: &str = "hasteclip";
const CFG_NAME: &str = "gamestate_integration_hasteclip.cfg";
/// Events older than this are dropped (longer than any replay buffer; a
/// recording's summary only needs its kills, kept up to the cap).
const KEEP: Duration = Duration::from_secs(4 * 3600);
const MAX_EVENTS: usize = 5000;
/// An ace or multikill this close to a kill is that kill's (see `moments`).
const MERGE: Duration = Duration::from_millis(1500);
/// Dota 2 and League: kills further apart than this are separate fights (a
/// streak's kills are in one fight). Dota's own multikills allow 18 seconds
/// between kills; League's 10.
const FIGHT_GAP: Duration = Duration::from_secs(18);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Game {
    Cs2,
    Dota2,
    League,
}

#[derive(Debug, Clone, PartialEq)]
enum Kind {
    /// `streak`: which run of kills it's part of; it changes when the player
    /// dies, a new round starts (CS2) or a fight ends (Dota 2, League).
    Kill { headshot: bool, streak: u64 },
    /// CS2: all five of the other team in one round.
    Ace { streak: u64 },
    /// League: two to five champions in a row.
    Multikill { n: u32, streak: u64 },
}

impl Kind {
    fn streak(&self) -> u64 {
        match *self {
            Kind::Kill { streak, .. } | Kind::Ace { streak } | Kind::Multikill { streak, .. } => streak,
        }
    }
}

#[derive(Debug, Clone)]
struct Event {
    at: Instant,
    game: Game,
    kind: Kind,
    /// The map (CS2), hero (Dota 2) or champion (League) at the time.
    context: Option<String>,
}

#[derive(Default)]
struct State {
    events: VecDeque<Event>,
    cs2: Cs2Tracker,
    dota: DotaTracker,
    league: LeagueTracker,
    osu: crate::osu_plays::Tracker,
}

impl State {
    fn push(&mut self, at: Instant, game: Game, kind: Kind, context: Option<String>) {
        self.events.push_back(Event { at, game, kind, context });
        while self.events.len() > MAX_EVENTS || self.events.front().is_some_and(|e| at.saturating_duration_since(e.at) > KEEP) {
            self.events.pop_front();
        }
    }
}

/// The game events seen, from every game that reports them.
#[derive(Clone)]
pub struct Events {
    state: Arc<Mutex<State>>,
    /// Which games name clips (`None`: none).
    options: Arc<Mutex<Option<GameTitles>>>,
    /// Whether CS2's and Dota 2's config files were last put in (`None`: not
    /// looked at yet).
    installed: Arc<Mutex<Option<(bool, bool)>>>,
}

impl Events {
    pub fn new() -> Self {
        let events = Self { state: Default::default(), options: Default::default(), installed: Default::default() };
        let e = events.clone();
        std::thread::Builder::new().name("game state".into()).spawn(move || serve_gsi(&e)).expect("spawn game state thread");
        let e = events.clone();
        std::thread::Builder::new().name("league".into()).spawn(move || poll_league(&e)).expect("spawn league thread");
        let e = events.clone();
        std::thread::Builder::new().name("osu".into()).spawn(move || poll_osu(&e)).expect("spawn osu thread");
        events
    }

    /// Which games name clips (`None`: none); cheap when unchanged. Puts the
    /// config files in CS2's and Dota 2's folders, or takes them out again.
    pub fn set_options(&self, options: Option<GameTitles>) {
        let valve = options.as_ref().map_or((false, false), |o| (o.cs2, o.dota2));
        {
            let mut current = self.options.lock().unwrap();
            if *current != options {
                *current = options;
            }
        }
        let mut installed = self.installed.lock().unwrap();
        if *installed != Some(valve) {
            *installed = Some(valve);
            std::thread::spawn(move || {
                for (dir, game) in valve_cfg_dirs() {
                    let on = if game == Game::Cs2 { valve.0 } else { valve.1 };
                    if let Err(e) = set_cfg(&dir, game, on) {
                        eprintln!("game state config in {}: {e}", dir.display());
                    }
                }
            });
        }
    }

    /// Whether names come from this game.
    fn on(&self, game: impl Fn(&GameTitles) -> bool) -> bool {
        self.options.lock().unwrap().as_ref().is_some_and(game)
    }

    /// What happened since `since`, as a short name, if anything did.
    pub fn summary(&self, since: Instant) -> Option<String> {
        let options = self.options.lock().unwrap().clone()?;
        let s = self.state.lock().unwrap();
        if options.osu
            && let Some(title) = s.osu.title(since, Instant::now(), &options)
        {
            return Some(title);
        }
        let on = |g: Game| match g {
            Game::Cs2 => options.cs2,
            Game::Dota2 => options.dota2,
            Game::League => options.league,
        };
        let events: Vec<&Event> = s.events.iter().filter(|e| e.at >= since && on(e.game)).collect();
        summarize(&events)
    }
}

/// Read osu! twice a second while its names are wanted.
impl Events {
    /// The kills (and aces, multikills) from `since` to `until`, for a clip
    /// spanning that time: how long before `until` each was, and what to
    /// call it ("Headshot", "Ace", "Triple kill"). Only games whose
    /// details are turned on.
    pub fn moments(&self, since: Instant, until: Instant) -> Vec<(Duration, String)> {
        let Some(options) = self.options.lock().unwrap().clone() else { return Vec::new() };
        let on = |g: Game| match g {
            Game::Cs2 => options.cs2,
            Game::Dota2 => options.dota2,
            Game::League => options.league,
        };
        let s = self.state.lock().unwrap();
        let mut out: Vec<(Duration, String)> = Vec::new();
        for e in s.events.iter().filter(|e| e.at >= since && e.at <= until && on(e.game)) {
            let ago = until.saturating_duration_since(e.at);
            let label = match e.kind {
                Kind::Kill { headshot: true, .. } => "Headshot".to_owned(),
                Kind::Kill { .. } => "Kill".to_owned(),
                Kind::Ace { .. } => "Ace".to_owned(),
                Kind::Multikill { n, .. } => match n {
                    2..=5 => ["Double kill", "Triple kill", "Quadra kill", "Pentakill"][(n - 2) as usize].to_owned(),
                    _ => format!("{n} kills"),
                },
            };
            // An ace or multikill comes with the kill that made it: that
            // kill is called by it, not pinned twice.
            if matches!(e.kind, Kind::Ace { .. } | Kind::Multikill { .. })
                && let Some(last) = out.last_mut().filter(|(a, _)| a.abs_diff(ago) <= MERGE)
            {
                last.1 = label;
                continue;
            }
            out.push((ago, label));
        }
        out
    }
}

fn poll_osu(events: &Events) {
    let mut reader = crate::osu_plays::Reader::new();
    loop {
        std::thread::sleep(Duration::from_millis(500));
        if !events.on(|o| o.osu) {
            continue;
        }
        let reading = reader.read();
        events.state.lock().unwrap().osu.update(Instant::now(), reading);
    }
}

/// The name for a clip with these events: the game with the most of them.
fn summarize(events: &[&Event]) -> Option<String> {
    let game = [Game::Cs2, Game::Dota2, Game::League]
        .into_iter()
        .map(|g| (g, events.iter().filter(|e| e.game == g).count()))
        .filter(|(_, n)| *n > 0)
        .max_by_key(|(_, n)| *n)?
        .0;
    let mut events: Vec<&&Event> = events.iter().filter(|e| e.game == game).collect();
    // Named after the best streak in it: the most kills (then an ace or the
    // biggest multikill, then the most headshots), the latest of equals.
    // A clip spanning lives or rounds would otherwise add up kills that
    // never happened together ("9 kills" in a deathmatch).
    let mut streaks = std::collections::BTreeMap::<u64, (usize, bool, u32, usize)>::new();
    for e in &events {
        let s = streaks.entry(e.kind.streak()).or_default();
        match e.kind {
            Kind::Kill { headshot, .. } => {
                s.0 += 1;
                s.3 += usize::from(headshot);
            }
            Kind::Ace { .. } => s.1 = true,
            Kind::Multikill { n, .. } => s.2 = s.2.max(n),
        }
    }
    let best = streaks.iter().max_by_key(|(id, (kills, ace, multi, hs))| (*kills, *ace, *multi, *hs, **id)).map(|(id, _)| *id)?;
    events.retain(|e| e.kind.streak() == best);
    let kills = events.iter().filter(|e| matches!(e.kind, Kind::Kill { .. })).count();
    let headshots = events.iter().filter(|e| matches!(e.kind, Kind::Kill { headshot: true, .. })).count();
    let ace = events.iter().any(|e| matches!(e.kind, Kind::Ace { .. }));
    let context = events.iter().rev().find_map(|e| e.context.clone());
    let kills_text = |n: usize| if n == 1 { "1 kill".to_owned() } else { format!("{n} kills") };
    let what = match game {
        Game::Cs2 if ace => "Ace".to_owned(),
        Game::Cs2 if kills == 1 && headshots == 1 => "Headshot".to_owned(),
        Game::Cs2 if headshots > 0 => format!("{} ({headshots} {})", kills_text(kills), if headshots == 1 { "headshot" } else { "headshots" }),
        Game::League => match events.iter().filter_map(|e| if let Kind::Multikill { n, .. } = e.kind { Some(n) } else { None }).max() {
            Some(n) if n >= 2 => ["Double kill", "Triple kill", "Quadra kill", "Pentakill"][(n.min(5) - 2) as usize].to_owned(),
            _ => kills_text(kills),
        },
        _ => kills_text(kills),
    };
    if kills == 0 && !ace {
        return None;
    }
    let joiner = if game == Game::Cs2 { "on" } else { "as" };
    Some(match context {
        Some(c) => format!("{what} {joiner} {c}"),
        None => what,
    })
}

// ---------------------------------------------------------------------------
// Counter-Strike 2 and Dota 2: Game State Integration
// ---------------------------------------------------------------------------

/// The config file that makes a game post to us.
fn cfg_text(game: Game) -> String {
    let (path, data) = match game {
        Game::Cs2 => ("cs2", ["provider", "map", "player_id", "player_state", "player_match_stats"].as_slice()),
        _ => ("dota2", ["provider", "map", "player", "hero"].as_slice()),
    };
    let data: String = data.iter().map(|d| format!("        \"{d}\" \"1\"\n")).collect();
    format!(
        "\"HasteClip\"\n{{\n    \"uri\" \"http://127.0.0.1:{GSI_PORT}/{path}\"\n    \"timeout\" \"1.0\"\n    \"buffer\" \"0.1\"\n    \"throttle\" \"0.1\"\n    \"heartbeat\" \"30.0\"\n    \"auth\"\n    {{\n        \"token\" \"{GSI_TOKEN}\"\n    }}\n    \"data\"\n    {{\n{data}    }}\n}}\n"
    )
}

/// Write (`on`) or remove our config file in a game's config folder.
fn set_cfg(dir: &Path, game: Game, on: bool) -> std::io::Result<()> {
    let file = dir.join(CFG_NAME);
    if !on {
        return match std::fs::remove_file(&file) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    let text = cfg_text(game);
    if std::fs::read_to_string(&file).is_ok_and(|t| t == text) {
        return Ok(());
    }
    std::fs::create_dir_all(dir)?;
    std::fs::write(file, text)
}

/// The config folders of CS2 and Dota 2, wherever Steam installed them.
fn valve_cfg_dirs() -> Vec<(PathBuf, Game)> {
    let mut dirs = Vec::new();
    for lib in steam_libraries() {
        let common = lib.join("steamapps").join("common");
        let cs = common.join("Counter-Strike Global Offensive").join("game").join("csgo").join("cfg");
        if cs.is_dir() {
            dirs.push((cs, Game::Cs2));
        }
        let dota = common.join("dota 2 beta").join("game").join("dota").join("cfg");
        if dota.is_dir() {
            dirs.push((dota.join("gamestate_integration"), Game::Dota2));
        }
    }
    dirs
}

/// Steam's library folders: Steam's own, and those in its `libraryfolders.vdf`.
fn steam_libraries() -> Vec<PathBuf> {
    let Some(steam) = steam_dir() else { return Vec::new() };
    let mut libs = vec![steam.clone()];
    if let Ok(text) = std::fs::read_to_string(steam.join("steamapps").join("libraryfolders.vdf")) {
        libs.extend(vdf_paths(&text));
    }
    let mut seen = Vec::new();
    libs.retain(|l| {
        let key = l.to_string_lossy().replace('\\', "/").to_lowercase();
        let new = !seen.contains(&key);
        seen.push(key);
        new
    });
    libs
}

/// The `"path"` values of a `libraryfolders.vdf` (backslashes are escaped).
fn vdf_paths(text: &str) -> Vec<PathBuf> {
    text.lines()
        .filter_map(|line| {
            let quoted: Vec<&str> = line.split('"').skip(1).step_by(2).collect();
            match quoted[..] {
                [key, value] if key.eq_ignore_ascii_case("path") => Some(PathBuf::from(value.replace("\\\\", "\\"))),
                _ => None,
            }
        })
        .collect()
}

fn steam_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_SZ, RegGetValueW};
        use windows::core::{HSTRING, PCWSTR};
        let mut buf = [0u16; 520];
        let mut size = (buf.len() * 2) as u32;
        let (key, name) = (HSTRING::from(r"Software\Valve\Steam"), HSTRING::from("SteamPath"));
        // SAFETY: the buffer and its size in bytes match.
        let found = unsafe { RegGetValueW(HKEY_CURRENT_USER, PCWSTR(key.as_ptr()), PCWSTR(name.as_ptr()), RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr().cast()), Some(&mut size)) }.is_ok();
        let dir = found.then(|| {
            let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            PathBuf::from(String::from_utf16_lossy(&buf[..len]))
        });
        dir.or_else(|| Some(PathBuf::from(r"C:\Program Files (x86)\Steam"))).filter(|d| d.is_dir())
    }
    #[cfg(target_os = "macos")]
    {
        dirs::data_dir().map(|d| d.join("Steam")).filter(|d| d.is_dir())
    }
    #[cfg(target_os = "linux")]
    {
        let home = dirs::home_dir()?;
        [home.join(".steam").join("steam"), home.join(".local").join("share").join("Steam")].into_iter().find(|d| d.is_dir())
    }
}

/// The local web server the games post to.
fn serve_gsi(events: &Events) {
    let listener = match TcpListener::bind(("127.0.0.1", GSI_PORT)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("game state: can't listen on port {GSI_PORT}: {e}");
            return;
        }
    };
    for stream in listener.incoming().flatten() {
        let events = events.clone();
        std::thread::spawn(move || {
            let _ = serve_connection(stream, &events);
        });
    }
}

/// Answer posts on one connection (the games keep it open) until it closes.
fn serve_connection(stream: TcpStream, events: &Events) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(120)))?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    loop {
        let mut request = String::new();
        if reader.read_line(&mut request)? == 0 {
            return Ok(());
        }
        let mut length = 0usize;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line)? == 0 {
                return Ok(());
            }
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.trim().eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0u8; length.min(4 << 20)];
        reader.read_exact(&mut body)?;
        record_post(events, &request, &body);
        writer.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 0\r\n\r\n")?;
    }
}

/// Note the kills in one posted game state (`request`: its request line).
fn record_post(events: &Events, request: &str, body: &[u8]) {
    let cs2 = request.split_whitespace().nth(1).unwrap_or("").starts_with("/cs2");
    if !events.on(|o| if cs2 { o.cs2 } else { o.dota2 }) {
        return;
    }
    let Ok(state) = serde_json::from_slice::<Value>(body) else { return };
    if state["auth"]["token"].as_str() != Some(GSI_TOKEN) {
        return;
    }
    let mut s = events.state.lock().unwrap();
    let s = &mut *s;
    let now = Instant::now();
    let (found, game) = if cs2 { (s.cs2.update(&state), Game::Cs2) } else { (s.dota.update(&state), Game::Dota2) };
    for (kind, context) in found {
        s.push(now, game, kind, context);
    }
}

/// Turns CS2's posted states into kills, by how the player's counts change.
#[derive(Default)]
struct Cs2Tracker {
    map: Option<String>,
    kills: Option<u64>,
    deaths: Option<u64>,
    round: Option<u64>,
    round_kills: u64,
    round_hs: u64,
    /// The current streak: it changes when the player dies or a round
    /// starts (in a deathmatch, the round never ends: dying ends a streak).
    streak: u64,
    /// The player was dead at the last post about them.
    dead: bool,
    /// A death already counted from no health, whose rise in the death count
    /// is still to come (it isn't another death).
    death_seen: bool,
}

impl Cs2Tracker {
    fn update(&mut self, state: &Value) -> Vec<(Kind, Option<String>)> {
        // Only the player's own game: while dead or spectating, `player` is
        // whoever is being watched.
        let me = state["provider"]["steamid"].as_str();
        let player = &state["player"];
        if me.is_none() || player["steamid"].as_str() != me {
            return Vec::new();
        }
        let map = state["map"]["name"].as_str().map(map_name);
        if map != self.map {
            let streak = self.streak + 1;
            *self = Self { map: map.clone(), streak, ..Self::default() };
        }
        let (Some(kills), Some(round_kills), Some(round_hs)) = (
            player["match_stats"]["kills"].as_u64(),
            player["state"]["round_kills"].as_u64(),
            player["state"]["round_killhs"].as_u64(),
        ) else {
            return Vec::new();
        };
        let round = state["map"]["round"].as_u64().unwrap_or(0);
        let deaths = player["match_stats"]["deaths"].as_u64();
        let dead = player["state"]["health"].as_u64() == Some(0);
        // A streak ends with a death or a new round. A death shows as no
        // health, or only as the death count going up next time the post is
        // about the player (while dead, posts are mostly about whoever is
        // watched); seen as both, it's one death.
        let counted = matches!((self.deaths, deaths), (Some(a), Some(b)) if b > a);
        let fell = dead && !self.dead;
        let died = fell || (counted && !self.death_seen);
        if fell {
            self.death_seen = !counted;
        } else if counted {
            self.death_seen = false;
        }
        let new_round = self.round.is_some_and(|r| r != round);
        // The round's counts start over each round (and with each life in a
        // deathmatch): what's beyond them counts from zero.
        let fresh = new_round || round_kills < self.round_kills;
        let new_kills = self.kills.map_or(0, |k| kills.saturating_sub(k));
        let new_hs = if fresh { round_hs } else { round_hs.saturating_sub(self.round_hs) };
        let mut found = Vec::new();
        let warmup = state["map"]["phase"].as_str() == Some("warmup");
        // A new round: its kills are a new streak.
        if new_round {
            self.streak += 1;
        }
        if !warmup {
            for k in 0..new_kills {
                found.push((Kind::Kill { headshot: k < new_hs, streak: self.streak }, self.map.clone()));
            }
            // An ace: the other team's five, in one round. Not in a deathmatch
            // (no team, and five a life is common) nor Wingman (two of them).
            let mode = state["map"]["mode"].as_str().unwrap_or("");
            let team_of_five = !matches!(mode, "deathmatch" | "scrimcomp2v2" | "wingman" | "gungameprogressive" | "gungametrbomb" | "armsrace");
            if team_of_five && self.kills.is_some() && round_kills >= 5 && (self.round_kills < 5 || fresh) {
                found.push((Kind::Ace { streak: self.streak }, self.map.clone()));
            }
        }
        // Died: what follows is a new streak (kills seen in the same post were
        // made before it).
        if died {
            self.streak += 1;
        }
        self.kills = Some(kills);
        self.deaths = deaths.or(self.deaths);
        self.round = Some(round);
        self.round_kills = round_kills;
        self.round_hs = round_hs;
        self.dead = dead;
        found
    }
}

/// "de_dust2" → "Dust II", "cs_office" → "Office".
fn map_name(id: &str) -> String {
    let base = id.rsplit('/').next().unwrap_or(id);
    let base = match base.split_once('_') {
        Some((prefix, rest)) if prefix.len() <= 3 => rest,
        _ => base,
    };
    match base {
        "dust2" => "Dust II".to_owned(),
        _ => title_case(base),
    }
}

/// Turns Dota 2's posted states into kills.
#[derive(Default)]
struct DotaTracker {
    match_id: Option<String>,
    kills: Option<u64>,
    deaths: Option<u64>,
    /// The current streak (one fight): it changes when the player dies or
    /// kills nothing for [`FIGHT_GAP`].
    streak: u64,
    last_kill: Option<Instant>,
}

impl DotaTracker {
    fn update(&mut self, state: &Value) -> Vec<(Kind, Option<String>)> {
        self.update_at(state, Instant::now())
    }

    fn update_at(&mut self, state: &Value, now: Instant) -> Vec<(Kind, Option<String>)> {
        let match_id = state["map"]["matchid"].as_str().map(str::to_owned);
        if match_id != self.match_id {
            let streak = self.streak + 1;
            *self = Self { match_id, streak, ..Self::default() };
        }
        // Spectating: `player` holds teams rather than one player.
        let Some(kills) = state["player"]["kills"].as_u64() else { return Vec::new() };
        let deaths = state["player"]["deaths"].as_u64();
        let hero = state["hero"]["name"].as_str().map(hero_name);
        let new = self.kills.map_or(0, |k| kills.saturating_sub(k));
        if new > 0 && self.last_kill.is_some_and(|t| now.saturating_duration_since(t) > FIGHT_GAP) {
            self.streak += 1;
        }
        let found = (0..new).map(|_| (Kind::Kill { headshot: false, streak: self.streak }, hero.clone())).collect();
        if new > 0 {
            self.last_kill = Some(now);
        }
        // Died: the next kills are another fight (kills in the same post came first).
        if matches!((self.deaths, deaths), (Some(a), Some(b)) if b > a) {
            self.streak += 1;
            self.last_kill = None;
        }
        self.kills = Some(kills);
        self.deaths = deaths.or(self.deaths);
        found
    }
}

/// "npc_dota_hero_queenofpain" → "Queen of Pain".
fn hero_name(id: &str) -> String {
    let base = id.strip_prefix("npc_dota_hero_").unwrap_or(id);
    // Heroes whose internal names aren't their names.
    let named = [
        ("antimage", "Anti-Mage"),
        ("nevermore", "Shadow Fiend"),
        ("zuus", "Zeus"),
        ("obsidian_destroyer", "Outworld Destroyer"),
        ("furion", "Nature's Prophet"),
        ("wisp", "Io"),
        ("magnataur", "Magnus"),
        ("windrunner", "Windranger"),
        ("rattletrap", "Clockwerk"),
        ("shredder", "Timbersaw"),
        ("skeleton_king", "Wraith King"),
        ("life_stealer", "Lifestealer"),
        ("doom_bringer", "Doom"),
        ("necrolyte", "Necrophos"),
        ("queenofpain", "Queen of Pain"),
        ("treant", "Treant Protector"),
        ("centaur", "Centaur Warrunner"),
        ("abyssal_underlord", "Underlord"),
        ("vengefulspirit", "Vengeful Spirit"),
        ("drow_ranger", "Drow Ranger"),
        ("keeper_of_the_light", "Keeper of the Light"),
        ("spirit_breaker", "Spirit Breaker"),
        ("night_stalker", "Night Stalker"),
        ("tiny", "Tiny"),
    ];
    named.iter().find(|(k, _)| *k == base).map_or_else(|| title_case(base), |(_, v)| (*v).to_owned())
}

/// "faceless_void" → "Faceless Void".
fn title_case(id: &str) -> String {
    id.split('_')
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut c = w.chars();
            c.next().map(|f| f.to_uppercase().chain(c).collect::<String>()).unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------------------
// League of Legends: Live Client Data API
// ---------------------------------------------------------------------------

/// Poll the game's local API while a match runs.
fn poll_league(events: &Events) {
    // The game serves it over HTTPS with its own certificate, on this
    // computer only.
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .tls_config(ureq::tls::TlsConfig::builder().disable_verification(true).build())
        .timeout_global(Some(Duration::from_secs(2)))
        .build()
        .into();
    loop {
        if !events.on(|o| o.league) {
            std::thread::sleep(Duration::from_secs(5));
            continue;
        }
        let data = agent
            .get("https://127.0.0.1:2999/liveclientdata/allgamedata")
            .call()
            .ok()
            .and_then(|mut r| r.body_mut().read_json::<Value>().ok());
        let Some(data) = data else {
            // No match running.
            events.state.lock().unwrap().league = LeagueTracker::default();
            std::thread::sleep(Duration::from_secs(3));
            continue;
        };
        let now = Instant::now();
        let mut s = events.state.lock().unwrap();
        for (ago, kind, champion) in s.league.update(&data) {
            s.push(now.checked_sub(ago).unwrap_or(now), Game::League, kind, champion);
        }
        drop(s);
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// Turns League's event list into the player's kills.
#[derive(Default)]
struct LeagueTracker {
    /// The last event already looked at (`None`: none yet this match).
    last_id: Option<i64>,
    /// The current streak (one fight): it changes when the player dies or
    /// kills nothing for [`FIGHT_GAP`] (game time).
    streak: u64,
    last_kill: Option<f64>,
}

impl LeagueTracker {
    /// New kills: how long ago each happened, what, and the player's champion.
    fn update(&mut self, data: &Value) -> Vec<(Duration, Kind, Option<String>)> {
        let me = &data["activePlayer"];
        // Event names use the Riot ID's name part (older games: summoner name).
        let names: Vec<&str> = ["riotIdGameName", "summonerName", "riotId"].iter().filter_map(|k| me[*k].as_str()).filter(|n| !n.is_empty()).collect();
        if names.is_empty() {
            return Vec::new();
        }
        let is_me = |n: &Value| n.as_str().is_some_and(|n| names.iter().any(|m| m.eq_ignore_ascii_case(n)));
        let champion = data["allPlayers"].as_array().and_then(|players| {
            players.iter().find(|p| ["riotIdGameName", "summonerName", "riotId"].iter().any(|k| is_me(&p[*k])))?["championName"].as_str().map(str::to_owned)
        });
        let game_time = data["gameData"]["gameTime"].as_f64().unwrap_or(0.0);
        let list = data["events"]["Events"].as_array().cloned().unwrap_or_default();
        let mut found = Vec::new();
        for e in &list {
            let Some(id) = e["EventID"].as_i64() else { continue };
            if self.last_id.is_some_and(|last| id <= last) {
                continue;
            }
            self.last_id = Some(id);
            let at = e["EventTime"].as_f64().unwrap_or(game_time);
            let ago = Duration::from_secs_f64((game_time - at).max(0.0));
            let kind = match e["EventName"].as_str() {
                Some("ChampionKill") if is_me(&e["VictimName"]) => {
                    // Died: the next kills are another fight.
                    self.streak += 1;
                    self.last_kill = None;
                    continue;
                }
                Some("ChampionKill") if is_me(&e["KillerName"]) => {
                    if self.last_kill.is_some_and(|t| at - t > FIGHT_GAP.as_secs_f64()) {
                        self.streak += 1;
                    }
                    self.last_kill = Some(at);
                    Kind::Kill { headshot: false, streak: self.streak }
                }
                Some("Multikill") if is_me(&e["KillerName"]) => Kind::Multikill { n: e["KillStreak"].as_u64().unwrap_or(0) as u32, streak: self.streak },
                _ => continue,
            };
            found.push((ago, kind, champion.clone()));
        }
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cs_state(kills: u64, round_kills: u64, hs: u64) -> Value {
        cs_state_in(0, kills, round_kills, hs)
    }

    fn cs_state_in(round: u64, kills: u64, round_kills: u64, hs: u64) -> Value {
        cs_full("competitive", "live", round, kills, 0, round_kills, hs, 100)
    }

    /// A CS2 post about the player: the mode and phase, the round, their
    /// kills and deaths in the match, kills and headshots this round (or
    /// this life, in a deathmatch) and health.
    #[allow(clippy::too_many_arguments)]
    fn cs_full(mode: &str, phase: &str, round: u64, kills: u64, deaths: u64, round_kills: u64, hs: u64, health: u64) -> Value {
        json!({
            "provider": {"steamid": "1"},
            "map": {"name": "de_mirage", "mode": mode, "phase": phase, "round": round},
            "player": {"steamid": "1", "state": {"health": health, "round_kills": round_kills, "round_killhs": hs}, "match_stats": {"kills": kills, "deaths": deaths}},
            "auth": {"token": GSI_TOKEN}
        })
    }

    fn summary_of(found: Vec<(Kind, Option<String>)>, game: Game) -> Option<String> {
        let now = Instant::now();
        let events: Vec<Event> = found.into_iter().map(|(kind, context)| Event { at: now, game, kind, context }).collect();
        summarize(&events.iter().collect::<Vec<_>>())
    }

    #[test]
    fn cs2_kills_and_headshots() {
        let mut t = Cs2Tracker::default();
        assert!(t.update(&cs_state(4, 0, 0)).is_empty(), "the first state is the baseline");
        let mut found = t.update(&cs_state(5, 1, 1));
        found.extend(t.update(&cs_state(7, 3, 1)));
        assert_eq!(found.len(), 3);
        assert_eq!(summary_of(found, Game::Cs2).as_deref(), Some("3 kills (1 headshot) on Mirage"));
        // Spectating someone else: their kills aren't ours.
        let mut other = cs_state(20, 2, 0);
        other["player"]["steamid"] = json!("2");
        assert!(t.update(&other).is_empty());
    }

    #[test]
    fn cs2_names_the_best_round() {
        let mut t = Cs2Tracker::default();
        t.update(&cs_state_in(3, 10, 0, 0));
        // Round 3: 4 kills, 4 headshots; round 4: 2 kills, 1 headshot.
        let mut found = t.update(&cs_state_in(3, 14, 4, 4));
        found.extend(t.update(&cs_state_in(4, 15, 1, 0)));
        found.extend(t.update(&cs_state_in(4, 16, 2, 1)));
        assert_eq!(found.len(), 6);
        assert_eq!(summary_of(found, Game::Cs2).as_deref(), Some("4 kills (4 headshots) on Mirage"));
    }

    #[test]
    fn cs2_ace() {
        let mut t = Cs2Tracker::default();
        t.update(&cs_state(0, 0, 0));
        let found: Vec<_> = (1..=5).flat_map(|k| t.update(&cs_state(k, k, 0))).collect();
        assert_eq!(summary_of(found, Game::Cs2).as_deref(), Some("Ace on Mirage"));
    }

    /// A deathmatch is one long round: a clip spanning several lives is
    /// named after its best life, not all of its kills together, and five
    /// kills in a life isn't an ace.
    #[test]
    fn cs2_deathmatch_names_the_best_life() {
        let mut t = Cs2Tracker::default();
        let dm = |kills, deaths, rk, hs, health| cs_full("deathmatch", "live", 0, kills, deaths, rk, hs, health);
        let mut found = t.update(&dm(10, 3, 0, 0, 100));
        // A life: 3 kills (1 headshot), then dead (the post says so: no health).
        found.extend(t.update(&dm(11, 3, 1, 1, 100)));
        found.extend(t.update(&dm(13, 3, 3, 1, 100)));
        found.extend(t.update(&dm(13, 3, 3, 1, 0)));
        // While dead, posts are about whoever's watched: ignored.
        let mut other = dm(40, 9, 2, 0, 100);
        other["player"]["steamid"] = json!("2");
        found.extend(t.update(&other));
        // Respawned (the death counted now): 5 kills, 2 headshots, then a
        // death seen only in the count.
        for (k, hs) in [(1, 0), (2, 1), (3, 1), (4, 2), (5, 2)] {
            found.extend(t.update(&dm(13 + k, 4, k, hs, 100)));
        }
        found.extend(t.update(&dm(18, 5, 0, 0, 100)));
        // Another life: 2 kills.
        found.extend(t.update(&dm(19, 5, 1, 0, 100)));
        found.extend(t.update(&dm(20, 5, 2, 0, 100)));
        assert_eq!(found.iter().filter(|(k, _)| matches!(k, Kind::Kill { .. })).count(), 10);
        assert!(!found.iter().any(|(k, _)| matches!(k, Kind::Ace { .. })), "no ace in a deathmatch");
        assert_eq!(summary_of(found, Game::Cs2).as_deref(), Some("5 kills (2 headshots) on Mirage"));
    }

    /// Kills seen in the same post as the death were made before it: they
    /// stay with that life.
    #[test]
    fn cs2_kills_before_a_death_stay_in_that_life() {
        let mut t = Cs2Tracker::default();
        let dm = |kills, deaths, rk, health| cs_full("deathmatch", "live", 0, kills, deaths, rk, 0, health);
        let mut found = t.update(&dm(0, 0, 0, 100));
        found.extend(t.update(&dm(1, 0, 1, 100)));
        // Traded: the second kill and the death in one post.
        found.extend(t.update(&dm(2, 1, 2, 0)));
        found.extend(t.update(&dm(3, 1, 1, 100)));
        assert_eq!(summary_of(found, Game::Cs2).as_deref(), Some("2 kills on Mirage"));
    }

    /// Competitive: a round ends a streak even without dying, and an ace is
    /// still an ace; warmup kills don't count.
    #[test]
    fn cs2_rounds_aces_and_warmup() {
        let mut t = Cs2Tracker::default();
        let mut found = t.update(&cs_full("competitive", "warmup", 0, 0, 0, 0, 0, 100));
        found.extend(t.update(&cs_full("competitive", "warmup", 0, 6, 0, 6, 0, 100)));
        assert!(found.is_empty(), "warmup: {found:?}");
        // The match starts: counts start over.
        found.extend(t.update(&cs_full("competitive", "live", 1, 0, 0, 0, 0, 100)));
        for k in 1..=5 {
            found.extend(t.update(&cs_full("competitive", "live", 1, k, 0, k, 0, 100)));
        }
        // The next round, alive: 2 kills, a separate streak.
        found.extend(t.update(&cs_full("competitive", "live", 2, 6, 0, 1, 0, 100)));
        found.extend(t.update(&cs_full("competitive", "live", 2, 7, 0, 2, 0, 100)));
        assert_eq!(found.iter().filter(|(k, _)| matches!(k, Kind::Ace { .. })).count(), 1);
        assert_eq!(summary_of(found, Game::Cs2).as_deref(), Some("Ace on Mirage"));
    }

    /// Equal streaks: the latest is named (it's likely what was clipped:
    /// it's what just happened when the clip was saved).
    #[test]
    fn the_latest_of_equal_streaks() {
        let mut t = Cs2Tracker::default();
        let dm = |kills, deaths, rk, hs| cs_full("deathmatch", "live", 0, kills, deaths, rk, hs, 100);
        let mut found = t.update(&dm(0, 0, 0, 0));
        found.extend(t.update(&dm(2, 0, 2, 0)));
        found.extend(t.update(&dm(2, 1, 0, 0)));
        found.extend(t.update(&dm(4, 1, 2, 2)));
        assert_eq!(summary_of(found, Game::Cs2).as_deref(), Some("2 kills (2 headshots) on Mirage"));
    }

    /// Dota 2: a fight is kills without a long gap and without dying.
    #[test]
    fn dota_names_the_best_fight() {
        let mut t = DotaTracker::default();
        let state = |k: u64, d: u64| json!({"map": {"matchid": "9"}, "player": {"kills": k, "deaths": d}, "hero": {"name": "npc_dota_hero_nevermore"}});
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut found = t.update_at(&state(0, 0), at(0));
        // A fight: 2 kills 5 s apart. A minute later, another: 3 kills.
        found.extend(t.update_at(&state(1, 0), at(10)));
        found.extend(t.update_at(&state(2, 0), at(15)));
        found.extend(t.update_at(&state(3, 0), at(80)));
        found.extend(t.update_at(&state(4, 0), at(84)));
        found.extend(t.update_at(&state(5, 0), at(90)));
        // Died, then one more right after respawning.
        found.extend(t.update_at(&state(5, 1), at(95)));
        found.extend(t.update_at(&state(6, 1), at(99)));
        assert_eq!(summary_of(found, Game::Dota2).as_deref(), Some("3 kills as Shadow Fiend"));
    }

    /// League: dying ends a fight's streak.
    #[test]
    fn league_death_ends_a_streak() {
        let mut t = LeagueTracker::default();
        let data = json!({
            "activePlayer": {"riotIdGameName": "Me"},
            "allPlayers": [{"riotIdGameName": "Me", "championName": "Jinx"}],
            "gameData": {"gameTime": 100.0},
            "events": {"Events": [
                {"EventID": 1, "EventName": "ChampionKill", "EventTime": 60.0, "KillerName": "Me", "VictimName": "A"},
                {"EventID": 2, "EventName": "ChampionKill", "EventTime": 64.0, "KillerName": "Me", "VictimName": "B"},
                {"EventID": 3, "EventName": "ChampionKill", "EventTime": 66.0, "KillerName": "C", "VictimName": "Me"},
                {"EventID": 4, "EventName": "ChampionKill", "EventTime": 70.0, "KillerName": "Me", "VictimName": "C"}
            ]}
        });
        let found = t.update(&data).into_iter().map(|(_, k, c)| (k, c)).collect();
        assert_eq!(summary_of(found, Game::League).as_deref(), Some("2 kills as Jinx"));
    }

    #[test]
    fn dota_kills() {
        let mut t = DotaTracker::default();
        let state = |k: u64| json!({"map": {"matchid": "9"}, "player": {"kills": k}, "hero": {"name": "npc_dota_hero_nevermore"}});
        t.update(&state(1));
        let found = t.update(&state(3));
        assert_eq!(summary_of(found, Game::Dota2).as_deref(), Some("2 kills as Shadow Fiend"));
    }

    #[test]
    fn league_multikill() {
        let mut t = LeagueTracker::default();
        let data = json!({
            "activePlayer": {"riotId": "Me#EUW", "riotIdGameName": "Me"},
            "allPlayers": [{"riotIdGameName": "Me", "championName": "Jinx"}, {"riotIdGameName": "You", "championName": "Ashe"}],
            "gameData": {"gameTime": 100.0},
            "events": {"Events": [
                {"EventID": 0, "EventName": "GameStart", "EventTime": 0.0},
                {"EventID": 1, "EventName": "ChampionKill", "EventTime": 90.0, "KillerName": "Me", "VictimName": "You"},
                {"EventID": 2, "EventName": "ChampionKill", "EventTime": 92.0, "KillerName": "Me", "VictimName": "Them"},
                {"EventID": 3, "EventName": "Multikill", "EventTime": 92.0, "KillerName": "Me", "KillStreak": 2},
                {"EventID": 4, "EventName": "ChampionKill", "EventTime": 95.0, "KillerName": "You", "VictimName": "Me"}
            ]}
        });
        let found = t.update(&data);
        assert_eq!(found.len(), 3);
        assert_eq!(found[0].0, Duration::from_secs(10));
        assert!(t.update(&data).is_empty(), "events are only counted once");
        let found = found.into_iter().map(|(_, k, c)| (k, c)).collect();
        assert_eq!(summary_of(found, Game::League).as_deref(), Some("Double kill as Jinx"));
    }

    #[test]
    fn moments_pin_each_kill_once() {
        let titles = GameTitles { league: true, ..GameTitles::default() };
        let events = Events { state: Default::default(), options: Arc::new(Mutex::new(Some(titles))), installed: Default::default() };
        let now = Instant::now();
        {
            let mut s = events.state.lock().unwrap();
            let kill = Kind::Kill { headshot: false, streak: 0 };
            s.push(now - Duration::from_secs(20), Game::League, kill.clone(), None);
            s.push(now - Duration::from_secs(8), Game::League, kill, None);
            // The multikill comes with the second kill: it names it.
            s.push(now - Duration::from_secs(8), Game::League, Kind::Multikill { n: 2, streak: 0 }, None);
            // Before the clip.
            s.push(now - Duration::from_secs(90), Game::League, Kind::Kill { headshot: false, streak: 0 }, None);
        }
        let m = events.moments(now - Duration::from_secs(60), now);
        assert_eq!(m, vec![(Duration::from_secs(20), "Kill".to_owned()), (Duration::from_secs(8), "Double kill".to_owned())]);
    }

    #[test]
    fn names() {
        assert_eq!(map_name("de_dust2"), "Dust II");
        assert_eq!(map_name("cs_office"), "Office");
        assert_eq!(map_name("workshop/123/de_cache"), "Cache");
        assert_eq!(hero_name("npc_dota_hero_faceless_void"), "Faceless Void");
        assert_eq!(hero_name("npc_dota_hero_keeper_of_the_light"), "Keeper of the Light");
        assert_eq!(
            vdf_paths("\"libraryfolders\"\n{\n\t\"0\"\n\t{\n\t\t\"path\"\t\t\"C:\\\\Program Files (x86)\\\\Steam\"\n"),
            vec![PathBuf::from(r"C:\Program Files (x86)\Steam")]
        );
    }

    /// The server takes a post the way CS2 sends it and records the kill.
    #[test]
    fn gsi_post() {
        let events = Events { state: Default::default(), options: Arc::new(Mutex::new(Some(GameTitles::default()))), installed: Default::default() };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let e = events.clone();
        std::thread::spawn(move || serve_connection(listener.accept().unwrap().0, &e));
        let mut client = TcpStream::connect(addr).unwrap();
        for state in [cs_state(0, 0, 0), cs_state(1, 1, 1)] {
            let body = state.to_string();
            write!(client, "POST /cs2 HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
            let mut reply = [0u8; 64];
            let n = client.read(&mut reply).unwrap();
            assert!(reply[..n].starts_with(b"HTTP/1.1 200"));
        }
        let since = Instant::now() - Duration::from_secs(5);
        assert_eq!(events.summary(since).as_deref(), Some("Headshot on Mirage"));
    }
}
