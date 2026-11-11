//! Windows on Wayland desktops that list and capture them with the ext
//! protocols (Sway 1.11 and later, and others built on wlroots): listed by
//! `ext_foreign_toplevel_list_v1` (an identifier that stays the same, a
//! title and an app id), captured one at a time with
//! `ext_image_copy_capture_v1` from a toplevel source, at the window's own
//! size, its pointer painted in, whatever covers it.
//!
//! The list has no states: which window is active, which minimized, comes
//! from the wlroots list (`super::wlr_windows`), matched by app id and title.
//! A window's handle only means something on the connection that got it, so
//! a capture makes its own connection and finds the window by identifier.

use std::collections::HashMap;
use std::os::fd::AsFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use wayland_client::backend::ObjectId;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_buffer, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop, event_created_child};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1 as toplevel, ext_foreign_toplevel_list_v1 as toplevel_list,
};
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_foreign_toplevel_image_capture_source_manager_v1 as source_manager, ext_image_capture_source_v1 as source,
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1 as frame, ext_image_copy_capture_manager_v1 as capture_manager, ext_image_copy_capture_session_v1 as session,
};

use super::screen::{Latest, Order};
use super::wlr::Map;

/// A window in the ext list.
#[derive(Debug, Clone, Default)]
pub(crate) struct Window {
    /// Stays the same for as long as the window is open: `ext:` and it is
    /// what it's captured by.
    pub id: String,
    pub title: String,
    pub app_id: String,
}

/// Each window by its handle, and its changes until `done`.
#[derive(Default)]
struct Known {
    windows: HashMap<ObjectId, Window>,
    pending: HashMap<ObjectId, Window>,
}

static KNOWN: Mutex<Option<Known>> = Mutex::new(None);

/// Whether this desktop lists its windows and can capture one.
pub(crate) fn available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        let Ok(conn) = Connection::connect_to_env() else { return false };
        let Ok((globals, _)) = registry_queue_init::<ListState>(&conn) else { return false };
        let has = |name: &str| globals.contents().with_list(|l| l.iter().any(|g| g.interface == name));
        has(toplevel_list::ExtForeignToplevelListV1::interface().name)
            && has(source_manager::ExtForeignToplevelImageCaptureSourceManagerV1::interface().name)
            && has(capture_manager::ExtImageCopyCaptureManagerV1::interface().name)
    })
}

/// Every window, if the desktop lists them (followed from the first time).
pub(crate) fn windows() -> Option<Vec<Window>> {
    static STARTED: OnceLock<bool> = OnceLock::new();
    if !*STARTED.get_or_init(start) {
        return None;
    }
    let known = KNOWN.lock().unwrap();
    Some(known.as_ref()?.windows.values().cloned().collect())
}

/// Connect and follow the list on a thread. False if there's no such list.
fn start() -> bool {
    let Ok(conn) = Connection::connect_to_env() else { return false };
    let Ok((globals, mut queue)) = registry_queue_init::<ListState>(&conn) else { return false };
    let qh = queue.handle();
    let Ok(list) = globals.bind::<toplevel_list::ExtForeignToplevelListV1, _, _>(&qh, 1..=1, ()) else { return false };
    *KNOWN.lock().unwrap() = Some(Known::default());
    let mut state = ListState;
    if queue.roundtrip(&mut state).is_err() || queue.roundtrip(&mut state).is_err() {
        return false;
    }
    thread::Builder::new()
        .name("ext-windows".into())
        .spawn(move || {
            let _list = list;
            while queue.blocking_dispatch(&mut state).is_ok() {}
            *KNOWN.lock().unwrap() = None;
        })
        .is_ok()
}

struct ListState;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for ListState {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<toplevel_list::ExtForeignToplevelListV1, ()> for ListState {
    fn event(_: &mut Self, _: &toplevel_list::ExtForeignToplevelListV1, event: toplevel_list::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let toplevel_list::Event::Toplevel { toplevel } = event
            && let Some(k) = KNOWN.lock().unwrap().as_mut()
        {
            k.pending.insert(toplevel.id(), Window::default());
        }
    }

