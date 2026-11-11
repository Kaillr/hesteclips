//! KWin's window list (`org_kde_plasma_window_management`): every window's
//! process, title, state (which is active, which minimized) and where it is
//! (its frame, and its inside without title bar and borders), native
//! Wayland windows included, and the id KWin's screencast streams a window
//! by. Followed by a background thread from the first time it's asked, so
//! asking is instant.
//!
//! Restricted like KWin's screencast: our `.desktop` file asks for it (see
//! `super::kwin`). Plasma 6.8 and later give it to any app.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::thread;

use wayland_client::backend::ObjectId;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_registry;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, event_created_child};
use wayland_protocols_plasma::plasma_window_management::client::{
    org_kde_plasma_window as window, org_kde_plasma_window_management as management,
};

/// `org_kde_plasma_window_management.state`'s flags.
const ACTIVE: u32 = 0x1;
const MINIMIZED: u32 = 0x2;
const SKIP_TASKBAR: u32 = 0x1000;

/// A window, as KWin describes it.
#[derive(Debug, Clone, Default)]
pub(crate) struct Window {
    /// What KWin's screencast streams it by.
    pub uuid: String,
    pub pid: Option<u32>,
    pub title: String,
    /// Its Wayland app id (or X11 class): often its `.desktop` file's name.
    pub app_id: String,
    pub active: bool,
    pub minimized: bool,
    /// Not in the taskbar: a panel, a popup, a tooltip.
    pub skip_taskbar: bool,
    /// Where it is on the desktop, title bar and borders and all.
    pub frame: Option<Rect>,
    /// Where its inside is (the frame without title bar and borders), where
    /// KWin says (version 18 of its window list).
    pub inside: Option<Rect>,
}

/// A rectangle on the desktop, in its coordinates (logical pixels: a
/// window's picture is these times its display's scale).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// What's known: each window, by its object.
#[derive(Default)]
struct Known {
    windows: HashMap<ObjectId, Window>,
}

static KNOWN: Mutex<Option<Known>> = Mutex::new(None);

/// Whether KWin lets us follow its windows (started the first time it's asked).
pub(crate) fn following() -> bool {
    static STARTED: OnceLock<bool> = OnceLock::new();
    *STARTED.get_or_init(|| {
        let kde = std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|d| d.split(':').any(|d| d.eq_ignore_ascii_case("KDE")));
        kde && start()
    })
}

/// Every window, if KWin lets us follow them.
pub(crate) fn windows() -> Option<Vec<Window>> {
    if !following() {
        return None;
    }
    let known = KNOWN.lock().unwrap();
    Some(known.as_ref()?.windows.values().cloned().collect())
}

/// Window `uuid`'s size, as KWin says: its inside's, where it says that.
pub(crate) fn size(uuid: &str) -> Option<(u32, u32)> {
    let (frame, inside) = geometry(uuid)?;
    inside.or(frame).map(|r| (r.width, r.height))
}

/// Where window `uuid` is, title bar and borders and all, and where its
/// inside is, as far as KWin has said.
pub(crate) fn geometry(uuid: &str) -> Option<(Option<Rect>, Option<Rect>)> {
    if !following() {
        return None;
    }
    let known = KNOWN.lock().unwrap();
    let w = known.as_ref()?.windows.values().find(|w| w.uuid == uuid)?;
    Some((w.frame, w.inside))
}

/// The active window's process, if KWin lets us follow its windows.
pub(crate) fn active_pid() -> Option<u32> {
    windows()?.into_iter().find(|w| w.active).and_then(|w| w.pid)
}

/// Connect, and follow the windows on a thread. False if KWin won't say.
fn start() -> bool {
    let Ok(conn) = Connection::connect_to_env() else { return false };
    let Ok((globals, mut queue)) = registry_queue_init::<State>(&conn) else { return false };
    let qh = queue.handle();
    let Ok(_manager) = globals.bind::<management::OrgKdePlasmaWindowManagement, _, _>(&qh, 12..=18, ()) else { return false };
    *KNOWN.lock().unwrap() = Some(Known::default());
    let mut state = State;
    // The first answer lists the windows; the second, what each one is.
    if queue.roundtrip(&mut state).is_err() || queue.roundtrip(&mut state).is_err() {
        return false;
    }
    thread::Builder::new()
        .name("kwin-windows".into())
        .spawn(move || {
            let _manager = _manager;
            while queue.blocking_dispatch(&mut state).is_ok() {}
            *KNOWN.lock().unwrap() = None;
        })
        .is_ok()
}

struct State;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<management::OrgKdePlasmaWindowManagement, ()> for State {
    fn event(
        _: &mut Self,
        manager: &management::OrgKdePlasmaWindowManagement,
        event: management::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let management::Event::WindowWithUuid { uuid, .. } = event {
            let w = manager.get_window_by_uuid(uuid.clone(), qh, ());
            if let Some(k) = KNOWN.lock().unwrap().as_mut() {
                k.windows.insert(w.id(), Window { uuid, ..Default::default() });
            }
        }
    }

    event_created_child!(State, management::OrgKdePlasmaWindowManagement, [
        management::EVT_WINDOW_OPCODE => (window::OrgKdePlasmaWindow, ()),
    ]);
}

impl Dispatch<window::OrgKdePlasmaWindow, ()> for State {
    fn event(_: &mut Self, w: &window::OrgKdePlasmaWindow, event: window::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        let mut known = KNOWN.lock().unwrap();
        let Some(k) = known.as_mut() else { return };
        match event {
            window::Event::PidChanged { pid } => k.windows.entry(w.id()).or_default().pid = Some(pid),
            window::Event::TitleChanged { title } => k.windows.entry(w.id()).or_default().title = title,
            window::Event::AppIdChanged { app_id } => k.windows.entry(w.id()).or_default().app_id = app_id,
            window::Event::Geometry { x, y, width, height } => k.windows.entry(w.id()).or_default().frame = Some(Rect { x, y, width, height }),
            window::Event::ClientGeometry { x, y, width, height } => k.windows.entry(w.id()).or_default().inside = Some(Rect { x, y, width, height }),
            window::Event::StateChanged { flags } => {
                let win = k.windows.entry(w.id()).or_default();
                win.active = flags & ACTIVE != 0;
                win.minimized = flags & MINIMIZED != 0;
                win.skip_taskbar = flags & SKIP_TASKBAR != 0;
            }
            window::Event::Unmapped => {
                k.windows.remove(&w.id());
                w.destroy();
            }
            _ => {}
        }
    }
}
