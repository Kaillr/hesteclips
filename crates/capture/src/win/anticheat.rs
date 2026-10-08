//! Whether a game uses anti-cheat, so the game capture hook stays out of it
//! unless you ask for it. Anti-cheat software watches what's loaded into its
//! game; a hook it doesn't recognise can get the game closed, or worse.
//!
//! Three ways to tell, any one enough: the anti-cheat's own files next to the
//! game (Easy Anti-Cheat, BattlEye and most others ship a folder or a
//! launcher with it), AreWeAntiCheatYet's list of games with anti-cheat (by
//! Steam app id, Epic namespace or name), and a short list of our own for
//! games where neither works (Riot's Vanguard, Battle.net's games). Nothing
//! here looks inside the game's process: only at files on disk.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::stores::{self, StoreId};

/// The anti-cheat a game uses (its name, as "BattlEye"), if it uses any we
/// can tell. Cached per executable: it looks at the disk.
pub fn anticheat(exe: &Path) -> Option<String> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Option<String>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(known) = cache.lock().unwrap().get(exe) {
        return known.clone();
    }
    let found = detect(exe);
    cache.lock().unwrap().insert(exe.to_path_buf(), found.clone());
    found
}

fn detect(exe: &Path) -> Option<String> {
    let file = exe.file_name()?.to_string_lossy().to_lowercase();
    if let Some((_, name)) = KNOWN_EXES.iter().find(|(e, _)| e.eq_ignore_ascii_case(&file)) {
        return Some((*name).to_owned());
    }
    let path = exe.to_string_lossy().to_lowercase();
    if path.contains(r"\riot games\") {
        return Some("Vanguard".to_owned());
    }
    let store = stores::steam_game(exe).or_else(|| stores::epic_game(exe));
    if let Some(found) = store.as_ref().and_then(|g| listed(Some(&g.id), &g.name)) {
        return Some(found);
    }
    // The game's folder: the store's, else the app's own and the two above
    // it (games often keep their binaries a few folders down).
    let root = stores::steam_dir(exe).map(|(steamapps, dir)| steamapps.join("common").join(dir));
    let dirs: Vec<PathBuf> = match &root {
        Some(root) => exe.ancestors().skip(1).take_while(|d| d.starts_with(root)).map(Path::to_path_buf).collect(),
        None => exe.ancestors().skip(1).take(3).filter(|d| !is_container(d)).map(Path::to_path_buf).collect(),
    };
    for dir in &dirs {
        if let Some(found) = files_in(dir) {
            return Some(found);
        }
        // A folder named like a game on the list.
        if let Some(name) = dir.file_name().map(|n| n.to_string_lossy().into_owned())
            && let Some(found) = listed(None, &name)
        {
            return Some(found);
        }
    }
    None
}

/// Folders that hold many apps rather than being one (never searched for an
/// anti-cheat's files: one game's wouldn't say anything about another).
fn is_container(dir: &Path) -> bool {
    let Some(name) = dir.file_name() else { return true }; // a drive's root
    let name = name.to_string_lossy().to_lowercase();
    ["program files", "program files (x86)", "games", "steamapps", "common", "steamlibrary", "xboxgames", "windows", "users", "appdata", "local", "roaming", "programs"]
        .contains(&name.as_str())
}

/// The anti-cheat whose files are in `dir`, if any.
fn files_in(dir: &Path) -> Option<String> {
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten().take(2000) {
        let name = entry.file_name().to_string_lossy().to_lowercase();
        if let Some((_, ac)) = MARKERS.iter().find(|(m, _)| *m == name) {
            return Some((*ac).to_owned());
        }
    }
    None
}

/// Files and folders anti-cheats install next to their game (lowercase).
const MARKERS: &[(&str, &str)] = &[
    ("easyanticheat", "Easy Anti-Cheat"),
    ("easyanticheat_eos", "Easy Anti-Cheat"),
    ("easyanticheat_eos_setup.exe", "Easy Anti-Cheat"),
    ("easyanticheat_setup.exe", "Easy Anti-Cheat"),
    ("start_protected_game.exe", "Easy Anti-Cheat"),
    ("battleye", "BattlEye"),
    ("beservice.exe", "BattlEye"),
    ("beservice_x64.exe", "BattlEye"),
    ("beclient.dll", "BattlEye"),
    ("beclient_x64.dll", "BattlEye"),
    ("gameguard", "nProtect GameGuard"),
    ("gameguard.des", "nProtect GameGuard"),
    ("xigncode", "XIGNCODE3"),
    ("x3.xem", "XIGNCODE3"),
    ("mhypbase.dll", "miHoYo Protect"),
    ("mhyprot2.sys", "miHoYo Protect"),
    ("mhyprot3.sys", "miHoYo Protect"),
    ("anticheatexpert", "Anti-Cheat Expert"),
    ("eaanticheat", "EA Anticheat"),
    ("eaanticheat.gameservicelauncher.exe", "EA Anticheat"),
    ("eaanticheat.installer.exe", "EA Anticheat"),
    ("pnkbstra.exe", "PunkBuster"),
    ("pnkbstrb.exe", "PunkBuster"),
    ("hshield", "AhnLab HackShield"),
    ("ehsvc.dll", "AhnLab HackShield"),
    ("equ8", "EQU8"),
    ("blackcipher", "Nexon Game Security"),
    ("denuvo-anti-cheat", "Denuvo Anti-Cheat"),
];

