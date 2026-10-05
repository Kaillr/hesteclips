//! The installed app's log. A release build on Windows has no console, so what
//! it prints (errors, what audio sources are doing, panics) would go nowhere:
//! it goes to `%LOCALAPPDATA%\hesteclips\logs\hesteclips.log` instead, for
//! someone to send when something doesn't work. Each launch starts it afresh;
//! the one before is kept as `hesteclips.previous.log`.

#![cfg_attr(not(windows), allow(dead_code))]

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
