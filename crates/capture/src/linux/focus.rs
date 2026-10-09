//! Which app is in front, for telling the game being played (its name on
//! clips, per-game folders, Discord): the focused window's process, asked of
//! the desktop itself, then where that process's program is installed.
//!
//! - **X11, and XWayland on any Wayland desktop**: the X server says which
//!   window is focused (`_NET_ACTIVE_WINDOW`) and, through XRes, which
//!   process made it. Most games on Linux run through XWayland (Proton/Wine
//!   always does), so this finds them on GNOME and KDE too.
//! - **KDE Plasma**: KWin's window list has every window's process and which
//!   is active, native Wayland ones included. Restricted like its screencast
//!   (`super::kwin`): our `.desktop` file asks for it.
//! - **Sway, Hyprland and their kin**: their window list says which window is
//!   active and its app id, which is matched to a running process.
//! - **GNOME's native Wayland windows** can't be asked about (GNOME keeps that
//!   to its own portal). When the X server says a Wayland window is in front,
//!   a game is found among the running programs instead: the one game running.
//!
//! A Proton or Wine game's process is Wine; its Windows program is in its
//! command line (`Z:\home\…\Game.exe`), turned back into a path here, so it's
//! known by its `.exe`'s name and folder as on Windows.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use x11rb::connection::Connection as _;
use x11rb::protocol::res::{ClientIdMask, ClientIdSpec, ConnectionExt as _};
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _};
use x11rb::rust_connection::RustConnection;

use super::{kwin_windows, wlr_windows};

/// The app in focus: its id (the program's file name, `cs2.exe` or `hl2_linux`)
/// and where it's installed. `None` for us, or when it can't be told.
pub fn foreground_app_path() -> Option<(String, PathBuf)> {
    let pid = focused_pid()?;
    if pid == std::process::id() {
        return None;
    }
    let path = program_of(pid)?;
    Some((path.file_name()?.to_string_lossy().into_owned(), path))
}

/// What the desktop says about the focused window.
enum Focus {
    /// A window of this process.
    Pid(u32),
    /// A window, but whose can't be told (GNOME's Wayland windows).
    Unknown,
    /// Nothing can be asked.
    Unavailable,
}

