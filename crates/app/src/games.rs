//! Which game is being clipped, for the folder a clip goes in.
//!
//! The app in focus is a game when Discord's list of games knows its
//! executable (it has the best names and icons), when it's installed where game
//! stores put games (`…/steamapps/common/<game>/`, Epic, GOG, Xbox — works
//! offline, and for games Discord doesn't list), or when it's on the games
//! and apps list. Anything else in focus (Explorer, a browser, Discord) is the
//! desktop.
//!
//! While capture runs, a thread notes once a second what's in focus — on its
//! own, as the app may draw no frames while a game covers it. A clip is of
//! what was in focus longest in it: a replay clip's window, a recording's
//! whole length. So clipping a browser with a game open in the background
//! files the clip under Desktop, not the game.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::discord::Game;

/// What's being captured, as the focus history needs it.
#[derive(Debug, Clone, PartialEq)]
pub enum Capturing {
    Off,
    /// The replay buffer: clips are of the last this many seconds.
    Buffer(u32),
    Record,
}

#[derive(Default)]
struct State {
    capturing: Option<Capturing>,
    /// The games and apps list: (id, name).
    apps: Vec<(String, String)>,
    /// The replay buffer: what was in focus each second, newest last (`None`:
    /// the desktop, or any app that isn't a game).
    recent: VecDeque<Option<String>>,
    /// A recording: seconds in focus, per game (`None`: the desktop).
    totals: HashMap<Option<String>, u32>,
    /// Games by where their app is installed, so the stores' files are read
    /// once per app.
    by_path: HashMap<PathBuf, Option<String>>,
}

/// Notes what's in focus while capture runs.
#[derive(Clone)]
pub struct Tracker {
    state: Arc<Mutex<State>>,
}

impl Tracker {
    pub fn new() -> Self {
        let state: Arc<Mutex<State>> = Default::default();
        let tracker = Self { state };
        let t = tracker.clone();
        std::thread::Builder::new()
            .name("games".into())
            .spawn(move || loop {
                std::thread::sleep(Duration::from_secs(1));
                sample(&t);
            })
            .expect("spawn games thread");
        tracker
    }

    /// Keep up with the capture and the games and apps list (cheap when
    /// nothing changed). A new capture starts a new history.
    pub fn set(&self, capturing: Capturing, apps: &[(String, String)]) {
        let mut s = self.state.lock().unwrap();
        if s.capturing.as_ref() != Some(&capturing) {
            s.recent.clear();
            s.totals.clear();
            s.capturing = Some(capturing);
        }
        if s.apps != apps {
            s.apps = apps.to_vec();
        }
    }

    /// The game a replay clip saved now is of: the one in focus longest in the
    /// buffer's window. `None`: the desktop.
    pub fn clip_game(&self) -> Option<String> {
        let s = self.state.lock().unwrap();
        if s.recent.is_empty() {
            // Saved right as the buffer started: what's in focus now.
            drop(s);
            return self.focused().map(|(_, g)| g.name);
        }
        let mut counts: HashMap<&Option<String>, u32> = HashMap::new();
        for g in &s.recent {
            *counts.entry(g).or_default() += 1;
        }
        longest(counts.into_iter().map(|(g, n)| (g.clone(), n)))
    }

    /// The app in focus now and the game it is, if it's one.
    pub fn focused(&self) -> Option<(String, Game)> {
        let (id, path) = capture::foreground_app_path()?;
        let apps = self.state.lock().unwrap().apps.clone();
        let game = identify(&id, self.installed(&path), &apps)?;
        Some((id, game))
    }

    /// [`installed_game`], read once per app.
    fn installed(&self, path: &Path) -> Option<String> {
        if let Some(known) = self.state.lock().unwrap().by_path.get(path) {
            return known.clone();
        }
        // Outside the lock: reading the stores' files can take a moment.
        let found = installed_game(path);
        self.state.lock().unwrap().by_path.insert(path.to_path_buf(), found.clone());
        found
    }

    /// The game a recording stopped now is of: the one in focus longest.
    pub fn recording_game(&self) -> Option<String> {
        let s = self.state.lock().unwrap();
        if s.totals.is_empty() {
            drop(s);
            return self.focused().map(|(_, g)| g.name);
        }
        longest(s.totals.iter().map(|(g, n)| (g.clone(), *n)))
    }
}

