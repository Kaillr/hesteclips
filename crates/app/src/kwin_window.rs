//! Hiding the window in the tray, and bringing it back, on KDE Plasma under
//! Wayland. Wayland doesn't let an app hide or raise its own window (winit's
//! `set_visible` and `focus_window` do nothing there), so KWin is asked to,
//! through a script of its own: minimized and out of the taskbar, the task
//! switcher and the pager, as good as hidden; then back, and in front.
//!
//! The window is found by our process id. Scripts go through KWin's D-Bus
//! scripting interface, on a thread (it answers within milliseconds, but the
//! UI shouldn't wait on it).
//!
//! A minimized window gets no frames from KWin, so the app's UI loop doesn't
//! run while it's hidden: what must work then (the tray, another launch
//! asking for the window, the save shortcut) works from its own thread, and
//! brings the window back through here when it needs the UI.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

/// Whether it's hidden in the tray now.
static HIDDEN: AtomicBool = AtomicBool::new(false);

/// Whether KWin can do it here: a Wayland session on KDE Plasma.
pub fn available() -> bool {
    let plasma = std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|d| d.split(':').any(|d| d.eq_ignore_ascii_case("KDE")));
    plasma && crate::portal_shortcuts::wanted()
}

/// Hide the window in the tray.
pub fn hide() {
    HIDDEN.store(true, Ordering::Relaxed);
    show(false);
}

/// Bring the window back in front, if it's hidden (from any thread: the UI's
/// doesn't run meanwhile). Whether it was.
pub fn bring_back() -> bool {
    let was = HIDDEN.swap(false, Ordering::Relaxed);
    if was {
        show(true);
    }
    was
}

/// Bring it back regardless: in front, if it's behind others.
pub fn raise() {
    HIDDEN.store(false, Ordering::Relaxed);
    show(true);
}

/// Whether it's hidden in the tray now.
pub fn hidden() -> bool {
    HIDDEN.load(Ordering::Relaxed)
}

fn show(visible: bool) {
    let pid = std::process::id();
    let flags = if visible { "false" } else { "true" };
    let activate = if visible { "workspace.activeWindow = w;" } else { "" };
    let script = format!(
        "for (const w of workspace.windowList()) {{ if (w.pid === {pid} && w.normalWindow) {{ w.skipTaskbar = {flags}; w.skipSwitcher = {flags}; w.skipPager = {flags}; w.minimized = {flags}; {activate} }} }}"
    );
    std::thread::spawn(move || {
        if let Err(e) = pollster::block_on(run(&script)) {
            eprintln!("KWin couldn't {} the window: {e}", if visible { "show" } else { "hide" });
        }
    });
}

async fn run(script: &str) -> zbus::Result<()> {
    // KWin loads scripts from files: one of ours, gone once it's run.
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let path = dir.join(format!("hesteclips-window-{}.js", std::process::id()));
    std::fs::File::create(&path).and_then(|mut f| f.write_all(script.as_bytes())).map_err(|e| zbus::Error::Failure(e.to_string()))?;
    let conn = zbus::Connection::session().await?;
    let name = "hesteclips-window";
    let scripting = |method| (Some("org.kde.KWin"), "/Scripting", Some("org.kde.kwin.Scripting"), method);
    // A script of the same name still loaded (one run before) would be kept.
    let (dest, path_, iface, method) = scripting("unloadScript");
    let _ = conn.call_method(dest, path_, iface, method, &(name,)).await;
    let (dest, path_, iface, method) = scripting("loadScript");
    conn.call_method(dest, path_, iface, method, &(path.to_string_lossy().as_ref(), name)).await?;
    let (dest, path_, iface, method) = scripting("start");
    conn.call_method(dest, path_, iface, method, &()).await?;
    let (dest, path_, iface, method) = scripting("unloadScript");
    conn.call_method(dest, path_, iface, method, &(name,)).await?;
    let _ = std::fs::remove_file(&path);
    Ok(())
}