/// The focused window's process.
fn focused_pid() -> Option<u32> {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    if wayland {
        // Asked of the compositor first: it knows its own windows and XWayland's.
        if let Some(pid) = kwin_windows::active_pid() {
            return Some(pid);
        }
        if let Some(app_id) = wlr_windows::active_app_id() {
            return pid_of_app_id(&app_id);
        }
    }
    match x11_focus() {
        Focus::Pid(pid) => Some(pid),
        // A Wayland window is in front and nothing says whose: the one game running.
        Focus::Unknown if wayland => running_game(),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// X11 / XWayland
// ---------------------------------------------------------------------------

/// The connection to the X server, kept between calls (asked every second).
static X11: Mutex<Option<(RustConnection, u32)>> = Mutex::new(None);

fn x11_focus() -> Focus {
    if std::env::var_os("DISPLAY").is_none() {
        return Focus::Unavailable;
    }
    let mut slot = X11.lock().unwrap();
    if slot.is_none() {
        let Ok((conn, screen)) = x11rb::connect(None) else { return Focus::Unavailable };
        let root = conn.setup().roots[screen].root;
        *slot = Some((conn, root));
    }
    let (conn, root) = slot.as_ref().unwrap();
    match x11_focus_on(conn, *root) {
        Some(focus) => focus,
        None => {
            // The connection may have broken (XWayland restarted): next time anew.
            *slot = None;
            Focus::Unavailable
        }
    }
}

fn x11_focus_on(conn: &RustConnection, root: u32) -> Option<Focus> {
    let atom = |name: &[u8]| conn.intern_atom(false, name).ok()?.reply().ok().map(|r| r.atom);
    let active = atom(b"_NET_ACTIVE_WINDOW")?;
    let reply = conn.get_property(false, root, active, AtomEnum::WINDOW, 0, 1).ok()?.reply().ok()?;
    let Some(window) = reply.value32().and_then(|mut v| v.next()).filter(|w| *w != 0) else { return Some(Focus::Unavailable) };
    // XRes asks the server which process the window's connection is: right
    // even when the window doesn't say (Wine's often don't).
    let spec = ClientIdSpec { client: window, mask: ClientIdMask::LOCAL_CLIENT_PID };
    let by_server = conn.res_query_client_ids(&[spec]).ok().and_then(|c| c.reply().ok()).and_then(|r| r.ids.into_iter().find_map(|id| id.value.first().copied()));
    let pid = by_server.or_else(|| {
        let wm_pid = atom(b"_NET_WM_PID")?;
        conn.get_property(false, window, wm_pid, AtomEnum::CARDINAL, 0, 1).ok()?.reply().ok()?.value32()?.next()
    });
    // A window of no process we can see, or of the compositor or XWayland
    // itself: on Wayland, the stand-in for "a Wayland window has focus" (GNOME's
    // is Mutter's, made by gnome-shell or mutter).
    Some(match pid.filter(|p| *p != 0 && !is_compositor(*p)) {
        Some(pid) => Focus::Pid(pid),
        None => Focus::Unknown,
    })
}

/// Whether a process is the desktop's compositor or XWayland, which own the
/// stand-in window for "a Wayland window has focus".
fn is_compositor(pid: u32) -> bool {
    std::fs::read_link(format!("/proc/{pid}/exe"))
        .is_ok_and(|p| p.file_name().is_some_and(|n| ["Xwayland", "gnome-shell", "mutter", "kwin_wayland"].iter().any(|c| n == *c)))
}

// ---------------------------------------------------------------------------
// Processes
// ---------------------------------------------------------------------------

/// The program a process runs: its executable, or for Wine/Proton the
/// Windows program it was started with.
pub(crate) fn program_of(pid: u32) -> Option<PathBuf> {
    let exe = std::fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    let name = exe.file_name()?.to_string_lossy().to_ascii_lowercase();
    let wine = name.starts_with("wine") || name.contains("preloader");
    if wine && let Some(windows) = windows_program(pid) {
        return Some(windows);
    }
    Some(exe)
}

/// A Wine process's Windows program: the first argument that's a Windows
/// path to an `.exe`, as a path here (Wine's `Z:` is `/`; other drives are
/// in the prefix's `dosdevices`).
fn windows_program(pid: u32) -> Option<PathBuf> {
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let arg = cmdline
        .split(|b| *b == 0)
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .find(|a| a.to_ascii_lowercase().ends_with(".exe"))?;
    unix_path(&arg, pid)
}

/// `Z:\home\me\Game\game.exe` → `/home/me/Game/game.exe`; `C:\…` through
/// the prefix's `dosdevices/c:` link; an already-unix path as it is.
fn unix_path(arg: &str, pid: u32) -> Option<PathBuf> {
    if arg.starts_with('/') {
        return Some(PathBuf::from(arg));
    }
    let bytes = arg.as_bytes();
    if bytes.len() < 3 || bytes[1] != b':' || !(bytes[2] == b'\\' || bytes[2] == b'/') {
        // Just a name (started from its own folder): the working directory has it.
        let cwd = std::fs::read_link(format!("/proc/{pid}/cwd")).ok()?;
        return Some(cwd.join(arg.replace('\\', "/")));
    }
    let drive = (bytes[0] as char).to_ascii_lowercase();
    let rest = arg[3..].replace('\\', "/");
    if drive == 'z' {
        return Some(Path::new("/").join(rest));
    }
    let prefix = std::env::var_os("WINEPREFIX")
        .map(PathBuf::from)
        .or_else(|| wine_prefix_of(pid))?;
    Some(prefix.join("dosdevices").join(format!("{drive}:")).join(rest))
}

/// The Wine prefix a process runs in, from its environment (ours isn't its).
fn wine_prefix_of(pid: u32) -> Option<PathBuf> {
    let env = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    env.split(|b| *b == 0).find_map(|e| e.strip_prefix(b"WINEPREFIX=")).map(|p| PathBuf::from(String::from_utf8_lossy(p).into_owned()))
}

/// Every process of ours (the same user), by id.
fn our_processes() -> Vec<u32> {
    // SAFETY: getuid can't fail.
    let me = unsafe { libc::getuid() };
    let Ok(dir) = std::fs::read_dir("/proc") else { return Vec::new() };
    dir.flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| std::fs::metadata(format!("/proc/{pid}")).is_ok_and(|m| std::os::unix::fs::MetadataExt::uid(&m) == me))
        .collect()
}

/// The process with this Wayland app id: its desktop file's name
/// (`org.mozilla.firefox`) or its program's (`firefox`), as Wayland apps set
/// it. The newest such process, the one most likely in front.
fn pid_of_app_id(app_id: &str) -> Option<u32> {
    let want = app_id.to_ascii_lowercase();
    let last = want.rsplit('.').next().unwrap_or(&want).to_owned();
    let mut found: Option<u32> = None;
    for pid in our_processes() {
        let Some(exe) = std::fs::read_link(format!("/proc/{pid}/exe")).ok() else { continue };
        let name = exe.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default().trim().to_ascii_lowercase();
        if name == want || name == last || comm == want || comm == last {
            found = Some(found.map_or(pid, |f| f.max(pid)));
        }
    }
    found
}

/// The one game running, when exactly one is: a program in a game store's
/// folder (Steam's `steamapps/common`, a Wine prefix's `Program Files` with
/// it…). Read at most every few seconds, since it looks at every process.
fn running_game() -> Option<u32> {
    static LAST: Mutex<Option<(Instant, Option<u32>)>> = Mutex::new(None);
    let mut last = LAST.lock().unwrap();
    if let Some((at, pid)) = *last
        && at.elapsed() < Duration::from_secs(5)
        && pid.is_none_or(|p| Path::new(&format!("/proc/{p}")).exists())
    {
        return pid;
    }
    let mut games: Vec<(u32, PathBuf)> = Vec::new();
    for pid in our_processes() {
        let Some(path) = program_of(pid) else { continue };
        if crate::stores::steam_dir(&path).is_some() && !games.iter().any(|(_, p)| *p == path) {
            games.push((pid, path));
        }
    }
    let pid = match games.as_slice() {
        [(pid, _)] => Some(*pid),
        _ => None,
    };
    *last = Some((Instant::now(), pid));
    pid
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wine_paths_become_unix_paths() {
        assert_eq!(unix_path(r"Z:\home\me\Games\osu!\osu!.exe", 1).unwrap(), PathBuf::from("/home/me/Games/osu!/osu!.exe"));
        assert_eq!(
            unix_path(r"Z:\home\me\.steam\steam\steamapps\common\Counter-Strike Global Offensive\game\bin\win64\cs2.exe", 1).unwrap(),
            PathBuf::from("/home/me/.steam/steam/steamapps/common/Counter-Strike Global Offensive/game/bin/win64/cs2.exe")
        );
        assert_eq!(unix_path("/usr/bin/game", 1).unwrap(), PathBuf::from("/usr/bin/game"));
        // SAFETY: a test of our own environment; nothing else reads it here.
        unsafe { std::env::set_var("WINEPREFIX", "/home/me/.wine") };
        assert_eq!(unix_path(r"C:\Program Files\Game\game.exe", 1).unwrap(), PathBuf::from("/home/me/.wine/dosdevices/c:/Program Files/Game/game.exe"));
    }

    #[test]
    fn steam_games_are_found_by_their_unix_path() {
        let path = unix_path(r"Z:\home\me\.local\share\Steam\steamapps\common\Some Game\bin\game.exe", 1).unwrap();
        assert_eq!(crate::stores::steam_dir(&path).map(|(_, d)| d).as_deref(), Some("Some Game"));
    }
}
