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

    /// A file that isn't in the library listing (e.g. an edited clip's original),
    /// for thumbnails and durations.
    pub fn at(path: &Path) -> Option<Clip> {
        let meta = std::fs::metadata(path).ok()?;
        Some(Clip {
            path: path.to_path_buf(),
            name: path.file_name()?.to_string_lossy().into_owned(),
            modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            size_bytes: meta.len(),
            id: None,
            original: None,
        })
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

/// Scan `dir` for video files, newest first. Missing dir → empty list (not an error).
pub fn scan(dir: &Path) -> Vec<Clip> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    // Only edited clips have ids; skip reading metadata when nothing's been edited.
    let any_edits = dir.join(crate::store::DIR).is_dir();

    let mut clips: Vec<Clip> = entries
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
            let id = if any_edits { crate::store::read_id(&path) } else { None };
            let original = id.as_deref().and_then(|id| crate::store::find_original(dir, id));
            Some(Clip {
                name: path.file_name()?.to_string_lossy().into_owned(),
                modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                size_bytes: meta.len(),
                id,
                original,
                path,
            })
        })
        .collect();

    clips.sort_by(|a, b| b.captured_at().cmp(&a.captured_at()));
    clips
}

/// Reveal a file in the OS file manager, selecting it.
pub fn reveal_in_file_manager(path: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg("-R").arg(path).spawn()?;
    }
    #[cfg(target_os = "windows")]
    {
        // `explorer /select,<path>` highlights the file in its folder.
        std::process::Command::new("explorer")
            .arg(format!("/select,{}", path.display()))
            .spawn()?;
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // No universal "select" on Linux; open the containing folder.
        let dir = path.parent().unwrap_or(path);
        std::process::Command::new("xdg-open").arg(dir).spawn()?;
    }
    Ok(())
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
/// "recording_…" → "Recording 14:51:01"; anything else (a renamed clip) as is.
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
    let name = name.trim();
    if name.is_empty() {
        return Err("Give the clip a name.".into());
    }
    if name.starts_with('.') {
        return Err("Names can't start with a dot.".into());
    }
    if let Some(c) = name.chars().find(|c| matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')) {
        return Err(format!("Names can't contain “{c}”."));
    }
    let path = dir.join(format!("{name}.{ext}"));
    if path.exists() && current != Some(path.as_path()) {
        return Err("A clip with that name already exists.".into());
    }
    Ok(path)
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
        std::process::Command::new("cmd").args(["/C", "start", ""]).arg(path).spawn()?;
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
        assert_eq!(sanitize_name("14:51:01 (edit)"), "14.51.01 (edit)");
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
