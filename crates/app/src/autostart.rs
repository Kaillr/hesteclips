//! Starting HesteClips with the computer: the user's startup list on Windows
//! (the registry's Run key, no admin needed), a login item (LaunchAgent) on
//! macOS, an autostart entry on Linux. It starts in the tray (`--background`).
//! The switch reads and writes that entry itself, so it always shows what's
//! really set, and nothing changes unless it's flipped.

/// The launch argument for starting in the tray.
pub const BACKGROUND: &str = "--background";

/// What the switch is called here.
pub fn label() -> &'static str {
    if cfg!(windows) {
        "Start with Windows"
    } else if cfg!(target_os = "macos") {
        "Open at login"
    } else {
        "Start when you log in"
    }
}

/// Whether HesteClips starts with the computer now.
pub fn is_on() -> bool {
    imp::is_on()
}

/// Start with the computer, or not.
pub fn set(on: bool) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    imp::set(on, &exe)
}

#[cfg(windows)]
mod imp {
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW};
    use windows::core::HSTRING;

    const KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const NAME: &str = "HesteClips";

    pub fn is_on() -> bool {
        let (key, name) = (HSTRING::from(KEY), HSTRING::from(NAME));
        // SAFETY: only asks whether the value exists (no buffer).
        unsafe { RegGetValueW(HKEY_CURRENT_USER, &key, &name, RRF_RT_REG_SZ, None, None, None) }.is_ok()
    }

    pub fn set(on: bool, exe: &std::path::Path) -> Result<(), String> {
        let (key, name) = (HSTRING::from(KEY), HSTRING::from(NAME));
        if !on {
            // SAFETY: plain strings.
            let r = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, &key, &name) };
            return if r.is_ok() || !is_on() { Ok(()) } else { Err(format!("couldn't remove it ({r:?})")) };
        }
        let command = format!("\"{}\" {}", exe.display(), super::BACKGROUND);
        let wide: Vec<u16> = command.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: the data is a NUL-terminated UTF-16 string, its size in bytes.
        let r = unsafe { RegSetKeyValueW(HKEY_CURRENT_USER, &key, &name, REG_SZ.0, Some(wide.as_ptr().cast()), (wide.len() * 2) as u32) };
        if r.is_ok() { Ok(()) } else { Err(format!("couldn't add it ({r:?})")) }
    }
}

#[cfg(target_os = "macos")]
mod imp {
    fn plist() -> Option<std::path::PathBuf> {
        Some(dirs::home_dir()?.join("Library").join("LaunchAgents").join("com.hesteclips.app.plist"))
    }

    pub fn is_on() -> bool {
        plist().is_some_and(|p| p.exists())
    }

    pub fn set(on: bool, exe: &std::path::Path) -> Result<(), String> {
        let path = plist().ok_or("no home folder")?;
        if !on {
            return match std::fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
                _ => Ok(()),
            };
        }
        let xml = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>com.hesteclips.app</string>\n<key>ProgramArguments</key><array><string>{}</string><string>{}</string></array>\n<key>RunAtLoad</key><true/>\n</dict></plist>\n",
            exe.display(),
            super::BACKGROUND
        );
        std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
        std::fs::write(path, xml).map_err(|e| e.to_string())
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod imp {
    fn entry() -> Option<std::path::PathBuf> {
        Some(dirs::config_dir()?.join("autostart").join("hesteclips.desktop"))
    }

    pub fn is_on() -> bool {
        entry().is_some_and(|p| p.exists())
    }

    pub fn set(on: bool, exe: &std::path::Path) -> Result<(), String> {
        let path = entry().ok_or("no config folder")?;
        if !on {
            return match std::fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
                _ => Ok(()),
            };
        }
        // KDE gives its screencast and window list to the first entry it finds
        // whose Exec is us, so this one asks for them too, like the app's own.
        let desktop = format!(
            "[Desktop Entry]\nType=Application\nName=HesteClips\nExec=\"{}\" {}\nX-GNOME-Autostart-enabled=true\nX-KDE-Wayland-Interfaces=zkde_screencast_unstable_v1,org_kde_plasma_window_management\n",
            exe.display(),
            super::BACKGROUND
        );
        std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
        std::fs::write(path, desktop).map_err(|e| e.to_string())
    }
}