/// Games whose anti-cheat leaves nothing in their folder, by executable.
const KNOWN_EXES: &[(&str, &str)] = &[
    ("cs2.exe", "VAC"),
    ("csgo.exe", "VAC"),
    ("dota2.exe", "VAC"),
    ("project8.exe", "VAC"),
    ("deadlock.exe", "VAC"),
    ("tf_win64.exe", "VAC"),
    ("hl2.exe", "VAC"),
    ("valorant-win64-shipping.exe", "Vanguard"),
    ("league of legends.exe", "Vanguard"),
    ("overwatch.exe", "Defense Matrix"),
    ("cod.exe", "RICOCHET"),
    ("cod24-cod.exe", "RICOCHET"),
    ("blackops6.exe", "RICOCHET"),
    ("modernwarfare.exe", "RICOCHET"),
    ("wow.exe", "Warden"),
    ("diablo iv.exe", "Warden"),
    ("robloxplayerbeta.exe", "Hyperion"),
    ("fortniteclient-win64-shipping.exe", "Easy Anti-Cheat"),
    ("r5apex.exe", "Easy Anti-Cheat"),
    ("r5apex_dx12.exe", "Easy Anti-Cheat"),
    ("tslgame.exe", "BattlEye"),
    ("rainbowsix.exe", "BattlEye"),
    ("rainbowsix_be.exe", "BattlEye"),
    ("destiny2.exe", "BattlEye"),
    ("escapefromtarkov.exe", "BattlEye"),
    ("genshinimpact.exe", "miHoYo Protect"),
    ("starrail.exe", "miHoYo Protect"),
    ("zenlesszonezero.exe", "miHoYo Protect"),
    ("marvel-win64-shipping.exe", "Anti-Cheat Expert"),
    ("faceit.exe", "FACEIT"),
];

/// The anti-cheat AreWeAntiCheatYet lists for a game: by store id when
/// there's one, else by name (letters and digits only, any case).
fn listed(id: Option<&StoreId>, name: &str) -> Option<String> {
    let list = list();
    if let Some(found) = id.and_then(|id| list.by_id.get(id)) {
        return Some(found.clone());
    }
    let key = name_key(name);
    if key.len() < 3 {
        return None;
    }
    list.by_name.get(&key).cloned()
}

fn name_key(name: &str) -> String {
    name.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

struct List {
    by_id: HashMap<StoreId, String>,
    by_name: HashMap<String, String>,
}

/// AreWeAntiCheatYet's list, refreshed by `scripts/update-anticheat-list.mjs`.
fn list() -> &'static List {
    static LIST: OnceLock<List> = OnceLock::new();
    LIST.get_or_init(|| {
        let mut list = List { by_id: HashMap::new(), by_name: HashMap::new() };
        for line in include_str!("anticheat-games.tsv").lines().filter(|l| !l.starts_with('#')) {
            let mut cols = line.split('\t');
            let (Some(name), Some(acs)) = (cols.next(), cols.next()) else { continue };
            // The first is the main one ("VAC" for a game with VAC and FACEIT).
            let ac = acs.split(',').next().unwrap_or(acs).to_owned();
            if let Some(steam) = cols.next().filter(|s| !s.is_empty()) {
                list.by_id.insert(StoreId::Steam(steam.to_owned()), ac.clone());
            }
            if let Some(epic) = cols.next().filter(|s| !s.is_empty()) {
                list.by_id.insert(StoreId::Epic(epic.to_owned()), ac.clone());
            }
            // Entries for one game on a third-party service ("Counter-Strike 2
            // (FACEIT)") don't name the game itself.
            if !name.ends_with(')') {
                list.by_name.entry(name_key(name)).or_insert(ac);
            }
        }
        list
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listed_games() {
        assert_eq!(listed(Some(&StoreId::Steam("252950".into())), "whatever").as_deref(), Some("Easy Anti-Cheat"));
        assert_eq!(listed(None, "Rocket League®").as_deref(), Some("Easy Anti-Cheat"));
        assert_eq!(listed(None, "Geometry Dash"), None);
        assert_eq!(listed(None, "osu!"), None);
        assert_eq!(listed(None, "bin"), None);
    }

    #[test]
    fn known_and_by_files() {
        assert_eq!(detect(Path::new(r"C:\Riot Games\VALORANT\live\ShooterGame\Binaries\Win64\VALORANT-Win64-Shipping.exe")).as_deref(), Some("Vanguard"));
        assert_eq!(detect(Path::new(r"D:\SteamLibrary\steamapps\common\Counter-Strike Global Offensive\game\bin\win64\cs2.exe")).as_deref(), Some("VAC"));

        let root = std::env::temp_dir().join(format!("hc-ac-{}", std::process::id()));
        let game = root.join("Some Game");
        std::fs::create_dir_all(game.join("Binaries").join("Win64")).unwrap();
        let exe = game.join("Binaries").join("Win64").join("Game.exe");
        assert_eq!(detect(&exe), None);
        std::fs::create_dir_all(game.join("EasyAntiCheat")).unwrap();
        assert_eq!(detect(&exe).as_deref(), Some("Easy Anti-Cheat"));
        std::fs::remove_dir_all(&root).unwrap();

        assert_eq!(detect(Path::new(r"C:\Games\Geometry Dash\GeometryDash.exe")), None);
    }
}
