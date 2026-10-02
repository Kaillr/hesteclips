//! What the Windows backend asks the system: COM and Media Foundation setup,
//! the host clock, monitors, running processes, and the apps worth offering
//! as audio sources.

use std::collections::HashMap;
use std::sync::OnceLock;

use anyhow::{Result, anyhow};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplayMonitors, EnumDisplaySettingsW, GetMonitorInfoW, HDC, HMONITOR,
    MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MONITORINFOEXW, MonitorFromPoint,
};
use windows::Win32::Media::Audio::{
    DEVICE_STATE_ACTIVE, IAudioSessionControl2, IAudioSessionManager2, IMMDeviceEnumerator, MMDeviceEnumerator, eRender,
};
use windows::Win32::Media::MediaFoundation::{MF_VERSION, MFSTARTUP_FULL, MFStartup};
use windows::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GWL_EXSTYLE, GetWindow, GetWindowLongW, GetWindowTextLengthW, GetWindowThreadProcessId,
    IsWindowVisible, WS_EX_TOOLWINDOW,
};
use windows::core::{BOOL, Interface, PCWSTR};

use crate::Device;

/// Join the multithreaded COM apartment (if this thread isn't in one already —
/// a UI thread stays in its single-threaded one, which works just as well).
pub(crate) fn com_init() {
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
}

/// Start Media Foundation, once per process.
pub(crate) fn mf_startup() -> Result<()> {
    static STARTED: OnceLock<Result<(), String>> = OnceLock::new();
    STARTED
        .get_or_init(|| unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.map_err(|e| e.message()))
        .clone()
        .map_err(|e| anyhow!("Media Foundation isn't available ({e}) — on Windows N, install the Media Feature Pack"))
}

/// Host time in seconds: QueryPerformanceCounter, in the 100 ns units that
/// WASAPI and WGC timestamps use, so every source lands on one clock.
pub(crate) fn host_now() -> f64 {
    static FREQ: OnceLock<i64> = OnceLock::new();
    let freq = *FREQ.get_or_init(|| {
        let mut f = 0;
        let _ = unsafe { QueryPerformanceFrequency(&mut f) };
        f.max(1)
    });
    let mut now = 0;
    let _ = unsafe { QueryPerformanceCounter(&mut now) };
    (now as i128 * 10_000_000 / freq as i128) as f64 / 1e7
}

// ---------------------------------------------------------------------------
// Monitors
// ---------------------------------------------------------------------------

struct Monitor {
    handle: HMONITOR,
    /// GDI device name, e.g. `\\.\DISPLAY1` — stable while the setup is.
    device: String,
    primary: bool,
    width: u32,
    height: u32,
}

fn monitors() -> Vec<Monitor> {
    unsafe extern "system" fn collect(handle: HMONITOR, _: HDC, _: *mut RECT, out: LPARAM) -> BOOL {
        let out = unsafe { &mut *(out.0 as *mut Vec<HMONITOR>) };
        out.push(handle);
        true.into()
    }
    let mut handles: Vec<HMONITOR> = Vec::new();
    let _ = unsafe { EnumDisplayMonitors(None, None, Some(collect), LPARAM(&mut handles as *mut _ as isize)) };
    let mut out: Vec<Monitor> = handles
        .into_iter()
        .filter_map(|handle| {
            let mut info = MONITORINFOEXW::default();
            info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
            unsafe { GetMonitorInfoW(handle, &mut info as *mut _ as *mut MONITORINFO) }.ok().ok()?;
            let device = from_wide(&info.szDevice);
            // The mode's real pixels, whatever this process's DPI awareness.
            let mut mode = DEVMODEW { dmSize: std::mem::size_of::<DEVMODEW>() as u16, ..Default::default() };
            let (width, height) = if unsafe { EnumDisplaySettingsW(PCWSTR(info.szDevice.as_ptr()), ENUM_CURRENT_SETTINGS, &mut mode) }.as_bool() {
                (mode.dmPelsWidth, mode.dmPelsHeight)
            } else {
                let r = info.monitorInfo.rcMonitor;
                ((r.right - r.left) as u32, (r.bottom - r.top) as u32)
            };
            Some(Monitor { handle, device, primary: info.monitorInfo.dwFlags & 1 != 0, width, height })
        })
        .collect();
    // The main display first: it's the default choice.
    out.sort_by_key(|m| !m.primary);
    out
}

/// Screens to capture; ids are GDI device names.
pub fn list_screens() -> Vec<Device> {
    monitors()
        .into_iter()
        .map(|m| {
            let n = m.device.trim_start_matches(r"\\.\DISPLAY");
            let main = if m.primary { ", main" } else { "" };
            Device { name: format!("Display {n} ({}×{}{main})", m.width, m.height), id: m.device }
        })
        .collect()
}

