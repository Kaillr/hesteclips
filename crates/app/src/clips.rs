//! The clip library on disk: find finished clips and hand them off to the OS file
//! manager or default player.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Local, NaiveDate, NaiveDateTime};

const VIDEO_EXTS: &[&str] = &["mp4", "mkv", "mov", "webm", "avi"];

#[derive(Debug, Clone)]
pub struct Clip {
    pub path: PathBuf,
    pub name: String,
    pub modified: SystemTime,
    pub size_bytes: u64,
    /// Permanent id from the file's metadata, once the clip has been edited.
    pub id: Option<String>,
    /// The untouched recording, when this clip is an edit of it.
    pub original: Option<PathBuf>,
    /// The library's folder it's in (its game's), if it isn't loose in the library.
    pub folder: Option<String>,
}

impl Clip {
    /// When the clip was captured: parsed from our `clip_YYYY-MM-DD_HH-MM-SS` names
    /// (stable even if the file is copied), else the file's modified time.
    pub fn captured_at(&self) -> NaiveDateTime {
        self.own_timestamp()
            .unwrap_or_else(|| DateTime::<Local>::from(self.modified).naive_local())
    }

    /// Timestamp from our own names: `clip_…`/`recording_…` + `YYYY-MM-DD_HH-MM-SS`,
    /// optionally followed by a suffix (e.g. ` (edit)` on an edited copy).
    fn own_timestamp(&self) -> Option<NaiveDateTime> {
        let stem = self.path.file_stem()?.to_string_lossy().into_owned();
        let ts = stem.strip_prefix("clip_").or_else(|| stem.strip_prefix("recording_"))?;
        NaiveDateTime::parse_from_str(ts.get(..19)?, "%Y-%m-%d_%H-%M-%S").ok()
    }


    pub fn day(&self) -> NaiveDate {
        self.captured_at().date()
    }

    /// The name shown and edited when renaming: the title for our own
    /// timestamped names, so the default reads "14.51.01" rather than
    /// "clip_2026-10-01_14-51-01".
    pub fn editable_name(&self) -> String {
        sanitize_name(&self.title())
    }

    /// Card title: the capture time for our own clips, the file name otherwise.
    pub fn title(&self) -> String {
        let stem = self.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        title_for_stem(&stem)
    }

    /// Human-readable size, e.g. "1.4 GB".
    pub fn human_size(&self) -> String {
        let b = self.size_bytes as f64;
        const KB: f64 = 1024.0;
        const MB: f64 = KB * 1024.0;
        const GB: f64 = MB * 1024.0;
        if b >= GB {
            format!("{:.1} GB", b / GB)
        } else if b >= MB {
            format!("{:.0} MB", b / MB)
        } else if b >= KB {
            format!("{:.0} KB", b / KB)
        } else {
            format!("{b} B")
        }
    }
}

/// The library's clips, newest first: the ones in `lib` itself and in its
/// folders (one per game), one level deep. Missing dir → empty list (not an error).
pub fn scan(lib: &Path) -> Vec<Clip> {
    // Only edited clips have ids; skip reading metadata when nothing's been edited.
    let any_edits = lib.join(crate::store::DIR).is_dir();
    let mut clips = scan_dir(lib, lib, None, any_edits);
    for folder in folders(lib) {
        clips.extend(scan_dir(lib, &lib.join(&folder), Some(&folder), any_edits));
    }
    clips.sort_by(|a, b| b.captured_at().cmp(&a.captured_at()));
    clips
}

/// The library's folders, by name. Hidden ones (`.hesteclips`, the edits)
/// aren't clips.
pub fn folders(lib: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(lib) else { return Vec::new() };
    entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
        .filter(|name| !name.starts_with('.'))
        .collect()
}

/// When the library or one of its folders last changed: adding, removing or
/// renaming a clip in any of them changes this.
pub fn stamp(lib: &Path) -> Vec<Option<SystemTime>> {
    let mtime = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let mut names = folders(lib);
    names.sort();
    std::iter::once(mtime(lib)).chain(names.iter().map(|n| mtime(&lib.join(n)))).collect()
}

