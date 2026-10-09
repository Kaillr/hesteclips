//! The installed app's log. A release build on Windows has no console, and on
//! Linux an app started from the app menu has nowhere to print, so what it
//! prints (errors, what audio sources are doing, panics) would go nowhere: it
//! goes to `%LOCALAPPDATA%\hesteclips\logs\hesteclips.log` on Windows,
//! `~/.local/share/hesteclips/logs/hesteclips.log` on Linux, for someone to
//! send when something doesn't work. Each launch starts it afresh; the one
//! before is kept as `hesteclips.previous.log`.

#![cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]

use std::path::PathBuf;

pub fn dir() -> Option<PathBuf> {
    dirs::data_local_dir().map(|d| d.join("hesteclips").join("logs"))
}

/// Send stderr to the log file, and note what's running where.
#[cfg(windows)]
pub fn start() {
    use std::os::windows::io::IntoRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Console::{STD_ERROR_HANDLE, SetStdHandle};

    let Some(dir) = dir() else { return };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join("hesteclips.log");
    let _ = std::fs::rename(&path, dir.join("hesteclips.previous.log"));
    let Ok(file) = std::fs::File::create(&path) else { return };
    // Rust's stderr asks Windows for the handle on every write, so this takes
    // over `eprintln!` and panic messages from here on. The file stays open
    // for the life of the process.
    let handle = HANDLE(file.into_raw_handle());
    // SAFETY: a valid, open file handle, never closed.
    if unsafe { SetStdHandle(STD_ERROR_HANDLE, handle) }.is_err() {
        return;
    }
    eprintln!(
        "HesteClips {} on {}, started {}",
        env!("CARGO_PKG_VERSION"),
        windows_version(),
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    );
}

/// Copy stderr into the log file, still printing it where it went (a
/// terminal, when started from one), and note what's running where.
#[cfg(target_os = "linux")]
pub fn start() {
    use std::io::{Read, Write};
    use std::os::fd::FromRawFd;

    let Some(dir) = dir() else { return };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join("hesteclips.log");
    let _ = std::fs::rename(&path, dir.join("hesteclips.previous.log"));
    let Ok(mut file) = std::fs::File::create(&path) else { return };
    // stderr becomes a pipe, read by a thread that writes each piece to the
    // file and to wherever stderr went before. Panics included, since they
    // go to stderr too.
    let mut fds = [0; 2];
    // SAFETY: a fresh pipe, both ends ours; the old stderr is duplicated
    // before it's replaced, and the write end is kept open as stderr.
    let old = unsafe {
        if libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) != 0 {
            return;
        }
        let old = libc::dup(libc::STDERR_FILENO);
        if old < 0 || libc::dup2(fds[1], libc::STDERR_FILENO) < 0 {
            libc::close(fds[0]);
            libc::close(fds[1]);
            return;
        }
        libc::close(fds[1]);
        std::fs::File::from_raw_fd(old)
    };
    // SAFETY: the read end, owned by the thread from here on.
    let mut pipe = unsafe { std::fs::File::from_raw_fd(fds[0]) };
    let mut old = old;
    let started = std::thread::Builder::new().name("log".into()).spawn(move || {
        let mut buf = [0u8; 8192];
        while let Ok(n) = pipe.read(&mut buf) {
            if n == 0 {
                break;
            }
            let _ = file.write_all(&buf[..n]);
            let _ = old.write_all(&buf[..n]);
        }
    });
    if started.is_err() {
        return;
    }
    eprintln!(
        "HesteClips {} on {} ({}), started {}",
        env!("CARGO_PKG_VERSION"),
        linux_version(),
        linux_session(),
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    );
}

/// "Fedora Linux 42 (KDE Plasma Desktop Edition)", from os-release.
#[cfg(target_os = "linux")]
fn linux_version() -> String {
    let text = std::fs::read_to_string("/etc/os-release").or_else(|_| std::fs::read_to_string("/usr/lib/os-release")).unwrap_or_default();
    text.lines()
        .find_map(|l| l.strip_prefix("PRETTY_NAME="))
        .map(|v| v.trim_matches('"').to_owned())
        .unwrap_or_else(|| "Linux".into())
}

/// "KDE on wayland": the desktop and session type, which decide how the
/// screen is recorded.
#[cfg(target_os = "linux")]
fn linux_session() -> String {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_else(|_| "unknown desktop".into());
    let session = std::env::var("XDG_SESSION_TYPE").unwrap_or_else(|_| "unknown session".into());
    format!("{desktop} on {session}")
}

/// `HESTECLIPS_CURSOR_LOG=1`: log the cursor ten times a second when it
/// changes — shown or not, which image, which app has focus — to find out why
/// it vanishes in some games while HesteClips runs.
#[cfg(windows)]
pub fn log_cursor() {
    if std::env::var_os("HESTECLIPS_CURSOR_LOG").is_none() {
        return;
    }
    std::thread::spawn(|| {
        let mut last = None;
        loop {
            let now = (capture::win_cursor_state(), capture::foreground_exe());
            if last.as_ref() != Some(&now) {
                let (cursor, app) = &now;
                let cursor = match cursor {
                    Some((true, image)) => format!("shown (image {image:#x})"),
                    Some((false, image)) => format!("HIDDEN (image {image:#x})"),
                    None => "unknown".to_owned(),
                };
                eprintln!("{} cursor {cursor}, focus: {}", chrono::Local::now().format("%H:%M:%S%.3f"), app.as_deref().unwrap_or("-"));
                last = Some(now);
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    });
}

/// "Windows 11 24H2 (build 26100.4061)", from the registry (the version APIs
/// say whatever the app's manifest claims to support).
#[cfg(windows)]
fn windows_version() -> String {
    use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW};
    use windows::core::{HSTRING, PCWSTR};

    let key = HSTRING::from(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion");
    let text = |name: &str| -> Option<String> {
        let mut buf = [0u16; 128];
        let mut size = (buf.len() * 2) as u32;
        let name = HSTRING::from(name);
        // SAFETY: the buffer and its size in bytes match.
        unsafe { RegGetValueW(HKEY_LOCAL_MACHINE, PCWSTR(key.as_ptr()), PCWSTR(name.as_ptr()), RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr().cast()), Some(&mut size)) }
            .ok()
            .ok()?;
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..len]))
    };
    let ubr = {
        let mut value = 0u32;
        let mut size = 4u32;
        let name = HSTRING::from("UBR");
        // SAFETY: a u32 and its size in bytes.
        let ok = unsafe { RegGetValueW(HKEY_LOCAL_MACHINE, PCWSTR(key.as_ptr()), PCWSTR(name.as_ptr()), RRF_RT_REG_DWORD, None, Some((&mut value as *mut u32).cast()), Some(&mut size)) }.is_ok();
        ok.then_some(value)
    };
    let build = text("CurrentBuild").unwrap_or_default();
    // The registry still says "Windows 10" on 11; the build tells them apart.
    let product = text("ProductName").unwrap_or_else(|| "Windows".into());
    let product = if build.parse::<u32>().is_ok_and(|b| b >= 22000) { product.replace("Windows 10", "Windows 11") } else { product };
    let release = text("DisplayVersion").unwrap_or_default();
    match ubr {
        Some(ubr) => format!("{product} {release} (build {build}.{ubr})"),
        None => format!("{product} {release} (build {build})"),
    }
}
