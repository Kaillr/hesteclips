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
        let recovered = recover_unfinished(&dir);
        assert_eq!(recovered, vec![dir.join("recording_2026-10-02_09-45-44.mp4")]);
        for name in &hidden[1..] {
            assert!(dir.join(name).exists(), "{name} stays hidden");
        }
        assert!(assets.join("original.mp4").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
