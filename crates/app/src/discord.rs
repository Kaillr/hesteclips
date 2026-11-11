//! "Clipping osu! with HesteClips" on your Discord profile while HesteClips
//! clips or records a game (on unless turned off in Settings → Sharing), with
//! a Get HesteClips button that others see (Discord doesn't show you your own
//! activity's buttons). Nothing without a game: clipping runs all day in the
//! tray, and "Clipping" alone on a profile for hours says nothing (and is what
//! would make someone turn it off).
//!
//! Discord Rich Presence: the Discord app on this computer takes an activity
//! from us over its local pipe. The activity belongs to the "HesteClips"
//! application in Discord's developer portal ([`CLIENT_ID`]), whose `logo` art
//! asset is the small badge. The game's own picture and name come from
//! Discord's public list of games it detects, matched by executable.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
use std::time::{Duration, SystemTime};

use discord_rich_presence::activity::{Activity, Assets, Button, Timestamps};
use discord_rich_presence::{DiscordIpc, DiscordIpcClient};
use serde::{Deserialize, Serialize};

/// The HesteClips application in Discord's developer portal.
const CLIENT_ID: &str = "1556434870572679198";
/// Its art asset with the HesteClips logo (`assets/discord-logo.png`).
const LOGO: &str = "logo";
/// The activity's button: HesteClips' site, which shows what it is first.
const GET: (&str, &str) = ("Get HesteClips", "https://github.com/Kaillr/hesteclips");
/// While wanted but not connected (Discord not running yet), try again this often.
const RETRY_EVERY: Duration = Duration::from_secs(15);
/// Fetch Discord's games list again after this long (it's ~13 MB).
const GAMES_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// What the profile should show.
#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    /// Recording, not just the replay buffer.
    pub recording: bool,
    /// Live.
    pub streaming: bool,
    pub game: Game,
    /// Since when, for Discord's "elapsed" timer: when the game showed up.
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
    let doing = if status.streaming { "Streaming" } else if status.recording { "Recording" } else { "Clipping" };
    // As Medal does it: the activity is named after the game ("osu! with
    // HesteClips") instead of the app, so it reads like the game itself.
    let g = &status.game;
    let (name, details) = (format!("{} with HesteClips", g.name), format!("{doing} {} with HesteClips", g.name));
    let assets = match g.icon.as_deref() {
        // The game's picture, with HesteClips as the badge in its corner.
        Some(icon) => Assets::new().large_image(icon).large_text(g.name.as_str()).small_image(LOGO).small_text("HesteClips"),
        None => Assets::new().large_image(LOGO).large_text("HesteClips"),
    };
    // In seconds: Discord takes milliseconds as seconds (whatever the crate
    // says), and a start that far ahead shows a timer stuck at 0:00.
    let since = status.since.duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    Activity::new().name(name).details(details).assets(assets).timestamps(Timestamps::new().start(since)).buttons(vec![Button::new(GET.0, GET.1)])
}

/// A game Discord knows, by the app's id (`capture::foreground_exe`) and
/// where its program is: an executable on Windows (`osu!.exe`), a bundle id
/// on macOS, looked up by its bundle's name as Discord lists them (`world
/// of warcraft.app`), a program's name on Linux. `None` until the list has
/// loaded (from the cache, or from Discord in the background).
pub fn known_game(exe: &str, path: &Path) -> Option<Game> {
    #[cfg(target_os = "macos")]
    let exe = &capture::app_file_name(exe)?;
    let by_exe = &games()?.by_exe;
    let found = by_exe.get(&exe.to_lowercase());
    // Linux: Discord lists hardly any programs for it (8 of 11,000), but a
    // game made for both is mostly named as its Windows program is, less the
    // `.exe` (osu!'s `osu!`).
    #[cfg(target_os = "linux")]
    let found = found.or_else(|| windows_name(exe, path).and_then(|name| by_exe.get(&name)));
    let _ = path;
    found.map(KnownGame::game)
}

/// The name a native Linux program's game would have on Windows (`osu!` is
/// `osu!.exe`, a Unity or Godot game's `Game.x86_64` is `game.exe`), to
/// look it up among Discord's Windows programs. Not for what the system
/// installed, which is an app (KDE's file manager `dolphin` isn't the
/// emulator `dolphin.exe`; games it installs go in `/usr/games`), nor for
/// runtimes many games share (`java` isn't one game).
#[cfg(target_os = "linux")]
fn windows_name(exe: &str, path: &Path) -> Option<String> {
    const SYSTEM: [&str; 8] = ["/usr/", "/bin/", "/sbin/", "/lib/", "/lib64/", "/opt/", "/app/", "/snap/"];
    const GAMES: [&str; 2] = ["/usr/games/", "/usr/local/games/"];
    const RUNTIMES: [&str; 14] = ["java", "javaw", "python", "python3", "pythonw", "node", "mono", "dotnet", "love", "electron", "wine", "wine64", "bash", "sh"];
    let place = path.to_string_lossy();
    if SYSTEM.iter().any(|d| place.starts_with(d)) && !GAMES.iter().any(|d| place.starts_with(d)) {
        return None;
    }
    let mut name = exe.to_lowercase();
    if name.ends_with(".exe") {
        return None;
    }
    while let Some(stem) = [".x86_64", ".x86", ".bin", ".sh", ".appimage"].iter().find_map(|s| name.strip_suffix(s)) {
        name = stem.to_owned();
    }
    (!name.is_empty() && !RUNTIMES.contains(&name.as_str())).then(|| format!("{name}.exe"))
}