/// The clips directly in `dir`, which is `lib` or its `folder`.
fn scan_dir(lib: &Path, dir: &Path, folder: Option<&str>, any_edits: bool) -> Vec<Clip> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            // Recordings are written to a hidden `.clip_….mp4` and only renamed when
            // finished, so skipping dotfiles hides anything still being written.
            if path.file_name()?.to_string_lossy().starts_with('.') {
                return None;
            }
            let ext_ok = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| VIDEO_EXTS.contains(&e.to_ascii_lowercase().as_str()))
                .unwrap_or(false);
            if !ext_ok {
                return None;
            }
            let meta = entry.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            let id = if any_edits { crate::store::read_id(&path) } else { None };
            let original = id.as_deref().and_then(|id| crate::store::find_original(lib, id));
            Some(Clip {
                name: path.file_name()?.to_string_lossy().into_owned(),
                modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                size_bytes: meta.len(),
                id,
                original,
                folder: folder.map(str::to_owned),
                path,
            })
        })
        .collect()
}

/// The folder for clips with no game.
pub const DESKTOP: &str = "Desktop";

/// A folder name for a game, as Windows (the strictest) allows: "Counter-Strike:
/// Global Offensive" → "Counter-Strike Global Offensive". Names Windows keeps
/// for devices (`CON`, `COM1`…) get a `_`.
pub fn folder_name(game: &str) -> String {
    let kept: String = game.chars().filter(|&c| !c.is_control() && !FORBIDDEN.contains(&c)).collect();
    let mut name = kept.split_whitespace().collect::<Vec<_>>().join(" ");
    // No leading dot (hidden, and how we tell our own files apart), no
    // trailing dot or space (Windows drops them).
    name = name.trim_start_matches('.').trim_end_matches(['.', ' ']).to_owned();
    if name.chars().count() > 80 {
        name = name.chars().take(80).collect::<String>().trim_end_matches(['.', ' ']).to_owned();
    }
    if name.is_empty() {
        return DESKTOP.to_owned();
    }
    let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.as_bytes()[3].is_ascii_digit());
    if device {
        name.push('_');
    }
    name
}

/// Characters no file or folder name can have (on Windows).
const FORBIDDEN: [char; 9] = ['/', '\\', ':', '*', '?', '"', '<', '>', '|'];

/// A path in `dir` for a file named `name` that isn't taken: "clip.mp4", else
/// "clip (2).mp4", "clip (3).mp4"…
pub fn free_path(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    if !path.exists() {
        return path;
    }
    let p = Path::new(name);
    let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = p.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    (2..).map(|n| dir.join(format!("{stem} ({n}){ext}"))).find(|p| !p.exists()).expect("a free name")
}

/// Reveal a file in the OS file manager, selecting it.
pub fn reveal_in_file_manager(path: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg("-R").arg(path).spawn()?;
    }
    #[cfg(target_os = "windows")]
    {
        // `explorer /select,"<path>"` highlights the file in its folder. Passed
        // raw: Explorer doesn't understand the switch quoted as a whole, which is
        // what normal argument quoting does to a path with spaces.
        use std::os::windows::process::CommandExt;
        std::process::Command::new("explorer")
            .raw_arg(format!("/select,\"{}\"", std::path::absolute(path)?.display()))
            .spawn()?;
    }
    #[cfg(target_os = "linux")]
    {
        // The file manager's D-Bus interface opens the folder with the file
        // selected (Files, Dolphin, Nemo, …); without one, just the folder.
        if show_items(path).is_err() {
            let dir = path.parent().unwrap_or(path);
            std::process::Command::new("xdg-open").arg(dir).spawn()?;
        }
    }
    #[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
    {
        let dir = path.parent().unwrap_or(path);
        std::process::Command::new("xdg-open").arg(dir).spawn()?;
    }
    Ok(())
}

/// `org.freedesktop.FileManager1.ShowItems`: show `path` selected.
#[cfg(target_os = "linux")]
fn show_items(path: &Path) -> Result<(), ashpd::zbus::Error> {
    let uri = file_uri(&std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()));
    pollster::block_on(async {
        let bus = ashpd::zbus::Connection::session().await?;
        bus.call_method(
            Some("org.freedesktop.FileManager1"),
            "/org/freedesktop/FileManager1",
            Some("org.freedesktop.FileManager1"),
            "ShowItems",
            &(vec![uri], ""),
        )
        .await
        .map(|_| ())
    })
}