/// The game with the most seconds (a game over the desktop on a tie, then
/// by name, so the pick doesn't wobble).
fn longest(counts: impl Iterator<Item = (Option<String>, u32)>) -> Option<String> {
    counts.max_by(|(a, n), (b, m)| n.cmp(m).then(a.is_some().cmp(&b.is_some())).then(b.cmp(a))).and_then(|(g, _)| g)
}

/// Note what's in focus now.
fn sample(tracker: &Tracker) {
    let state = &tracker.state;
    let seconds = match state.lock().unwrap().capturing {
        Some(Capturing::Buffer(secs)) => Some(secs.max(1) as usize),
        Some(Capturing::Record) => None,
        _ => return,
    };
    let game = tracker.focused().map(|(_, g)| g.name);
    let mut s = state.lock().unwrap();
    match seconds {
        Some(window) => {
            s.recent.push_back(game);
            while s.recent.len() > window {
                s.recent.pop_front();
            }
        }
        None => *s.totals.entry(game).or_default() += 1,
    }
}

/// The game an app is, if it's one: Discord's name for it, else its store
/// folder's (`by_place`), else its name on the games and apps list.
fn identify(id: &str, by_place: Option<String>, apps: &[(String, String)]) -> Option<Game> {
    crate::discord::known_game(id)
        .or_else(|| by_place.map(|name| Game { name, icon: None }))
        .or_else(|| apps.iter().find(|(a, _)| a.eq_ignore_ascii_case(id)).map(|(_, name)| Game { name: name.clone(), icon: None }))
}

/// Where a clip goes: the folder of its game (`None`: Desktop) in the
/// library, or a folder it's been renamed to (`renamed`), or the library
/// itself without folders per game.
pub fn folder_for(lib: &Path, per_game: bool, renamed: &std::collections::BTreeMap<String, String>, game: Option<&str>) -> PathBuf {
    if !per_game {
        return lib.to_path_buf();
    }
    let folder = crate::clips::folder_name(game.unwrap_or(crate::clips::DESKTOP));
    lib.join(renamed.get(&folder).cloned().unwrap_or(folder))
}

/// A game, by where it's installed: in a Steam library (named as Steam does),
/// an Epic game (as the Epic launcher does), or in GOG's or the Xbox app's
/// games folder (by its folder).
pub fn installed_game(exe: &Path) -> Option<String> {
    let parts: Vec<String> = exe.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    let after = |a: &str, b: Option<&str>| -> Option<usize> {
        (0..parts.len()).find(|&i| parts[i].eq_ignore_ascii_case(a) && b.is_none_or(|b| parts.get(i + 1).is_some_and(|p| p.eq_ignore_ascii_case(b))))
            .map(|i| i + if b.is_some() { 2 } else { 1 })
    };
    // The game's folder must hold the app, not be it.
    let folder = |i: usize| (i + 1 < parts.len()).then(|| parts[i].clone());
    if let Some(i) = after("steamapps", Some("common")) {
        let dir = folder(i)?;
        let steamapps: PathBuf = parts[..i - 1].iter().collect();
        return Some(steam_name(&steamapps, &dir).unwrap_or(dir));
    }
    if let Some(name) = epic_name(exe) {
        return Some(name);
    }
    if let Some(i) = after("GOG Galaxy", Some("Games")).or_else(|| after("GOG Games", None)) {
        return folder(i);
    }
    if let Some(i) = after("XboxGames", None) {
        return folder(i);
    }
    None
}

/// Steam's name for the game installed in `steamapps/common/<dir>`, from its
/// `appmanifest_<id>.acf` ("installdir" → "name").
fn steam_name(steamapps: &Path, dir: &str) -> Option<String> {
    for entry in std::fs::read_dir(steamapps).ok()?.flatten() {
        let file = entry.file_name().to_string_lossy().into_owned();
        if !(file.starts_with("appmanifest_") && file.ends_with(".acf")) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else { continue };
        let fields = acf_fields(&text);
        if fields.get("installdir").is_some_and(|d| d.eq_ignore_ascii_case(dir)) {
            return fields.get("name").cloned();
        }
    }
    None
}

