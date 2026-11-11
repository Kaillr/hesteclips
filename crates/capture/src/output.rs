//! File naming for recordings, shared by the capture backends.

use std::path::{Path, PathBuf};

/// The hidden folder next to a file where it's written until it's complete.
pub const WRITING: &str = ".writing";

/// Where a file is written until it's complete: `dir/.writing/name.ext`. The
/// folder is hidden from Finder and from our clip scan; the file keeps its
/// real name, so it never has a leading dot of its own. (In iCloud Drive,
/// macOS sometimes marks a dot-named file hidden a few seconds after it's
/// made, and the mark stays when it's renamed: clips written as `.name.ext`
/// came out hidden in Finder.) Makes the folder.
pub fn in_progress(file: &Path) -> PathBuf {
    let dir = file.parent().unwrap_or(Path::new(".")).join(WRITING);
    if std::fs::create_dir_all(&dir).is_ok() {
        mark_hidden(&dir);
    }
    dir.join(file.file_name().unwrap_or_default())
}

/// A leading dot hides a folder on macOS and Linux; Windows needs it marked.
#[cfg_attr(not(windows), allow(unused_variables))]
fn mark_hidden(path: &Path) {
    #[cfg(windows)]
    {
        use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_HIDDEN, FILE_FLAGS_AND_ATTRIBUTES, GetFileAttributesW, INVALID_FILE_ATTRIBUTES, SetFileAttributesW};
        use windows::core::HSTRING;
        let path = HSTRING::from(path.as_os_str());
        // SAFETY: a valid path string for both calls.
        unsafe {
            let current = GetFileAttributesW(&path);
            if current != INVALID_FILE_ATTRIBUTES && current & FILE_ATTRIBUTE_HIDDEN.0 == 0 {
                let _ = SetFileAttributesW(&path, FILE_FLAGS_AND_ATTRIBUTES(current | FILE_ATTRIBUTE_HIDDEN.0));
            }
        }
    }
}

/// A name for a new replay clip in `dir`: `clip_<timestamp>.<ext>`, with
/// `-2`, `-3`… when clips are saved within the same second (one may still be
/// being written, so its file can't be relied on to exist yet).
#[cfg_attr(not(any(target_os = "macos", target_os = "windows", target_os = "linux")), allow(dead_code))]
pub(crate) fn new_clip_path(dir: &Path, ext: &str) -> PathBuf {
    static LAST: std::sync::Mutex<Option<(String, u32)>> = std::sync::Mutex::new(None);
    let ts = timestamp();
    let mut last = LAST.lock().unwrap();
    let mut n = match &*last {
        Some((t, n)) if *t == ts => n + 1,
        _ => 1,
    };
    loop {
        let name = if n == 1 { format!("clip_{ts}.{ext}") } else { format!("clip_{ts}-{n}.{ext}") };
        let path = dir.join(name);
        if !path.exists() && !dir.join(WRITING).join(path.file_name().unwrap_or_default()).exists() {
            *last = Some((ts, n));
            return path;
        }
        n += 1;
    }
}

/// Where a finished recording goes: `file`'s name in `dir`, when one is given
/// and can be made (a game's folder), else where it is. Never over another file.
pub(crate) fn destination(file: &Path, dir: Option<&Path>) -> PathBuf {
    let moved = dir.zip(file.file_name()).map(|(dir, name)| dir.join(name));
    match moved {
        Some(to) if !to.exists() && to.parent().is_some_and(|d| std::fs::create_dir_all(d).is_ok()) => to,
        _ => file.to_path_buf(),
    }
}

/// Give a just-written file its real name. On Windows, an antivirus scan or the
/// search indexer often opens a new file for a moment, and renaming it then
/// fails ("access denied"): keep trying for a couple of seconds.
///
/// Into a folder on another drive (the clips folder changed during a
/// recording), a rename can't go: it's copied there and the original deleted.
pub(crate) fn finish_rename(from: &Path, to: &Path) -> std::io::Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) if std::time::Instant::now() < deadline && e.kind() == std::io::ErrorKind::PermissionDenied => {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => {
                std::fs::copy(from, to)?;
                return std::fs::remove_file(from);
            }
            Err(e) => return Err(e),
        }
    }
}

/// Local timestamp for filenames, e.g. "2026-08-13_14-32-05".
pub(crate) fn timestamp() -> String {
    chrono::Local::now().format("%Y-%m-%d_%H-%M-%S").to_string()
}

/// Recordings a previous run didn't get to finish (crash, force quit, power
/// loss) are still playable up to the last second — they're fragmented MP4. Give
/// them their real names so they show up in the library. Anything else left
/// half-written (a clip being saved, an edit being rendered, a screenshot) is
/// useless half-done, so it's removed. Returns the recovered files.
pub fn recover_unfinished(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut recovered = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if name == WRITING {
            recovered.extend(finish_writing_dir(dir));
            continue;
        }
        // Clips are saved straight into their game's folder: one cut off there
        // is removed the same way. (Recordings are written in the library itself.)
        if !name.starts_with('.') && entry.file_type().is_ok_and(|t| t.is_dir()) {
            let _ = finish_writing_dir(&path);
            remove_old_unfinished_clips(&path);
            continue;
        }
        // Before `.writing`, files were written as `.name.ext` next to their
        // place (0.4 and earlier).
        let Some(real) = name.strip_prefix('.') else { continue };
        // Only exact capture names: the editor keeps hidden files next to clips too
        // (`.recording_….mp4.edit.json`, `.….edited.mp4`), and those must stay hidden.
        if is_capture_name(real, "recording_") && entry.metadata().is_ok_and(|m| m.len() > 0) {
            let target = dir.join(real);
            if !target.exists() && std::fs::rename(&path, &target).is_ok() {
                recovered.push(target);
            }
        } else if is_capture_name(real, "clip_") {
            let _ = std::fs::remove_file(&path);
        }
    }
    recovered
}

