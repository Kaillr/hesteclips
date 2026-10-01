//! File naming for recordings, shared by the capture backends.

use std::path::{Path, PathBuf};

/// Hidden sibling a file is written to until it's complete: `dir/.name.ext`.
/// The leading dot hides it from Finder and from our clip scan; keeping the
/// extension keeps the container obvious.
pub fn in_progress(file: &Path) -> PathBuf {
    let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    file.with_file_name(format!(".{name}"))
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
        let Some(real) = name.strip_prefix('.') else { continue };
        let path = entry.path();
        if real.starts_with("recording_") && entry.metadata().is_ok_and(|m| m.len() > 0) {
            let target = dir.join(real);
            if !target.exists() && std::fs::rename(&path, &target).is_ok() {
                recovered.push(target);
            }
        } else if real.starts_with("clip_") {
            let _ = std::fs::remove_file(&path);
        }
    }
    recovered
}
