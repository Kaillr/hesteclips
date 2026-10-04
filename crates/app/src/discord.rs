//! "Clipping osu!" on your Discord profile, while HesteClips records (opt-in,
//! Settings → Discord).
//!
//! Discord Rich Presence: the Discord app on this computer takes an activity
//! from us over its local pipe. The activity belongs to the "HesteClips"
//! application in Discord's developer portal ([`CLIENT_ID`]), whose `logo` art
//! asset is the small badge. The game's own picture and name come from
//! Discord's public list of games it detects, matched by executable.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
use std::time::{Duration, SystemTime};

use discord_rich_presence::activity::{Activity, Assets, Timestamps};
use discord_rich_presence::{DiscordIpc, DiscordIpcClient};
use serde::{Deserialize, Serialize};

/// The HesteClips application in Discord's developer portal.
const CLIENT_ID: &str = "1556434870572679198";
/// Its art asset with the HesteClips logo (`assets/discord-logo.png`).
const LOGO: &str = "logo";
/// While wanted but not connected (Discord not running yet), try again this often.
const RETRY_EVERY: Duration = Duration::from_secs(15);
/// Fetch Discord's games list again after this long (it's ~13 MB).
const GAMES_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// What the profile should show.
#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    /// Recording, not just the replay buffer.
    pub recording: bool,
    pub game: Option<Game>,
    /// Since when, for Discord's "elapsed" timer.
    pub since: SystemTime,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Game {
    pub name: String,
    /// Its picture, when Discord knows the game.
    pub icon: Option<String>,
}

/// The connection to Discord, run on its own thread so a slow or missing
/// Discord never holds up the UI.
pub struct Presence {
    tx: mpsc::Sender<Option<Status>>,
    last: Option<Option<Status>>,
    connected: Arc<AtomicBool>,
}

impl Presence {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        let connected = Arc::new(AtomicBool::new(false));
        let flag = connected.clone();
        std::thread::Builder::new()
            .name("discord".into())
            .spawn(move || run(rx, flag))
            .expect("spawn discord thread");
        Self { tx, last: None, connected }
    }

    /// Show `status`, or nothing. Cheap to call every frame: only changes go out.
    pub fn set(&mut self, status: Option<Status>) {
        if self.last.as_ref() != Some(&status) {
            self.last = Some(status.clone());
            let _ = self.tx.send(status);
        }
    }

    /// Whether Discord took the last status.
    pub fn connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }
}

fn run(rx: mpsc::Receiver<Option<Status>>, connected: Arc<AtomicBool>) {
    let mut client: Option<DiscordIpcClient> = None;
    let mut wanted: Option<Status> = None;
    loop {
        match rx.recv_timeout(RETRY_EVERY) {
            Ok(status) => wanted = status,
            // Nothing new: only a missing connection needs another try.
            Err(mpsc::RecvTimeoutError::Timeout) if wanted.is_none() || client.is_some() => continue,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        let Some(status) = &wanted else {
            // Off: hang up, which clears the activity too.
            if let Some(mut c) = client.take() {
                let _ = c.clear_activity();
                let _ = c.close();
            }
            connected.store(false, Ordering::Relaxed);
            continue;
        };
        if client.is_none() {
            let mut c = DiscordIpcClient::new(CLIENT_ID);
            if c.connect().is_ok() {
                client = Some(c);
            }
        }
        let ok = client.as_mut().is_some_and(|c| c.set_activity(activity(status)).is_ok());
        if !ok {
            // Discord quit (or never ran): start over on the next try.
            client = None;
        }
        connected.store(ok, Ordering::Relaxed);
    }
    if let Some(mut c) = client {
        let _ = c.close();
    }
}

fn activity(status: &Status) -> Activity<'_> {
    let doing = if status.recording { "Recording" } else { "Clipping" };
    // As Medal does it: the activity is named after the game ("osu! with
    // HesteClips") instead of the app, so it reads like the game itself.
    let (name, details) = match &status.game {
        Some(g) => (Some(format!("{} with HesteClips", g.name)), format!("{doing} {} with HesteClips", g.name)),
        None if status.recording => (None, "Recording".to_owned()),
        None => (None, "Replay buffer on".to_owned()),
    };
    let assets = match status.game.as_ref().and_then(|g| Some((g, g.icon.as_deref()?))) {
        // The game's picture, with HesteClips as the badge in its corner.
        Some((g, icon)) => Assets::new().large_image(icon).large_text(g.name.as_str()).small_image(LOGO).small_text("HesteClips"),
        None => Assets::new().large_image(LOGO).large_text("HesteClips"),
    };
    // In seconds: Discord takes milliseconds as seconds (whatever the crate
    // says), and a start that far ahead shows a timer stuck at 0:00.
    let since = status.since.duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    let activity = Activity::new().details(details).assets(assets).timestamps(Timestamps::new().start(since));
    match name {
        Some(name) => activity.name(name),
        None => activity,
    }
}