/// `dir/.writing`: recordings in it go to `dir`, everything else is removed,
/// and so is the folder once it's empty. The recordings moved.
fn finish_writing_dir(dir: &Path) -> Vec<PathBuf> {
    let writing = dir.join(WRITING);
    let Ok(entries) = std::fs::read_dir(&writing) else { return Vec::new() };
    let mut recovered = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if is_capture_name(&name, "recording_") && entry.metadata().is_ok_and(|m| m.len() > 0) {
            let target = dir.join(&name);
            if !target.exists() && std::fs::rename(&path, &target).is_ok() {
                recovered.push(target);
                continue;
            }
        }
        let _ = if path.is_dir() { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
    }
    let _ = std::fs::remove_dir(&writing);
    recovered
}

fn remove_old_unfinished_clips(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.strip_prefix('.').is_some_and(|real| is_capture_name(real, "clip_")) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// `{prefix}2026-08-13_14-32-05.{ext}`: a file name the capture backends write,
/// with nothing after the container extension.
fn is_capture_name(name: &str, prefix: &str) -> bool {
    let Some(rest) = name.strip_prefix(prefix) else { return false };
    let Some((stamp, ext)) = rest.split_once('.') else { return false };
    chrono::NaiveDateTime::parse_from_str(stamp, "%Y-%m-%d_%H-%M-%S").is_ok() && matches!(ext, "mp4" | "mov" | "mkv")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovers_only_unfinished_captures() {
        let dir = std::env::temp_dir().join(format!("hesteclips-recover-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let hidden = [
            ".recording_2026-10-02_09-45-44.mp4",
            ".recording_2026-10-01_15-23-13.mp4.edit.json",
            ".recording_2026-10-01_15-23-13.mp4.edited.mp4",
            ".recording_2026-10-01_15-23-13.mp4.rendering.mp4",
            ".clip_2026-10-01_14-51-01.mp4.edit.json",
        ];
        for name in hidden {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        // The app's asset folder (edited clips' originals) is never touched.
        let assets = dir.join(".hesteclips").join("3f9c0000aaaa");
        std::fs::create_dir_all(&assets).unwrap();
        std::fs::write(assets.join("original.mp4"), b"x").unwrap();
        // A game's folder: a clip cut off while saving goes, finished ones stay.
        let game = dir.join("osu!");
        std::fs::create_dir_all(&game).unwrap();
        std::fs::write(game.join(".clip_2026-10-02_10-00-00.mp4"), b"x").unwrap();
        std::fs::write(game.join("clip_2026-10-02_09-59-00.mp4"), b"x").unwrap();
        std::fs::write(game.join(".clip_2026-10-02_09-59-00.mp4.edit.json"), b"x").unwrap();
        let recovered = recover_unfinished(&dir);
        assert_eq!(recovered, vec![dir.join("recording_2026-10-02_09-45-44.mp4")]);
        assert!(!game.join(".clip_2026-10-02_10-00-00.mp4").exists());
        assert!(game.join("clip_2026-10-02_09-59-00.mp4").exists());
        assert!(game.join(".clip_2026-10-02_09-59-00.mp4.edit.json").exists());
        for name in &hidden[1..] {
            assert!(dir.join(name).exists(), "{name} stays hidden");
        }
        assert!(assets.join("original.mp4").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recovers_from_the_writing_folder() {
        let dir = std::env::temp_dir().join(format!("hesteclips-recover-writing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // A recording cut off, and a clip, an edit and a screenshot half-written.
        let file = in_progress(&dir.join("recording_2026-10-22_14-00-00.mp4"));
        assert!(file.parent().unwrap().ends_with(WRITING));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&file, b"x").unwrap();
        std::fs::write(in_progress(&dir.join("clip_2026-10-22_14-01-00.mp4")), b"x").unwrap();
        std::fs::write(in_progress(&dir.join("deagle clutch.mp4")), b"x").unwrap();
        // The same in a game's folder.
        let game = dir.join("osu!");
        std::fs::create_dir_all(&game).unwrap();
        std::fs::write(in_progress(&game.join("clip_2026-10-22_14-02-00.mp4")), b"x").unwrap();
        std::fs::write(in_progress(&game.join("screenshot_2026-10-22_14-03-00.png")), b"x").unwrap();
        std::fs::write(game.join("clip_2026-10-22_13-00-00.mp4"), b"x").unwrap();

        let recovered = recover_unfinished(&dir);
        assert_eq!(recovered, vec![dir.join("recording_2026-10-22_14-00-00.mp4")]);
        assert!(!dir.join(WRITING).exists() && !game.join(WRITING).exists(), "nothing half-written is left");
        assert!(game.join("clip_2026-10-22_13-00-00.mp4").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saves_overlap() {
        let dir = std::env::temp_dir().join(format!("hesteclips-overlap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // The second is asked for before the first is written; its file is
        // only made after the first is done.
        let (a, b) = (dir.join("clip_a.mp4"), dir.join("clip_b.mp4"));
        let (pa, pb) = (in_progress(&a), in_progress(&b));
        std::fs::write(&pa, b"a").unwrap();
        finish_rename(&pa, &a).unwrap();
        std::fs::write(&pb, b"b").unwrap();
        finish_rename(&pb, &b).unwrap();
        assert_eq!((std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap()), (b"a".to_vec(), b"b".to_vec()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
