//! What's on screen, from the window server (CGWindowList): which app is in
//! front, its windows, and whether one is still showing. Cheap enough to ask
//! ten times a second (~1 ms); ScreenCaptureKit's own window list (slower,
//! and asynchronous) is only fetched when the window to record changes.
//!
//! Apps go by bundle id (`com.mojang.minecraftlauncher`), or by executable
//! name for the ones without a bundle (a game run from a folder: `java`,
//! `wine64-preloader`) — the same ids the audio sources use.

use objc2_app_kit::{NSApplicationActivationPolicy, NSRunningApplication, NSWorkspace};
use objc2_core_foundation::{CFArray, CFDictionary, CFNumber, CGRect, CFRetained, CFString, CFType};
use objc2_core_graphics::{
    CGRectMakeWithDictionaryRepresentation, CGWindowListCopyWindowInfo, CGWindowListOption, kCGNullWindowID, kCGWindowBounds, kCGWindowIsOnscreen,
    kCGWindowLayer, kCGWindowNumber, kCGWindowOwnerPID,
};

use crate::Device;

/// Windows smaller than this (points) aren't an app's picture: tool palettes,
/// status items, invisible helpers.
const MIN_SIDE: f64 = 120.0;

/// One window of an app, as the window server lists it (front to back).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Window {
    pub id: u32,
    pub pid: i32,
    pub on_screen: bool,
    pub width: f64,
    pub height: f64,
}

/// Every normal app window (layer 0, big enough), front to back; with
/// `on_screen_only`, only the ones showing.
pub(crate) fn windows(on_screen_only: bool) -> Vec<Window> {
    let mut option = CGWindowListOption::ExcludeDesktopElements;
    if on_screen_only {
        option |= CGWindowListOption::OptionOnScreenOnly;
    }
    let Some(list) = CGWindowListCopyWindowInfo(option, kCGNullWindowID) else { return Vec::new() };
    let list: CFRetained<CFArray<CFDictionary<CFString, CFType>>> = unsafe { CFRetained::cast_unchecked(list) };
    let own = std::process::id() as i32;
    let number = |d: &CFDictionary<CFString, CFType>, key: &CFString| d.get(key).and_then(|v| v.downcast::<CFNumber>().ok()).and_then(|n| n.as_i64());
    let mut out = Vec::new();
    for i in 0..list.len() {
        let Some(d) = list.get(i) else { continue };
        if number(&d, unsafe { kCGWindowLayer }) != Some(0) {
            continue;
        }
        let (Some(id), Some(pid)) = (number(&d, unsafe { kCGWindowNumber }), number(&d, unsafe { kCGWindowOwnerPID })) else { continue };
        if pid as i32 == own {
            continue;
        }
        let mut rect = CGRect::default();
        let bounds = d.get(unsafe { kCGWindowBounds }).and_then(|v| v.downcast::<CFDictionary>().ok());
        if !bounds.is_some_and(|b| unsafe { CGRectMakeWithDictionaryRepresentation(Some(&b), &mut rect) }) {
            continue;
        }
        if rect.size.width < MIN_SIDE || rect.size.height < MIN_SIDE {
            continue;
        }
        let on_screen = d.get(unsafe { kCGWindowIsOnscreen }).and_then(|v| v.downcast::<objc2_core_foundation::CFBoolean>().ok()).is_some_and(|b| b.as_bool());
        out.push(Window { id: id as u32, pid: pid as i32, on_screen, width: rect.size.width, height: rect.size.height });
    }
    out
}

fn id_of(app: &NSRunningApplication) -> Option<String> {
    app.bundleIdentifier()
        .map(|b| b.to_string())
        .or_else(|| app.executableURL().and_then(|u| u.lastPathComponent()).map(|n| n.to_string()))
}

/// An app's name for people: its localized name, else its id.
fn name_of(app: &NSRunningApplication) -> Option<String> {
    app.localizedName().map(|n| n.to_string()).or_else(|| id_of(app))
}

/// The app in front (the one with the keyboard).
pub(crate) fn frontmost() -> Option<(i32, String)> {
    let app = NSWorkspace::sharedWorkspace().frontmostApplication()?;
    let pid = app.processIdentifier();
    (pid != std::process::id() as i32).then(|| Some((pid, id_of(&app)?))).flatten()
}

/// The pids of running apps with this id (an app can run more than once).
pub(crate) fn pids_of(id: &str) -> Vec<i32> {
    NSWorkspace::sharedWorkspace()
        .runningApplications()
        .iter()
        .filter(|a| id_of(a).is_some_and(|i| i.eq_ignore_ascii_case(id)))
        .map(|a| a.processIdentifier())
        .collect()
}

/// The window to record for app `id`: its frontmost window that's showing
/// (the list is front to back), else its biggest one at all (a minimized
/// game keeps its last picture).
pub(crate) fn find_app_window(id: &str, all: &[Window]) -> Option<Window> {
    let pids = pids_of(id);
    let mine = || all.iter().filter(|w| pids.contains(&w.pid));
    mine().find(|w| w.on_screen).or_else(|| mine().max_by(|a, b| (a.width * a.height).total_cmp(&(b.width * b.height)))).copied()
}

/// Running apps with a window, for games and apps capture, sorted by name.
pub fn list_windowed_apps() -> Vec<Device> {
    let pids: std::collections::HashSet<i32> = windows(false).iter().map(|w| w.pid).collect();
    let mut apps: Vec<Device> = NSWorkspace::sharedWorkspace()
        .runningApplications()
        .iter()
        .filter(|a| pids.contains(&a.processIdentifier()))
        // Apps with a Dock icon, or a bare process with no bundle (a game run
        // from a folder). Agents with a window (menu bar apps' popovers,
        // helpers, system UI like loginwindow) aren't apps to record.
        .filter(|a| a.activationPolicy() == NSApplicationActivationPolicy::Regular || a.bundleIdentifier().is_none())
        .filter_map(|a| Some(Device { id: id_of(&a)?, name: name_of(&a)? }))
        .filter(|d| d.id != "com.apple.finder")
        .collect();
    apps.sort_by_key(|d| (d.name.to_lowercase(), d.id.to_lowercase()));
    apps.dedup_by(|a, b| a.id.eq_ignore_ascii_case(&b.id));
    apps
}

/// The app in front's id.
pub fn foreground_exe() -> Option<String> {
    frontmost().map(|(_, id)| id)
}

/// A running app's bundle's file name (`World of Warcraft.app`), else its
/// executable's.
pub(crate) fn bundle_file_name(id: &str) -> Option<String> {
    let apps = NSWorkspace::sharedWorkspace().runningApplications();
    let app = apps.iter().find(|a| id_of(a).is_some_and(|i| i.eq_ignore_ascii_case(id)))?;
    app.bundleURL().or_else(|| app.executableURL()).and_then(|u| u.lastPathComponent()).map(|n| n.to_string())
}
