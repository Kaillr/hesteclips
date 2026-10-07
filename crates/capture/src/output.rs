//! File naming for recordings, shared by the capture backends.

use std::path::{Path, PathBuf};

/// Hidden sibling a file is written to until it's complete: `dir/.name.ext`.
/// The leading dot hides it from Finder and from our clip scan; keeping the
/// extension keeps the container obvious.
pub fn in_progress(file: &Path) -> PathBuf {
    let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    file.with_file_name(format!(".{name}"))
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
        if !path.exists() && !in_progress(&path).exists() {
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
/// them their real names so they show up in the library. Clips being saved are
/// written in one go and are useless half-done, so those are removed. Returns the
/// recovered files.
pub fn recover_unfinished(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut recovered = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        // Clips are saved straight into their game's folder: one cut off there
        // is removed the same way. (Recordings are written in the library itself.)
        if !name.starts_with('.') && entry.file_type().is_ok_and(|t| t.is_dir()) {
            remove_unfinished_clips(&path);
            continue;
        }
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

fn remove_unfinished_clips(dir: &Path) {
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
}
