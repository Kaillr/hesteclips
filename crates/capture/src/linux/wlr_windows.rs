//! The window list of Sway, Hyprland and their kin
//! (`zwlr_foreign_toplevel_manager_v1`): which window is active and its app
//! id. Followed by a background thread from the first time it's asked, so
//! asking is instant. It has no process ids: `super::focus` finds the
//! process by the app id.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::thread;

use wayland_client::backend::ObjectId;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_registry;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, event_created_child};
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1 as handle, zwlr_foreign_toplevel_manager_v1 as manager,
};

/// Each window's app id and whether it's active, by its handle; the pending
/// changes are applied on `done`, as the protocol says.
#[derive(Default)]
struct Known {
    windows: HashMap<ObjectId, (String, bool)>,
    pending: HashMap<ObjectId, (Option<String>, Option<bool>)>,
}

static KNOWN: Mutex<Option<Known>> = Mutex::new(None);

/// The active window's app id, if the compositor lists its windows.
pub(crate) fn active_app_id() -> Option<String> {
    static STARTED: OnceLock<bool> = OnceLock::new();
    if !*STARTED.get_or_init(start) {
        return None;
    }
    let known = KNOWN.lock().unwrap();
    known.as_ref()?.windows.values().find(|(_, active)| *active).map(|(id, _)| id.clone()).filter(|id| !id.is_empty())
}

/// Connect, and follow the windows on a thread. False if there's no such list.
fn start() -> bool {
    let Ok(conn) = Connection::connect_to_env() else { return false };
    let Ok((globals, mut queue)) = registry_queue_init::<State>(&conn) else { return false };
    let qh = queue.handle();
    let Ok(manager) = globals.bind::<manager::ZwlrForeignToplevelManagerV1, _, _>(&qh, 1..=3, ()) else { return false };
    *KNOWN.lock().unwrap() = Some(Known::default());
    let mut state = State;
    if queue.roundtrip(&mut state).is_err() {
        return false;
    }
    thread::Builder::new()
        .name("wlr-windows".into())
        .spawn(move || {
            let _manager = manager;
            while queue.blocking_dispatch(&mut state).is_ok() {}
            *KNOWN.lock().unwrap() = None;
        })
        .is_ok()
}

struct State;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<manager::ZwlrForeignToplevelManagerV1, ()> for State {
    fn event(_: &mut Self, _: &manager::ZwlrForeignToplevelManagerV1, event: manager::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let manager::Event::Toplevel { toplevel } = event
            && let Some(k) = KNOWN.lock().unwrap().as_mut()
        {
            k.pending.insert(toplevel.id(), (None, None));
        }
    }

    event_created_child!(State, manager::ZwlrForeignToplevelManagerV1, [
        manager::EVT_TOPLEVEL_OPCODE => (handle::ZwlrForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<handle::ZwlrForeignToplevelHandleV1, ()> for State {
    fn event(_: &mut Self, h: &handle::ZwlrForeignToplevelHandleV1, event: handle::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        let mut known = KNOWN.lock().unwrap();
        let Some(k) = known.as_mut() else { return };
        match event {
            handle::Event::AppId { app_id } => k.pending.entry(h.id()).or_default().0 = Some(app_id),
            handle::Event::State { state } => {
                // An array of native-endian u32 states; 2 is "activated".
                let active = state.chunks_exact(4).any(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]) == handle::State::Activated as u32);
                k.pending.entry(h.id()).or_default().1 = Some(active);
            }
            handle::Event::Done => {
                if let Some((app_id, active)) = k.pending.remove(&h.id()) {
                    let entry = k.windows.entry(h.id()).or_default();
                    if let Some(app_id) = app_id {
                        entry.0 = app_id;
                    }
                    if let Some(active) = active {
                        entry.1 = active;
                    }
                }
            }
            handle::Event::Closed => {
                k.windows.remove(&h.id());
                k.pending.remove(&h.id());
                h.destroy();
            }
            _ => {}
        }
    }
}