/// The monitor with this device name, else the main one.
pub(crate) fn find_monitor(id: &str) -> HMONITOR {
    monitors()
        .into_iter()
        .find(|m| m.device == id)
        .map(|m| m.handle)
        .unwrap_or_else(|| unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) })
}

// ---------------------------------------------------------------------------
// Processes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub(crate) struct Process {
    pub pid: u32,
    pub parent: u32,
    /// Executable file name, e.g. `Discord.exe`.
    pub exe: String,
}

/// Every running process.
pub(crate) fn processes() -> Vec<Process> {
    let mut out = Vec::new();
    let Ok(snap) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) else { return out };
    let mut entry = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
    let mut ok = unsafe { Process32FirstW(snap, &mut entry) }.is_ok();
    while ok {
        out.push(Process { pid: entry.th32ProcessID, parent: entry.th32ParentProcessID, exe: from_wide(&entry.szExeFile) });
        ok = unsafe { Process32NextW(snap, &mut entry) }.is_ok();
    }
    let _ = unsafe { CloseHandle(snap) };
    out
}

/// Processes by pid, for walking up parent chains.
pub(crate) struct ProcessTree {
    by_pid: HashMap<u32, Process>,
}

impl ProcessTree {
    pub(crate) fn snapshot() -> Self {
        Self { by_pid: processes().into_iter().map(|p| (p.pid, p)).collect() }
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &Process> {
        self.by_pid.values()
    }

    pub(crate) fn get(&self, pid: u32) -> Option<&Process> {
        self.by_pid.get(&pid)
    }

    /// `pid`'s parent, grandparent, … (stops at a loop or after a sane depth,
    /// since pids get reused).
    pub(crate) fn ancestors(&self, pid: u32) -> impl Iterator<Item = &Process> {
        let mut cur = self.by_pid.get(&pid).map(|p| p.parent);
        let mut depth = 0;
        std::iter::from_fn(move || {
            let p = self.by_pid.get(&cur?)?;
            depth += 1;
            if depth > 32 || p.pid == 0 || Some(p.parent) == Some(p.pid) {
                cur = None;
            } else {
                cur = Some(p.parent);
            }
            (depth <= 32).then_some(p)
        })
    }

    /// Whether `pid` is `exe` or was started (directly or not) by it — what a
    /// process-tree capture of that app would include.
    pub(crate) fn belongs_to(&self, pid: u32, exe: &str) -> bool {
        self.get(pid).is_some_and(|p| p.exe.eq_ignore_ascii_case(exe)) || self.ancestors(pid).any(|p| p.exe.eq_ignore_ascii_case(exe))
    }

    /// The top processes of an app: those running `exe` whose parent isn't
    /// also `exe`. Capturing their trees captures the whole app once.
    pub(crate) fn roots_of(&self, exe: &str) -> Vec<u32> {
        let mut roots: Vec<u32> = self
            .iter()
            .filter(|p| p.exe.eq_ignore_ascii_case(exe))
            .filter(|p| !self.ancestors(p.pid).any(|a| a.exe.eq_ignore_ascii_case(exe)))
            .map(|p| p.pid)
            .collect();
        roots.sort_unstable();
        roots
    }
}

/// Processes that have an audio session on any output device — the ones that
/// play (or have played) sound.
pub(crate) fn audio_session_pids() -> Vec<u32> {
    let mut pids = Vec::new();
    let result = (|| -> windows::core::Result<()> {
        let enumerator: IMMDeviceEnumerator = unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
        let devices = unsafe { enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)? };
        for i in 0..unsafe { devices.GetCount()? } {
            let Ok(device) = (unsafe { devices.Item(i) }) else { continue };
            let Ok(manager) = (unsafe { device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) }) else { continue };
            let Ok(sessions) = (unsafe { manager.GetSessionEnumerator() }) else { continue };
            for k in 0..unsafe { sessions.GetCount()? } {
                let Ok(session) = (unsafe { sessions.GetSession(k) }) else { continue };
                let Ok(session) = session.cast::<IAudioSessionControl2>() else { continue };
                // S_OK (0) means it *is* the system sounds session.
                if unsafe { session.IsSystemSoundsSession() }.0 == 0 {
                    continue;
                }
                if let Ok(pid) = unsafe { session.GetProcessId() } {
                    if pid != 0 {
                        pids.push(pid);
                    }
                }
            }
        }
        Ok(())
    })();
    if let Err(e) = result {
        eprintln!("couldn't list audio sessions: {e}");
    }
    pids.sort_unstable();
    pids.dedup();
    pids
}

// ---------------------------------------------------------------------------
// Apps
// ---------------------------------------------------------------------------