/// A game Discord knows, by the library folder its clips go in
/// (`clips::folder_name` of its name, any case). `None` until the list has loaded.
pub fn game_for_folder(folder: &str) -> Option<Game> {
    static BY_FOLDER: OnceLock<HashMap<String, Game>> = OnceLock::new();
    let by_folder = match BY_FOLDER.get() {
        Some(map) => map,
        None => {
            let games = games()?;
            BY_FOLDER.get_or_init(|| games.by_name.iter().map(|(folder, g)| (folder.clone(), g.game())).collect())
        }
    };
    by_folder.get(&folder.to_lowercase()).cloned()
}

/// Discord's games by executable, once loaded: the first ask starts loading
/// them in the background.
fn games() -> Option<&'static Games> {
    static GAMES: OnceLock<Option<Games>> = OnceLock::new();
    static LOADING: AtomicBool = AtomicBool::new(false);
    if GAMES.get().is_none() && !LOADING.swap(true, Ordering::Relaxed) {
        std::thread::spawn(|| {
            let _ = GAMES.set(load_games());
        });
    }
    GAMES.get()?.as_ref()
}

/// What's known of Discord's games.
#[derive(Serialize, Deserialize)]
struct Games {
    /// By executable, for this OS (to tell what's running); on Linux,
    /// Windows' too (a Wine or Proton game is its `.exe`, and see
    /// `windows_name`).
    by_exe: HashMap<String, KnownGame>,
    /// By library folder name (lowercase), whatever OS the game is for: a
    /// folder keeps its icon on a Mac for a game that only runs on Windows.
    by_name: HashMap<String, KnownGame>,
}

#[derive(Clone, Serialize, Deserialize)]
struct KnownGame {
    name: String,
    id: String,
    icon: String,
}

impl KnownGame {
    fn game(&self) -> Game {
        Game { name: self.name.clone(), icon: Some(format!("https://cdn.discordapp.com/app-icons/{}/{}.png?size=512", self.id, self.icon)) }
    }
}

fn games_cache() -> Option<PathBuf> {
    dirs::cache_dir().map(|d| d.join("hesteclips").join("discord-games-4.json"))
}

/// Discord's games, from our cache while it's fresh, else downloaded (and cached).
fn load_games() -> Option<Games> {
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

fn download_games() -> Option<Games> {
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
    // Executable names are paths (`win64/cs2.exe`, `osx64/dota2.app`); match
    // on the file name.
    // Some are only told apart by their arguments (`>javaw.exe`), and a few
    // names are shared by several games (`game.exe`): those can't be matched.
    let mut by_exe: HashMap<String, Option<KnownGame>> = HashMap::new();
    let mut by_name: HashMap<String, KnownGame> = HashMap::new();
    for app in apps {
        let Some(icon) = app.icon_hash else { continue };
        by_name.entry(crate::clips::folder_name(&app.name).to_lowercase()).or_insert_with(|| KnownGame { name: app.name.clone(), id: app.id.clone(), icon: icon.clone() });
        // Discord's names for the OSes.
        let ours = match std::env::consts::OS {
            "windows" => "win32",
            "macos" => "darwin",
            other => other,
        };
        for exe in app.executables {
            let os = exe.os == ours || (ours == "linux" && exe.os == "win32");
            if exe.is_launcher || exe.arguments.is_some() || exe.name.starts_with('>') || !os {
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
    Some(Games { by_exe: by_exe.into_iter().filter_map(|(exe, g)| Some((exe, g?))).collect(), by_name })
}

#[cfg(test)]
mod tests {
    /// A native Linux game is looked up by its Windows name; what the system
    /// installed, and shared runtimes, aren't.
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_programs_by_their_windows_name() {
        use std::path::Path;
        let name = |exe: &str, path: &str| super::windows_name(exe, Path::new(path));
        assert_eq!(name("osu!", "/tmp/.mount_osu.ApgBOjon/usr/bin/osu!").as_deref(), Some("osu!.exe"), "an AppImage");
        assert_eq!(name("Game.x86_64", "/home/me/Games/game/Game.x86_64").as_deref(), Some("game.exe"));
        assert_eq!(name("terraria.bin.x86", "/home/me/Games/Terraria/terraria.bin.x86").as_deref(), Some("terraria.exe"));
        assert_eq!(name("supertuxkart", "/usr/games/supertuxkart").as_deref(), Some("supertuxkart.exe"));
        assert_eq!(name("dolphin", "/usr/bin/dolphin"), None, "KDE's file manager, not the emulator");
        assert_eq!(name("chrome", "/opt/google/chrome/chrome"), None);
        assert_eq!(name("krita", "/app/bin/krita"), None, "a Flatpak");
        assert_eq!(name("java", "/home/me/.local/share/PrismLauncher/java/bin/java"), None, "a runtime");
        assert_eq!(name("game.exe", "/home/me/Games/wine/game.exe"), None, "already a Windows name");
    }

    /// Downloads Discord's real list: `cargo test -p hesteclips discord -- --ignored`.
    #[test]
    #[ignore = "network"]
    fn finds_games_by_executable() {
        let games = super::download_games().expect("download and parse Discord's games");
        assert_eq!(games.by_exe["osu!.exe"].name, "osu!");
        assert_eq!(games.by_exe["cs2.exe"].name, "Counter-Strike 2");
        assert!(!games.by_exe.contains_key("game.exe"), "names shared by several games are dropped");
        assert_eq!(games.by_name["counter-strike 2"].name, "Counter-Strike 2");
        // osu!lazer on Linux, from its AppImage: known by its Windows name.
        #[cfg(target_os = "linux")]
        {
            let name = super::windows_name("osu!", std::path::Path::new("/tmp/.mount_osu.ApgBOjon/usr/bin/osu!")).unwrap();
            assert_eq!(games.by_exe[&name].name, "osu!");
        }
    }
}
