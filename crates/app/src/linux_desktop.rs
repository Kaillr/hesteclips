//! Telling the Linux desktop about the app: a `.desktop` file and an icon.
//!
//! The desktop knows an app by its `.desktop` file: its name and icon in the
//! dock and app grid (Wayland matches the window to it by app id), and, for
//! the desktop portal, who's asking — GNOME won't keep global shortcuts for
//! an app it can't find one for. A package installs both system-wide; run
//! from anywhere else (an unpacked download, a build), the app puts its own
//! in `~/.local/share`, pointing at wherever it's being run from.

use std::path::{Path, PathBuf};

use crate::portal_shortcuts::APP_ID;

const DESKTOP_FILE: &str = include_str!("../assets/io.github.kaillr.HesteClips.desktop");
const ICON: &[u8] = include_bytes!("../assets/icon.svg");

/// Make sure the desktop can find us. Call before talking to the portal.
pub fn integrate() {
    let data_dirs = std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    let installed = data_dirs.split(':').filter(|d| !d.is_empty()).any(|d| Path::new(d).join("applications").join(format!("{APP_ID}.desktop")).is_file());
    if installed {
        return;
    }
    let Some(home) = data_home() else { return };
    let Ok(exe) = std::env::current_exe() else { return };
    let entry = DESKTOP_FILE.replace("Exec=hesteclips", &format!("Exec={}", quote_exec(&exe)));
    let desktop = home.join("applications").join(format!("{APP_ID}.desktop"));
    let icon = home.join("icons/hicolor/scalable/apps").join(format!("{APP_ID}.svg"));
    let wrote = write_if_changed(&desktop, entry.as_bytes()) | write_if_changed(&icon, ICON);
    if wrote {
        eprintln!("added HesteClips to the desktop's apps ({})", desktop.display());
    }
}

/// `$XDG_DATA_HOME`, or `~/.local/share`.
fn data_home() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
}

/// Write `bytes` to `path` unless it already holds them. Whether it wrote.
fn write_if_changed(path: &Path, bytes: &[u8]) -> bool {
    if std::fs::read(path).is_ok_and(|have| have == bytes) {
        return false;
    }
    let written = path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::write(path, bytes));
    if let Err(e) = &written {
        eprintln!("couldn't write {}: {e}", path.display());
    }
    written.is_ok()
}

/// A path as an `Exec=` argument: quoted when it needs to be, with the
/// characters the desktop entry spec reserves escaped.
fn quote_exec(path: &Path) -> String {
    let s = path.display().to_string();
    if !s.chars().any(|c| c.is_whitespace() || "\"'\\><~|&;$*?#()`".contains(c)) {
        return s;
    }
    let escaped: String = s.chars().flat_map(|c| if matches!(c, '"' | '`' | '$' | '\\') { vec!['\\', c] } else { vec![c] }).collect();
    // A backslash is itself escaped once more in the file's string syntax.
    format!("\"{}\"", escaped.replace('\\', "\\\\"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_paths_are_quoted_when_needed() {
        assert_eq!(quote_exec(Path::new("/opt/hesteclips/hesteclips")), "/opt/hesteclips/hesteclips");
        assert_eq!(quote_exec(Path::new("/home/a b/hesteclips")), "\"/home/a b/hesteclips\"");
        assert_eq!(quote_exec(Path::new("/x/$y")), "\"/x/\\\\$y\"");
    }
}