/// A `file://` URI for an absolute path, with anything but plain characters
/// percent-encoded.
#[cfg(target_os = "linux")]
pub fn file_uri(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut uri = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
            uri.push(b as char);
        } else {
            uri.push_str(&format!("%{b:02X}"));
        }
    }
    uri
}

/// "Today" / "Yesterday" / "Tuesday 29 September" (year added when not this year).
pub fn day_label(day: NaiveDate) -> String {
    let today = Local::now().date_naive();
    if day == today {
        "Today".to_owned()
    } else if today.pred_opt() == Some(day) {
        "Yesterday".to_owned()
    } else if day.format("%Y").to_string() == today.format("%Y").to_string() {
        day.format("%A %-d %B").to_string()
    } else {
        day.format("%A %-d %B %Y").to_string()
    }
}

/// Friendly title for a file stem: "clip_2026-10-01_14-51-01" → "14:51:01",
/// "recording_…" → "Recording 14:51:01", and with what happened in the game
/// ("clip_… - 3 kills on Mirage") → "3 kills on Mirage · 14:51:01"; anything
/// else (a renamed clip) as is.
pub fn title_for_stem(stem: &str) -> String {
    let (kind, rest) = if let Some(r) = stem.strip_prefix("clip_") {
        ("", r)
    } else if let Some(r) = stem.strip_prefix("recording_") {
        ("Recording ", r)
    } else {
        return stem.to_owned();
    };
    match rest.get(..19).and_then(|ts| NaiveDateTime::parse_from_str(ts, "%Y-%m-%d_%H-%M-%S").ok()) {
        Some(ts) => {
            let suffix = rest.get(19..).unwrap_or("").trim();
            let time = ts.format("%H:%M:%S");
            if let Some(details) = suffix.strip_prefix("- ") {
                return format!("{details} · {kind}{time}");
            }
            if suffix.is_empty() { format!("{kind}{time}") } else { format!("{kind}{time} {suffix}") }
        }
        None => stem.to_owned(),
    }
}

/// A safe file name for a clip, e.g. a card title "14:51:01" → "14.51.01".
pub fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|c| if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { '.' } else { c })
        .collect::<String>()
        .trim()
        .trim_start_matches('.')
        .to_owned()
}

/// Check a name the user typed for a clip and turn it into a path next to
/// `existing` (same folder, same extension). `current` is the clip being renamed,
/// which may keep its own name.
pub fn path_for_name(dir: &Path, name: &str, ext: &str, current: Option<&Path>) -> Result<PathBuf, String> {
    let name = check_name(name, "Give the clip a name.")?;
    let path = dir.join(format!("{name}.{ext}"));
    if path.exists() && current != Some(path.as_path()) {
        return Err("A clip with that name already exists.".into());
    }
    Ok(path)
}

/// Check a new name the user typed for one of the library's folders, and
/// turn it into its path.
pub fn path_for_folder(lib: &Path, name: &str, current: &Path) -> Result<PathBuf, String> {
    let name = check_name(name, "Give the folder a name.")?;
    if name.ends_with('.') {
        return Err("Names can't end with a dot.".into());
    }
    let path = lib.join(name);
    // A change of case only is the same folder on Windows and macOS.
    let same = path.to_string_lossy().to_lowercase() == current.to_string_lossy().to_lowercase();
    if path.exists() && !same {
        return Err("A folder with that name already exists.".into());
    }
    Ok(path)
}

/// A name the user typed, trimmed, if a file or folder can have it.
fn check_name<'a>(name: &'a str, empty: &str) -> Result<&'a str, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(empty.into());
    }
    if name.starts_with('.') {
        return Err("Names can't start with a dot.".into());
    }
    if let Some(c) = name.chars().find(|c| FORBIDDEN.contains(c)) {
        return Err(format!("Names can't contain “{c}”."));
    }
    Ok(name)
}

/// Rename a clip. Its edit follows by id, so only the file moves.
pub fn rename(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::rename(from, to)
}

