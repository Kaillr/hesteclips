//! What game stores say about an installed game: its name and its id in the
//! store, from the files they keep next to their games (Steam's app
//! manifests, the Epic launcher's install manifests).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A game as its store knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreGame {
    pub name: String,
    pub id: StoreId,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum StoreId {
    /// Steam's app id.
    Steam(String),
    /// The Epic launcher's catalog namespace.
    Epic(String),
}

/// The Steam game an app is part of, when it's installed in a Steam library
/// (`…/steamapps/common/<dir>/…`) and Steam's manifest for it is there.
pub fn steam_game(exe: &Path) -> Option<StoreGame> {
    let (steamapps, dir) = steam_dir(exe)?;
    steam_manifest(&steamapps, &dir)
}

/// The library (`…/steamapps`) and game folder an app in a Steam library is
/// in. The game's folder must hold the app, not be it.
pub fn steam_dir(exe: &Path) -> Option<(PathBuf, String)> {
    let parts: Vec<String> = exe.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    let i = (0..parts.len().saturating_sub(1))
        .find(|&i| parts[i].eq_ignore_ascii_case("steamapps") && parts[i + 1].eq_ignore_ascii_case("common"))?
        + 2;
    if i + 1 >= parts.len() {
        return None;
    }
    Some((parts[..i - 1].iter().collect(), parts[i].clone()))
}

/// Steam's manifest for the game installed in `steamapps/common/<dir>`
/// (`appmanifest_<id>.acf`, "installdir" → "name", "appid").
fn steam_manifest(steamapps: &Path, dir: &str) -> Option<StoreGame> {
    for entry in std::fs::read_dir(steamapps).ok()?.flatten() {
        let file = entry.file_name().to_string_lossy().into_owned();
        if !(file.starts_with("appmanifest_") && file.ends_with(".acf")) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else { continue };
        let fields = acf_fields(&text);
        if fields.get("installdir").is_some_and(|d| d.eq_ignore_ascii_case(dir)) {
            let id = fields.get("appid").cloned().or_else(|| Some(file.strip_prefix("appmanifest_")?.strip_suffix(".acf")?.to_owned()))?;
            return Some(StoreGame { name: fields.get("name").cloned().unwrap_or_else(|| dir.to_owned()), id: StoreId::Steam(id) });
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

/// The Epic game installed where `exe` is, from the launcher's install
/// manifests (`ProgramData\Epic\EpicGamesLauncher\Data\Manifests`).
pub fn epic_game(exe: &Path) -> Option<StoreGame> {
    #[cfg(windows)]
    {
        let data = std::env::var_os("ProgramData")?;
        let dir = Path::new(&data).join("Epic").join("EpicGamesLauncher").join("Data").join("Manifests");
        let exe = exe.to_string_lossy().to_lowercase();
        for entry in std::fs::read_dir(dir).ok()?.flatten() {
            let Ok(text) = std::fs::read_to_string(entry.path()) else { continue };
            let Some(game) = epic_manifest(&text, &exe) else { continue };
            return Some(game);
        }
        None
    }
    #[cfg(not(windows))]
    {
        let _ = exe;
        None
    }
}

/// The game in one Epic install manifest, if `exe` (lowercase) is inside it.
#[cfg_attr(not(windows), allow(dead_code))]
fn epic_manifest(text: &str, exe: &str) -> Option<StoreGame> {
    let item: serde_json::Value = serde_json::from_str(text).ok()?;
    let (name, at) = (item["DisplayName"].as_str()?, item["InstallLocation"].as_str()?);
    let at = at.trim_end_matches(['\\', '/']).to_lowercase();
    if at.is_empty() || !exe.starts_with(&at) || !exe[at.len()..].starts_with(['\\', '/']) {
        return None;
    }
    let namespace = item["CatalogNamespace"].as_str().unwrap_or_default().to_owned();
    Some(StoreGame { name: name.to_owned(), id: StoreId::Epic(namespace) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steam_games() {
        let steam = std::env::temp_dir().join(format!("hc-stores-{}", std::process::id())).join("steamapps");
        std::fs::create_dir_all(steam.join("common")).unwrap();
        std::fs::write(
            steam.join("appmanifest_730.acf"),
            "\"AppState\"\n{\n\t\"appid\"\t\t\"730\"\n\t\"name\"\t\t\"Counter-Strike 2\"\n\t\"installdir\"\t\t\"Counter-Strike Global Offensive\"\n\t\"UserConfig\"\n\t{\n\t\t\"name\"\t\t\"other\"\n\t}\n}\n",
        )
        .unwrap();
        let cs = steam.join("common").join("Counter-Strike Global Offensive").join("game").join("bin").join("win64").join("cs2.exe");
        assert_eq!(steam_game(&cs), Some(StoreGame { name: "Counter-Strike 2".into(), id: StoreId::Steam("730".into()) }));
        assert_eq!(steam_game(&steam.join("common").join("Other").join("x.exe")), None);
        assert_eq!(steam_dir(&steam.join("common").join("stray.exe")), None);
        std::fs::remove_dir_all(steam.parent().unwrap()).unwrap();
    }

    #[test]
    fn epic_manifests() {
        let text = r#"{ "FormatVersion": 0, "DisplayName": "Rocket League\u00ae", "InstallLocation": "C:\\Games\\rocketleague",
            "CatalogNamespace": "9773aa1aa54f4f7b80e44bef04986cea" }"#;
        let game = epic_manifest(text, r"c:\games\rocketleague\binaries\win64\rocketleague.exe").unwrap();
        assert_eq!(game.name, "Rocket League®");
        assert_eq!(game.id, StoreId::Epic("9773aa1aa54f4f7b80e44bef04986cea".into()));
        assert_eq!(epic_manifest(text, r"c:\games\rocketleague2\x.exe"), None);
    }
}
