//! Dragging clips out of the window on Linux under Wayland: the files offered
//! as `text/uri-list` through the window's own Wayland connection (winit's,
//! which it hands out as a raw display), started from the window's surface
//! with the serial of the press that began it, as Wayland asks.
//!
//! winit keeps its seat and data device to itself, so this binds its own on
//! a queue of its own on the same connection, from the first frame: its
//! pointer hears the same presses, which gives the serial. A thread
//! dispatches that queue and hands the file list to whoever the clips are
//! dropped on. X11 has no way in from outside winit's window: Copy clip.

use std::io::Write;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use wayland_client::backend::{Backend, ObjectId};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_data_device, wl_data_device_manager as manager, wl_data_offer, wl_data_source, wl_pointer, wl_registry, wl_seat, wl_surface};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum, delegate_noop, event_created_child};

/// What's set up on the window's connection.
struct Drag {
    conn: Connection,
    qh: QueueHandle<State>,
    manager: manager::WlDataDeviceManager,
    device: wl_data_device::WlDataDevice,
    surface: wl_surface::WlSurface,
    state: Arc<Mutex<Shared>>,
}

/// Shared with the thread dispatching our queue.
#[derive(Default)]
struct Shared {
    /// The last button press on our window: its serial, while it's held.
    press: Option<u32>,
    /// What the drag in progress offers.
    uris: Vec<u8>,
}

static DRAG: OnceLock<Option<Drag>> = OnceLock::new();

/// Get ready to drag out (from the first frame, so the press that starts a
/// drag is heard). Whether it can be done here: a Wayland session whose
/// compositor has a data device.
pub fn prepare(frame: &eframe::Frame) -> bool {
    DRAG.get_or_init(|| setup(frame).inspect_err(|e| eprintln!("dragging clips out: {e}")).ok()).is_some()
}

fn setup(frame: &eframe::Frame) -> Result<Drag, String> {
    let (RawDisplayHandle::Wayland(d), RawWindowHandle::Wayland(w)) = (
        frame.display_handle().map_err(|e| e.to_string())?.as_raw(),
        frame.window_handle().map_err(|e| e.to_string())?.as_raw(),
    ) else {
        return Err("not a Wayland window".into());
    };
    // SAFETY: winit's live wl_display, which outlives the app's frames.
    let backend = unsafe { Backend::from_foreign_display(d.display.as_ptr().cast()) };
    let conn = Connection::from_backend(backend);
    // SAFETY: the window's live wl_surface, on that display.
    let id = unsafe { ObjectId::from_ptr(wl_surface::WlSurface::interface(), w.surface.as_ptr().cast()) }.map_err(|e| e.to_string())?;
    let surface = wl_surface::WlSurface::from_id(&conn, id).map_err(|e| e.to_string())?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn).map_err(|e| e.to_string())?;
    let qh = queue.handle();
    let manager: manager::WlDataDeviceManager = globals.bind(&qh, 1..=3, ()).map_err(|_| "the compositor has no data device".to_string())?;
    let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=7, ()).map_err(|_| "no seat".to_string())?;
    let device = manager.get_data_device(&seat, &qh, ());
    let shared = Arc::new(Mutex::new(Shared::default()));
    let mut state = State { shared: shared.clone(), seat: seat.clone(), pointer: None };
    queue.roundtrip(&mut state).map_err(|e| e.to_string())?;
    std::thread::Builder::new()
        .name("wayland-drag".into())
        .spawn(move || while queue.blocking_dispatch(&mut state).is_ok() {})
        .map_err(|e| e.to_string())?;
    Ok(Drag { conn, qh, manager, device, surface, state: shared })
}

/// Whether dragging out was set up (`prepare`).
pub fn available() -> bool {
    matches!(DRAG.get(), Some(Some(_)))
}

/// Start dragging `files` out, from the press now held on the window.
pub fn start(frame: &eframe::Frame, files: &[PathBuf]) -> Result<(), String> {
    if !prepare(frame) {
        return Err("dragging clips out isn't available here: use Copy clip".into());
    }
    let d = DRAG.get().and_then(Option::as_ref).ok_or("not ready")?;
    let serial = {
        let mut s = d.state.lock().unwrap();
        s.uris = uri_list(files);
        s.press.ok_or("the press that started the drag wasn't heard")?
    };
    let source = d.manager.create_data_source(&d.qh, ());
    source.offer("text/uri-list".into());
    if source.version() >= 3 {
        source.set_actions(manager::DndAction::Copy);
    }
    d.device.start_drag(Some(&source), &d.surface, None, serial);
    d.conn.flush().map_err(|e| e.to_string())
}

/// `file://` URIs, one a line, as `text/uri-list` has them.
fn uri_list(files: &[PathBuf]) -> Vec<u8> {
    let mut out = Vec::new();
    for f in files {
        out.extend_from_slice(b"file://");
        for &b in std::os::unix::ffi::OsStrExt::as_bytes(f.as_os_str()) {
            if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
                out.push(b);
            } else {
                out.extend_from_slice(format!("%{b:02X}").as_bytes());
            }
        }
        out.extend_from_slice(b"\r\n");
    }
    out
}

struct State {
    shared: Arc<Mutex<Shared>>,
    seat: wl_seat::WlSeat,
    pointer: Option<wl_pointer::WlPointer>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(state: &mut Self, _: &wl_seat::WlSeat, event: wl_seat::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_seat::Event::Capabilities { capabilities: WEnum::Value(c) } = event
            && c.contains(wl_seat::Capability::Pointer)
            && state.pointer.is_none()
        {
            state.pointer = Some(state.seat.get_pointer(qh, ()));
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(state: &mut Self, _: &wl_pointer::WlPointer, event: wl_pointer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_pointer::Event::Button { serial, state: WEnum::Value(s), .. } = event {
            state.shared.lock().unwrap().press = (s == wl_pointer::ButtonState::Pressed).then_some(serial);
        }
    }
}

impl Dispatch<wl_data_source::WlDataSource, ()> for State {
    fn event(state: &mut Self, source: &wl_data_source::WlDataSource, event: wl_data_source::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            wl_data_source::Event::Send { fd, .. } => {
                let uris = state.shared.lock().unwrap().uris.clone();
                let fd: OwnedFd = fd;
                let _ = std::fs::File::from(fd).write_all(&uris);
            }
            wl_data_source::Event::Cancelled | wl_data_source::Event::DndFinished => source.destroy(),
            _ => {}
        }
    }
}

impl Dispatch<wl_data_device::WlDataDevice, ()> for State {
    fn event(_: &mut Self, _: &wl_data_device::WlDataDevice, event: wl_data_device::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        // Offers made to us (something dragged over the window) aren't taken.
        if let wl_data_device::Event::DataOffer { id } = event {
            id.destroy();
        }
    }

    event_created_child!(State, wl_data_device::WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (wl_data_offer::WlDataOffer, ()),
    ]);
}

delegate_noop!(State: ignore manager::WlDataDeviceManager);
delegate_noop!(State: ignore wl_data_offer::WlDataOffer);
delegate_noop!(State: ignore wl_surface::WlSurface);

#[cfg(test)]
mod tests {
    #[test]
    fn uris_are_escaped() {
        let got = super::uri_list(&["/home/me/My Clips/clip #1.mp4".into(), "/tmp/a.mp4".into()]);
        assert_eq!(got, b"file:///home/me/My%20Clips/clip%20%231.mp4\r\nfile:///tmp/a.mp4\r\n");
    }
}