/// A game Discord knows, by executable (`osu!.exe`, lower case). `None` until
/// the list has loaded (from the cache, or from Discord in the background).
pub fn known_game(exe: &str) -> Option<Game> {
    static GAMES: OnceLock<Option<HashMap<String, KnownGame>>> = OnceLock::new();
    static LOADING: AtomicBool = AtomicBool::new(false);
    if GAMES.get().is_none() && !LOADING.swap(true, Ordering::Relaxed) {
        std::thread::spawn(|| {
            let _ = GAMES.set(load_games());
        });
    }
    let game = GAMES.get()?.as_ref()?.get(&exe.to_lowercase())?;
    Some(Game { name: game.name.clone(), icon: Some(format!("https://cdn.discordapp.com/app-icons/{}/{}.png?size=512", game.id, game.icon)) })
}

#[derive(Serialize, Deserialize)]
struct KnownGame {
    name: String,
    id: String,
    icon: String,
}

fn games_cache() -> Option<PathBuf> {
    dirs::cache_dir().map(|d| d.join("hesteclips").join("discord-games.json"))
}

/// Discord's games, from our cache while it's fresh, else downloaded (and cached).
fn load_games() -> Option<HashMap<String, KnownGame>> {
    let cache = games_cache()?;
    let fresh = std::fs::metadata(&cache).and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().is_ok_and(|age| age < GAMES_MAX_AGE));
    let cached = || serde_json::from_slice(&std::fs::read(&cache).ok()?).ok();
    if fresh && let Some(games) = cached() {
        return Some(games);
    }
    match download_games() {
        Some(games) => {
            if let Some(dir) = cache.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&cache, serde_json::to_vec(&games).unwrap_or_default());
            Some(games)
        }
        // Offline: an old list beats none.
        None => cached(),
    }
}

fn download_games() -> Option<HashMap<String, KnownGame>> {
    #[derive(Deserialize)]
    struct App {
        id: String,
        name: String,
        icon_hash: Option<String>,
        #[serde(default)]
        executables: Vec<Exe>,
    }
    #[derive(Deserialize)]
    struct Exe {
        name: String,
        os: String,
        #[serde(default)]
        is_launcher: bool,
        arguments: Option<String>,
    }
    let apps: Vec<App> = ureq::get("https://discord.com/api/v9/applications/detectable")
        .call()
        .ok()?
        .body_mut()
        .with_config()
        .limit(64 << 20)
        .read_json()
        .ok()?;
    // Executable names are paths (`win64/cs2.exe`); match on the file name.
    // Some are only told apart by their arguments (`>javaw.exe`), and a few
    // names are shared by several games (`game.exe`): those can't be matched.
    let mut by_exe: HashMap<String, Option<KnownGame>> = HashMap::new();
    for app in apps {
        let Some(icon) = app.icon_hash else { continue };
        let ours = std::env::consts::OS == "windows";
        for exe in app.executables {
            if exe.is_launcher || exe.arguments.is_some() || exe.name.starts_with('>') || (exe.os == "win32") != ours {
                continue;
            }
            let file = exe.name.rsplit('/').next().unwrap_or(&exe.name).to_lowercase();
            let game = KnownGame { name: app.name.clone(), id: app.id.clone(), icon: icon.clone() };
            by_exe
                .entry(file)
                .and_modify(|g| {
                    if g.as_ref().is_some_and(|g| g.id != app.id) {
                        *g = None;
                    }
                })
                .or_insert(Some(game));
        }
    }
    Some(by_exe.into_iter().filter_map(|(exe, g)| Some((exe, g?))).collect())
}

#[cfg(test)]
mod tests {
    /// Downloads Discord's real list: `cargo test -p hesteclips discord -- --ignored`.
    #[test]
    #[ignore = "network"]
    fn finds_games_by_executable() {
        let games = super::download_games().expect("download and parse Discord's games");
        assert_eq!(games["osu!.exe"].name, "osu!");
        assert_eq!(games["cs2.exe"].name, "Counter-Strike 2");
        assert!(!games.contains_key("game.exe"), "names shared by several games are dropped");
    }
}
