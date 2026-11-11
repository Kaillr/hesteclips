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
/// The glyph without the macOS plate, as on Windows, at each size of the
/// icon theme's (made by scripts/icons.sh). PNGs, not an SVG: KDE draws an
/// SVG icon with Qt, which ignores paint-order (the glyph's keyline covered
/// it), and the bigger sizes' soft shadow is a blur no SVG icon can count on.
const ICONS: [(u32, &[u8]); 10] = [
    (16, include_bytes!("../assets/glyph-icon/16.png")),
    (22, include_bytes!("../assets/glyph-icon/22.png")),
    (24, include_bytes!("../assets/glyph-icon/24.png")),
    (32, include_bytes!("../assets/glyph-icon/32.png")),
    (48, include_bytes!("../assets/glyph-icon/48.png")),
    (64, include_bytes!("../assets/glyph-icon/64.png")),
    (96, include_bytes!("../assets/glyph-icon/96.png")),
    (128, include_bytes!("../assets/glyph-icon/128.png")),
    (192, include_bytes!("../assets/glyph-icon/192.png")),
    (256, include_bytes!("../assets/glyph-icon/256.png")),
];

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
    let mut wrote = write_if_changed(&desktop, entry.as_bytes());
    for (size, png) in ICONS {
        wrote |= write_if_changed(&home.join(format!("icons/hicolor/{size}x{size}/apps/{APP_ID}.png")), png);
    }
    // The SVG earlier versions put there: the desktop would draw it at any
    // size the PNGs don't have.
    let old = home.join("icons/hicolor/scalable/apps").join(format!("{APP_ID}.svg"));
    wrote |= std::fs::remove_file(old).is_ok();
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
        assert_eq!(quote_exec(Path::new("/opt/Kaillr/hesteclips")), "/opt/Kaillr/hesteclips");
        assert_eq!(quote_exec(Path::new("/home/a b/hesteclips")), "\"/home/a b/hesteclips\"");
        assert_eq!(quote_exec(Path::new("/x/$y")), "\"/x/\\\\$y\"");
    }
}