/// Open a file in the OS default application (video player).
pub fn open_in_default_app(path: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(path).spawn()?;
    }
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::UI::Shell::ShellExecuteW;
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        use windows::core::{HSTRING, w};
        let file = HSTRING::from(std::path::absolute(path)?.as_os_str());
        // SAFETY: plain strings in, no window handle; returns a pseudo-HINSTANCE
        // that's > 32 on success.
        let result = unsafe { ShellExecuteW(None, w!("open"), &file, None, None, SW_SHOWNORMAL) };
        if result.0 as isize <= 32 {
            return Err(std::io::Error::other("no app is set up to play this file"));
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open").arg(path).spawn()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_and_names() {
        assert_eq!(title_for_stem("clip_2026-10-01_14-51-01"), "14:51:01");
        assert_eq!(title_for_stem("recording_2026-10-01_14-51-01"), "Recording 14:51:01");
        assert_eq!(title_for_stem("clip_2026-10-01_14-51-01 (edit)"), "14:51:01 (edit)");
        assert_eq!(title_for_stem("Ace clutch"), "Ace clutch");
        assert_eq!(title_for_stem("clip_2026-10-01_14-51-01 - 3 kills on Mirage"), "3 kills on Mirage · 14:51:01");
        assert_eq!(title_for_stem("recording_2026-10-01_14-51-01 - Ace on Mirage (edit)"), "Ace on Mirage (edit) · Recording 14:51:01");
        assert_eq!(sanitize_name("14:51:01 (edit)"), "14.51.01 (edit)");
    }

    #[test]
    fn folder_names() {
        assert_eq!(folder_name("osu!"), "osu!");
        assert_eq!(folder_name("Counter-Strike: Global Offensive"), "Counter-Strike Global Offensive");
        assert_eq!(folder_name("  What?  Now... "), "What Now");
        assert_eq!(folder_name(".hack//G.U."), "hackG.U");
        assert_eq!(folder_name("CON"), "CON_");
        assert_eq!(folder_name("com3"), "com3_");
        assert_eq!(folder_name("Company"), "Company");
        assert_eq!(folder_name("???"), DESKTOP);
    }

    #[test]
    fn scans_game_folders() {
        let lib = std::env::temp_dir().join(format!("hc-scan-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&lib);
        std::fs::create_dir_all(lib.join("osu!")).unwrap();
        std::fs::create_dir_all(lib.join(".hesteclips").join("abcdef12")).unwrap();
        std::fs::write(lib.join("clip_2026-10-01_14-51-01.mp4"), b"x").unwrap();
        std::fs::write(lib.join("osu!").join("clip_2026-10-02_14-51-01.mp4"), b"x").unwrap();
        std::fs::write(lib.join("osu!").join(".clip_2026-10-02_14-52-01.mp4"), b"x").unwrap();
        std::fs::write(lib.join(".hesteclips").join("abcdef12").join("original.mp4"), b"x").unwrap();
        let found: Vec<(String, Option<String>)> = scan(&lib).into_iter().map(|c| (c.name, c.folder)).collect();
        assert_eq!(
            found,
            [("clip_2026-10-02_14-51-01.mp4".to_owned(), Some("osu!".to_owned())), ("clip_2026-10-01_14-51-01.mp4".to_owned(), None)]
        );
        assert_eq!(free_path(&lib, "clip_2026-10-01_14-51-01.mp4"), lib.join("clip_2026-10-01_14-51-01 (2).mp4"));
        assert_eq!(free_path(&lib, "new.mp4"), lib.join("new.mp4"));
        assert!(path_for_folder(&lib, "OSU!", &lib.join("osu!")).is_ok());
        assert!(path_for_folder(&lib, "osu!", &lib.join("Desktop")).is_err());
        std::fs::remove_dir_all(&lib).unwrap();
    }

    #[test]
    fn rename_validation() {
        let dir = std::env::temp_dir().join(format!("hc-rename-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("clip_2026-10-01_14-51-01.mp4");
        std::fs::write(&a, b"x").unwrap();
        std::fs::write(dir.join("Taken.mp4"), b"y").unwrap();

        assert!(path_for_name(&dir, "  ", "mp4", Some(&a)).is_err());
        assert!(path_for_name(&dir, ".hidden", "mp4", Some(&a)).is_err());
        assert!(path_for_name(&dir, "a/b", "mp4", Some(&a)).is_err());
        assert!(path_for_name(&dir, "Taken", "mp4", Some(&a)).is_err());
        let to = path_for_name(&dir, "Ace clutch", "mp4", Some(&a)).unwrap();
        rename(&a, &to).unwrap();
        assert!(to.exists() && !a.exists());
        // Renaming to its own name is allowed.
        assert!(path_for_name(&dir, "Ace clutch", "mp4", Some(&to)).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