/// Shell and system processes that own windows but aren't apps anyone records.
const NOT_APPS: [&str; 9] = [
    "explorer.exe",
    "ApplicationFrameHost.exe",
    "SystemSettings.exe",
    "TextInputHost.exe",
    "ShellExperienceHost.exe",
    "StartMenuExperienceHost.exe",
    "SearchHost.exe",
    "LockApp.exe",
    "ShellHost.exe",
];

/// Apps the user could add as an audio source: processes with a real window
/// or an audio session, by executable, named by their file description.
pub fn list_apps() -> Vec<Device> {
    com_init();
    unsafe extern "system" fn collect(hwnd: HWND, out: LPARAM) -> BOOL {
        let out = unsafe { &mut *(out.0 as *mut Vec<u32>) };
        unsafe {
            let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
            let owned = GetWindow(hwnd, GW_OWNER).is_ok_and(|o| !o.is_invalid());
            if IsWindowVisible(hwnd).as_bool() && !owned && ex_style & WS_EX_TOOLWINDOW.0 == 0 && GetWindowTextLengthW(hwnd) > 0 {
                let mut pid = 0;
                GetWindowThreadProcessId(hwnd, Some(&mut pid));
                out.push(pid);
            }
        }
        true.into()
    }
    let mut pids: Vec<u32> = Vec::new();
    let _ = unsafe { EnumWindows(Some(collect), LPARAM(&mut pids as *mut _ as isize)) };
    pids.extend(audio_session_pids());

    let own = std::process::id();
    let mut seen = std::collections::HashSet::new();
    let mut apps: Vec<Device> = pids
        .into_iter()
        .filter(|&pid| pid != own && pid != 0 && seen.insert(pid))
        .filter_map(|pid| {
            let path = image_path(pid)?;
            let exe = std::path::Path::new(&path).file_name()?.to_string_lossy().into_owned();
            if NOT_APPS.iter().any(|n| n.eq_ignore_ascii_case(&exe)) {
                return None;
            }
            let name = file_description(&path).unwrap_or_else(|| exe.trim_end_matches(".exe").to_owned());
            Some(Device { id: exe, name })
        })
        .collect();
    let own_exe = std::env::current_exe().ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));
    apps.retain(|a| own_exe.as_deref().is_none_or(|own| !a.id.eq_ignore_ascii_case(own)));
    apps.sort_by_key(|d| (d.name.to_lowercase(), d.id.to_lowercase()));
    apps.dedup_by(|a, b| a.id.eq_ignore_ascii_case(&b.id));
    apps
}

/// Full path of a process's executable.
fn image_path(pid: u32) -> Option<String> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, windows::core::PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(handle);
        ok.then(|| String::from_utf16_lossy(&buf[..len as usize]))
    }
}

/// The "File description" from an executable's version info — what Task
/// Manager shows ("Discord", "Spotify"), unlike the exe name.
fn file_description(path: &str) -> Option<String> {
    let wide: Vec<u16> = path.encode_utf16().chain([0]).collect();
    unsafe {
        let size = GetFileVersionInfoSizeW(PCWSTR(wide.as_ptr()), None);
        if size == 0 {
            return None;
        }
        let mut data = vec![0u8; size as usize];
        GetFileVersionInfoW(PCWSTR(wide.as_ptr()), None, size, data.as_mut_ptr().cast()).ok()?;
        let query = |key: &str| -> Option<(*const u8, u32)> {
            let key: Vec<u16> = key.encode_utf16().chain([0]).collect();
            let mut ptr = std::ptr::null_mut();
            let mut len = 0u32;
            VerQueryValueW(data.as_ptr().cast(), PCWSTR(key.as_ptr()), &mut ptr, &mut len)
                .as_bool()
                .then_some((ptr as *const u8, len))
                .filter(|(p, len)| !p.is_null() && *len > 0)
        };
        let (ptr, len) = query(r"\VarFileInfo\Translation")?;
        let translations = std::slice::from_raw_parts(ptr as *const u16, (len / 2) as usize);
        let candidates = translations.chunks_exact(2).map(|t| (t[0], t[1])).chain([(0x0409, 0x04B0), (0x0409, 0x04E4)]);
        for (lang, cp) in candidates {
            if let Some((ptr, len)) = query(&format!(r"\StringFileInfo\{lang:04x}{cp:04x}\FileDescription")) {
                let text = std::slice::from_raw_parts(ptr as *const u16, len as usize);
                let s = String::from_utf16_lossy(text).trim_end_matches('\0').trim().to_owned();
                if !s.is_empty() {
                    return Some(s);
                }
            }
        }
        None
    }
}

/// A NUL-terminated UTF-16 buffer as a String.
pub(crate) fn from_wide(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}