/// The top-level `"key"  "value"` pairs of a Steam `.acf` file (the first of
/// each key: the app's own come before its nested sections').
fn acf_fields(text: &str) -> HashMap<String, String> {
    let mut fields = HashMap::new();
    for line in text.lines() {
        let quoted: Vec<&str> = line.split('"').skip(1).step_by(2).collect();
        if let [key, value] = quoted[..] {
            fields.entry(key.to_lowercase()).or_insert_with(|| value.to_owned());
        }
    }
    fields
}

/// The Epic launcher's name for the game installed where `exe` is, from its
/// install manifests (`ProgramData\Epic\EpicGamesLauncher\Data\Manifests`).
fn epic_name(exe: &Path) -> Option<String> {
    #[cfg(windows)]
    {
        let data = std::env::var_os("ProgramData")?;
        let dir = Path::new(&data).join("Epic").join("EpicGamesLauncher").join("Data").join("Manifests");
        let exe = exe.to_string_lossy().to_lowercase();
        for entry in std::fs::read_dir(dir).ok()?.flatten() {
            let Ok(text) = std::fs::read_to_string(entry.path()) else { continue };
            let Ok(item) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
            let (Some(name), Some(at)) = (item["DisplayName"].as_str(), item["InstallLocation"].as_str()) else { continue };
            let at = at.trim_end_matches(['\\', '/']).to_lowercase();
            if !at.is_empty() && exe.starts_with(&at) && exe[at.len()..].starts_with(['\\', '/']) {
                return Some(name.to_owned());
            }
        }
        None
    }
    #[cfg(not(windows))]
    {
        let _ = exe;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn games_by_folder() {
        let steam = std::env::temp_dir().join(format!("hc-steam-{}", std::process::id())).join("steamapps");
        std::fs::create_dir_all(steam.join("common")).unwrap();
        std::fs::write(
            steam.join("appmanifest_730.acf"),
            "\"AppState\"\n{\n\t\"appid\"\t\t\"730\"\n\t\"name\"\t\t\"Counter-Strike 2\"\n\t\"installdir\"\t\t\"Counter-Strike Global Offensive\"\n\t\"UserConfig\"\n\t{\n\t\t\"name\"\t\t\"other\"\n\t}\n}\n",
        )
        .unwrap();
        let cs = steam.join("common").join("Counter-Strike Global Offensive").join("game").join("bin").join("win64").join("cs2.exe");
        assert_eq!(installed_game(&cs).as_deref(), Some("Counter-Strike 2"));
        // No manifest for it: the folder's name.
        let other = steam.join("common").join("Some Game").join("game.exe");
        assert_eq!(installed_game(&other).as_deref(), Some("Some Game"));
        // The common folder itself isn't a game.
        assert_eq!(installed_game(&steam.join("common").join("stray.exe")), None);
        std::fs::remove_dir_all(steam.parent().unwrap()).unwrap();

    }

    /// Windows paths: they only parse as such on Windows.
    #[cfg(windows)]
    #[test]
    fn games_by_store_folder() {
        assert_eq!(installed_game(Path::new(r"C:\Program Files (x86)\GOG Galaxy\Games\Cyberpunk 2077\bin\x64\Cyberpunk2077.exe")).as_deref(), Some("Cyberpunk 2077"));
        assert_eq!(installed_game(Path::new(r"D:\XboxGames\Halo Infinite\Content\HaloInfinite.exe")).as_deref(), Some("Halo Infinite"));
        assert_eq!(installed_game(Path::new(r"C:\Windows\explorer.exe")), None);
        assert_eq!(installed_game(Path::new(r"C:\Program Files\Google\Chrome\Application\chrome.exe")), None);
    }

    #[test]
    fn longest_in_focus() {
        let g = |s: &str| Some(s.to_owned());
        assert_eq!(longest([(g("CS"), 20), (None, 40)].into_iter()), None);
        assert_eq!(longest([(g("CS"), 40), (None, 20)].into_iter()), g("CS"));
        // A tie goes to the game.
        assert_eq!(longest([(g("CS"), 30), (None, 30)].into_iter()), g("CS"));
        assert_eq!(longest(std::iter::empty()), None);
    }
}
