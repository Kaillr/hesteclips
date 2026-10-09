//! KWin's window list (`org_kde_plasma_window_management`): which window is
//! active and its process, native Wayland windows included. Followed by a
//! background thread from the first time it's asked, so asking is instant.
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

/// `org_kde_plasma_window_management.state.active`.
const ACTIVE: u32 = 0x1;

/// What's known: each window's process and whether it's active.
#[derive(Default)]
struct Known {
    windows: HashMap<ObjectId, (Option<u32>, bool)>,
}

static KNOWN: Mutex<Option<Known>> = Mutex::new(None);

/// The active window's process, if KWin lets us follow its windows.
pub(crate) fn active_pid() -> Option<u32> {
    static STARTED: OnceLock<bool> = OnceLock::new();
    let following = *STARTED.get_or_init(|| {
        let kde = std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|d| d.split(':').any(|d| d.eq_ignore_ascii_case("KDE")));
        kde && start()
    });
    if !following {
        return None;
    }
    let known = KNOWN.lock().unwrap();
    known.as_ref()?.windows.values().find(|(_, active)| *active).and_then(|(pid, _)| *pid)
}

/// Connect, and follow the windows on a thread. False if KWin won't say.
fn start() -> bool {
    let Ok(conn) = Connection::connect_to_env() else { return false };
    let Ok((globals, mut queue)) = registry_queue_init::<State>(&conn) else { return false };
    let qh = queue.handle();
    let Ok(_manager) = globals.bind::<management::OrgKdePlasmaWindowManagement, _, _>(&qh, 12..=16, ()) else { return false };
    *KNOWN.lock().unwrap() = Some(Known::default());
    let mut state = State;
    if queue.roundtrip(&mut state).is_err() {
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
            let w = manager.get_window_by_uuid(uuid, qh, ());
            if let Some(k) = KNOWN.lock().unwrap().as_mut() {
                k.windows.insert(w.id(), (None, false));
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
            window::Event::PidChanged { pid } => k.windows.entry(w.id()).or_default().0 = Some(pid),
            window::Event::StateChanged { flags } => k.windows.entry(w.id()).or_default().1 = flags & ACTIVE != 0,
            window::Event::Unmapped => {
                k.windows.remove(&w.id());
                w.destroy();
            }
            _ => {}
        }
    }
}