    event_created_child!(ListState, toplevel_list::ExtForeignToplevelListV1, [
        toplevel_list::EVT_TOPLEVEL_OPCODE => (toplevel::ExtForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<toplevel::ExtForeignToplevelHandleV1, ()> for ListState {
    fn event(_: &mut Self, h: &toplevel::ExtForeignToplevelHandleV1, event: toplevel::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        let mut known = KNOWN.lock().unwrap();
        let Some(k) = known.as_mut() else { return };
        match event {
            toplevel::Event::Identifier { identifier } => pending(k, &h.id()).id = format!("ext:{identifier}"),
            toplevel::Event::Title { title } => pending(k, &h.id()).title = title,
            toplevel::Event::AppId { app_id } => pending(k, &h.id()).app_id = app_id,
            toplevel::Event::Done => {
                if let Some(w) = k.pending.remove(&h.id()) {
                    k.windows.insert(h.id(), w);
                }
            }
            toplevel::Event::Closed => {
                k.windows.remove(&h.id());
                k.pending.remove(&h.id());
                h.destroy();
            }
            _ => {}
        }
    }
}

/// A window's changes until `done`, starting from what's known of it.
fn pending<'a>(k: &'a mut Known, id: &ObjectId) -> &'a mut Window {
    let current = k.windows.get(id).cloned().unwrap_or_default();
    k.pending.entry(id.clone()).or_insert(current)
}

// ---------------------------------------------------------------------------
// Capture
// ---------------------------------------------------------------------------

/// A connection capturing one window.
pub(crate) struct Grab {
    conn: Connection,
    queue: EventQueue<CaptureState>,
    state: CaptureState,
    shm: wl_shm::WlShm,
    session: session::ExtImageCopyCaptureSessionV1,
    buffer: Option<Buffer>,
}

struct Buffer {
    key: (wl_shm::Format, u32, u32),
    wl: wl_buffer::WlBuffer,
    pool: wl_shm_pool::WlShmPool,
    map: Map,
}

impl Drop for Buffer {
    fn drop(&mut self) {
        self.wl.destroy();
        self.pool.destroy();
    }
}

#[derive(Default)]
struct CaptureState {
    /// The window's handle on this connection, once the list has it.
    wanted: String,
    handles: HashMap<ObjectId, (toplevel::ExtForeignToplevelHandleV1, Option<String>)>,
    /// What the session asks of buffers: their size and the formats it takes.
    size: Option<(u32, u32)>,
    formats: Vec<wl_shm::Format>,
    constraints_done: bool,
    stopped: bool,
    /// The frame being captured: ready, or failed.
    ready: bool,
    failed: Option<String>,
}

impl Grab {
    /// Connect and start a capture session of window `ext:<identifier>`.
    pub(crate) fn open(id: &str) -> Result<Self> {
        let identifier = id.strip_prefix("ext:").context("not an ext window")?;
        let conn = Connection::connect_to_env().context("can't connect to the Wayland compositor")?;
        let (globals, mut queue) = registry_queue_init::<CaptureState>(&conn).context("the Wayland compositor didn't answer")?;
        let qh = queue.handle();
        let list: toplevel_list::ExtForeignToplevelListV1 = globals.bind(&qh, 1..=1, ()).context("the desktop doesn't list its windows")?;
        let sources: source_manager::ExtForeignToplevelImageCaptureSourceManagerV1 = globals.bind(&qh, 1..=1, ()).context("the desktop can't capture a window")?;
        let captures: capture_manager::ExtImageCopyCaptureManagerV1 = globals.bind(&qh, 1..=1, ()).context("the desktop can't capture a window")?;
        let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).context("no shared memory")?;
        let mut state = CaptureState { wanted: identifier.to_owned(), ..Default::default() };
        queue.roundtrip(&mut state)?;
        queue.roundtrip(&mut state)?;
        let handle = state.handles.values().find(|(_, i)| i.as_deref() == Some(identifier)).map(|(h, _)| h.clone()).context("the window is gone")?;
        let src = sources.create_source(&handle, &qh, ());
        let session = captures.create_session(&src, capture_manager::Options::PaintCursors, &qh, ());
        // The source is only needed to make the session; the list, once the handle's found.
        src.destroy();
        list.stop();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !state.constraints_done {
            if state.stopped {
                bail!("the desktop won't capture the window");
            }
            dispatch(&conn, &mut queue, &mut state, deadline)?;
        }
        Ok(Self { conn, queue, state, shm, session, buffer: None })
    }

    /// Capture the next picture (the compositor sends it once the window
    /// changed) into `latest`, waiting for it until `stop`.
    fn grab(&mut self, latest: &Latest, stop: &AtomicBool) -> Result<()> {
        let qh = self.queue.handle();
        let (width, height) = self.state.size.context("the desktop didn't say the window's size")?;
        let format = [wl_shm::Format::Xrgb8888, wl_shm::Format::Argb8888, wl_shm::Format::Xbgr8888, wl_shm::Format::Abgr8888]
            .into_iter()
            .find(|f| self.state.formats.contains(f))
            .context("the desktop offers the window in a format that can't be recorded")?;
        let order = match format {
            wl_shm::Format::Xbgr8888 | wl_shm::Format::Abgr8888 => Order::Rgb,
            _ => Order::Bgr,
        };
        let key = (format, width, height);
        if self.buffer.as_ref().is_none_or(|b| b.key != key) {
            self.buffer = None;
            let size = width as usize * height as usize * 4;
            let map = Map::new(size)?;
            let pool = self.shm.create_pool(map.fd.as_fd(), size as i32, &qh, ());
            let wl = pool.create_buffer(0, width as i32, height as i32, (width * 4) as i32, format, &qh, ());
            self.buffer = Some(Buffer { key, wl, pool, map });
        }
        let buffer = self.buffer.as_ref().expect("made above");
        self.state.ready = false;
        self.state.failed = None;
        let f = self.session.create_frame(&qh, ());
        f.attach_buffer(&buffer.wl);
        f.damage_buffer(0, 0, width as i32, height as i32);
        f.capture();
        while !self.state.ready {
            if let Some(why) = self.state.failed.take() {
                f.destroy();
                bail!("the desktop couldn't capture the window: {why}");
            }
            if self.state.stopped {
                f.destroy();
                bail!("the window closed");
            }
            // Nothing new yet: the frame waits for the window to change.
            if stop.load(Ordering::Relaxed) {
                f.destroy();
                return Ok(());
            }
            let _ = dispatch(&self.conn, &mut self.queue, &mut self.state, Instant::now() + Duration::from_millis(100));
        }
        f.destroy();
        let pixels = buffer.map.bytes();
        let row = width as usize * 4;
        latest.fill(width, height, order, |data| {
            data.copy_from_slice(&pixels[..row * height as usize]);
            data.chunks_exact_mut(4).for_each(|p| p[3] = 255);
        });
        Ok(())
    }
}

impl Drop for Grab {
    fn drop(&mut self) {
        self.buffer = None;
        self.session.destroy();
        let _ = self.conn.flush();
    }
}

/// Capture the window into `latest` until `stop` or it closes: a frame at a
/// time, each sent once the window changed.
pub(crate) fn run(mut grab: Grab, _id: &str, latest: &Latest, stop: &AtomicBool) {
    while !stop.load(Ordering::Relaxed) {
        match grab.grab(latest, stop) {
            Ok(()) => {}
            Err(e) => {
                latest.end(format!("{e:#}"));
                return;
            }
        }
    }
}

/// Wait for events and handle them, up to `deadline` (an error if it passes).
fn dispatch(conn: &Connection, queue: &mut EventQueue<CaptureState>, state: &mut CaptureState, deadline: Instant) -> Result<()> {
    queue.flush()?;
    if queue.dispatch_pending(state)? > 0 {
        return Ok(());
    }
    let Some(guard) = queue.prepare_read() else {
        queue.dispatch_pending(state)?;
        return Ok(());
    };
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        bail!("timed out");
    }
    let mut fd = libc::pollfd { fd: std::os::fd::AsRawFd::as_raw_fd(&conn.as_fd()), events: libc::POLLIN, revents: 0 };
    // SAFETY: one valid pollfd.
    let n = unsafe { libc::poll(&mut fd, 1, left.as_millis().min(i32::MAX as u128) as i32) };
    if n > 0 {
        guard.read()?;
    }
    queue.dispatch_pending(state)?;
    Ok(())
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for CaptureState {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<toplevel_list::ExtForeignToplevelListV1, ()> for CaptureState {
    fn event(state: &mut Self, _: &toplevel_list::ExtForeignToplevelListV1, event: toplevel_list::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let toplevel_list::Event::Toplevel { toplevel } = event {
            state.handles.insert(toplevel.id(), (toplevel, None));
        }
    }

    event_created_child!(CaptureState, toplevel_list::ExtForeignToplevelListV1, [
        toplevel_list::EVT_TOPLEVEL_OPCODE => (toplevel::ExtForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<toplevel::ExtForeignToplevelHandleV1, ()> for CaptureState {
    fn event(state: &mut Self, h: &toplevel::ExtForeignToplevelHandleV1, event: toplevel::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            toplevel::Event::Identifier { identifier } => {
                if let Some(e) = state.handles.get_mut(&h.id()) {
                    e.1 = Some(identifier);
                }
            }
            toplevel::Event::Closed => {
                if state.handles.get(&h.id()).and_then(|e| e.1.as_deref()) == Some(state.wanted.as_str()) {
                    state.stopped = true;
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<session::ExtImageCopyCaptureSessionV1, ()> for CaptureState {
    fn event(state: &mut Self, _: &session::ExtImageCopyCaptureSessionV1, event: session::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            // A new set of constraints starts over.
            session::Event::BufferSize { width, height } => {
                state.size = Some((width, height));
                state.formats.clear();
            }
            session::Event::ShmFormat { format: WEnum::Value(f) } => state.formats.push(f),
            session::Event::Done => state.constraints_done = true,
            session::Event::Stopped => state.stopped = true,
            _ => {}
        }
    }
}

impl Dispatch<frame::ExtImageCopyCaptureFrameV1, ()> for CaptureState {
    fn event(state: &mut Self, _: &frame::ExtImageCopyCaptureFrameV1, event: frame::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            frame::Event::Ready => state.ready = true,
            frame::Event::Failed { reason } => state.failed = Some(format!("{reason:?}")),
            _ => {}
        }
    }
}

delegate_noop!(CaptureState: ignore source_manager::ExtForeignToplevelImageCaptureSourceManagerV1);
delegate_noop!(CaptureState: ignore source::ExtImageCaptureSourceV1);
delegate_noop!(CaptureState: ignore capture_manager::ExtImageCopyCaptureManagerV1);
delegate_noop!(CaptureState: ignore wl_shm::WlShm);
delegate_noop!(CaptureState: ignore wl_shm_pool::WlShmPool);
delegate_noop!(CaptureState: ignore wl_buffer::WlBuffer);
