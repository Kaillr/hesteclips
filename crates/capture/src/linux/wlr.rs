//! The screen on wlroots-style Wayland desktops (Sway, Hyprland, niri,
//! river, labwc, Wayfire…), through their screencopy protocol: any app may
//! copy a monitor's picture, so there's no dialog, and every monitor is
//! known, so they're listed in the app like on X11, Windows and macOS.
//!
//! Each picture is copied into shared memory we hand the compositor, the
//! pointer drawn in by the compositor. GNOME and KDE don't offer this
//! protocol; they use the portal (`super::portal`).

use std::os::fd::{AsFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop};
use wayland_protocols_wlr::screencopy::v1::client::{zwlr_screencopy_frame_v1 as frame, zwlr_screencopy_manager_v1 as manager};

use super::screen::{Latest, Order};

/// Whether this is a Wayland session whose desktop lets apps copy the
/// screen. Asked once: the desktop doesn't change while the app runs.
pub(crate) fn available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| std::env::var_os("WAYLAND_DISPLAY").is_some() && Session::connect().is_ok())
}

/// One monitor, as the compositor describes it.
#[derive(Debug, Clone)]
pub(crate) struct Monitor {
    /// `wl:` and the output's name (`wl:DP-1`), which stays the same while
    /// it's plugged in.
    pub id: String,
    /// The output's name and, when known, what it is (`DP-1 (Dell U2720Q)`).
    pub name: String,
    /// Its size in pixels.
    pub width: u32,
    pub height: u32,
    /// Where it is on the desktop, to put the leftmost first.
    pub x: i32,
    pub y: i32,
}

/// The monitors, leftmost first. Empty if the compositor can't be reached.
pub(crate) fn monitors() -> Vec<Monitor> {
    let mut list: Vec<Monitor> = Session::connect().map(|s| s.state.outputs.iter().filter_map(Output::monitor).collect()).unwrap_or_default();
    list.sort_by_key(|m| (m.x, m.y));
    list
}

/// What's known about one output.
struct Output {
    wl: wl_output::WlOutput,
    name: Option<String>,
    description: Option<String>,
    /// The current mode's size, in pixels.
    size: Option<(i32, i32)>,
    position: (i32, i32),
}

impl Output {
    fn monitor(&self) -> Option<Monitor> {
        let name = self.name.clone()?;
        let (w, h) = self.size?;
        let label = match &self.description {
            Some(d) if !d.is_empty() && *d != name => format!("{name} ({d})"),
            _ => name.clone(),
        };
        Some(Monitor { id: format!("wl:{name}"), name: label, width: w as u32, height: h as u32, x: self.position.0, y: self.position.1 })
    }
}

/// What the event handlers fill in.
#[derive(Default)]
struct State {
    outputs: Vec<Output>,
    /// The frame being copied: what the compositor wants for it, and how it went.
    frame: Frame,
}

#[derive(Default)]
struct Frame {
    /// The shared-memory buffer it wants: format, width, height, stride.
    buffer: Option<(wl_shm::Format, u32, u32, u32)>,
    /// Version 3 says when it's done listing buffer kinds.
    listed: bool,
    y_invert: bool,
    ready: bool,
    failed: bool,
}

/// A connection to the compositor, with its outputs and the protocol's manager.
struct Session {
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
    shm: wl_shm::WlShm,
    manager: manager::ZwlrScreencopyManagerV1,
}

impl Session {
    fn connect() -> Result<Self> {
        let conn = Connection::connect_to_env().context("can't connect to the Wayland compositor")?;
        let (globals, mut queue) = registry_queue_init::<State>(&conn).context("the Wayland compositor didn't answer")?;
        let qh = queue.handle();
        let manager: manager::ZwlrScreencopyManagerV1 =
            globals.bind(&qh, 1..=3, ()).map_err(|_| anyhow!("this desktop doesn't let apps copy the screen"))?;
        let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).context("no shared memory on this compositor")?;
        let mut state = State::default();
        for g in globals.contents().clone_list() {
            if g.interface == wl_output::WlOutput::interface().name {
                // Version 4 has the output's name; older ones only its size.
                let wl = globals.registry().bind::<wl_output::WlOutput, _, _>(g.name, g.version.min(4), &qh, ());
                state.outputs.push(Output { wl, name: None, description: None, size: None, position: (0, 0) });
            }
        }
        // Once for the outputs to describe themselves.
        queue.roundtrip(&mut state).context("the Wayland compositor didn't answer")?;
        Ok(Self { conn, queue, state, shm, manager })
    }
}

/// A connection reading one monitor.
pub(crate) struct Grab {
    session: Session,
    output: wl_output::WlOutput,
    /// The shared memory the compositor copies into, and its buffer, made
    /// for the size and format it last asked for.
    buffer: Option<Buffer>,
}

struct Buffer {
    key: (wl_shm::Format, u32, u32, u32),
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

impl Grab {
    /// Connect and find the monitor with this id (the leftmost if there's
    /// none such).
    pub(crate) fn open(id: &str) -> Result<Self> {
        let session = Session::connect()?;
        let want = id.strip_prefix("wl:");
        let mut outputs: Vec<&Output> = session.state.outputs.iter().filter(|o| o.size.is_some()).collect();
        outputs.sort_by_key(|o| o.position);
        let output = outputs.iter().find(|o| o.name.as_deref() == want).or(outputs.first()).context("the desktop has no monitor")?;
        let output = output.wl.clone();
        Ok(Self { session, output, buffer: None })
    }

    /// Copy the monitor, pointer included, into `latest`.
    fn grab(&mut self, latest: &Latest) -> Result<()> {
        let s = &mut self.session;
        let qh = s.queue.handle();
        s.state.frame = Frame::default();
        let frame = s.manager.capture_output(1, &self.output, &qh, ());
        let deadline = Instant::now() + Duration::from_secs(2);
        // The compositor first says what buffer it wants (version 3: then "done").
        while s.state.frame.buffer.is_none() || (frame.version() >= 3 && !s.state.frame.listed) {
            if s.state.frame.failed {
                frame.destroy();
                bail!("the desktop wouldn't copy the screen");
            }
            dispatch(&s.conn, &mut s.queue, &mut s.state, deadline)?;
        }
        let key = s.state.frame.buffer.expect("checked above");
        let (format, width, height, stride) = key;
        let order = match format {
            wl_shm::Format::Xrgb8888 | wl_shm::Format::Argb8888 => Order::Bgr,
            wl_shm::Format::Xbgr8888 | wl_shm::Format::Abgr8888 => Order::Rgb,
            f => {
                frame.destroy();
                bail!("the desktop offers the screen in a format that can't be recorded ({f:?})");
            }
        };
        if self.buffer.as_ref().is_none_or(|b| b.key != key) {
            self.buffer = None;
            let size = stride as usize * height as usize;
            let map = Map::new(size)?;
            let pool = s.shm.create_pool(map.fd.as_fd(), size as i32, &qh, ());
            let wl = pool.create_buffer(0, width as i32, height as i32, stride as i32, format, &qh, ());
            self.buffer = Some(Buffer { key, wl, pool, map });
        }
        let buffer = self.buffer.as_ref().expect("made above");
        frame.copy(&buffer.wl);
        while !s.state.frame.ready {
            if s.state.frame.failed {
                frame.destroy();
                bail!("the desktop couldn't copy the screen");
            }
            dispatch(&s.conn, &mut s.queue, &mut s.state, deadline)?;
        }
        frame.destroy();
        let (row, stride) = (width as usize * 4, stride as usize);
        let pixels = buffer.map.bytes();
        let invert = s.state.frame.y_invert;
        latest.fill(width, height, order, |data| {
            for (y, out) in data.chunks_exact_mut(row).enumerate() {
                let from = if invert { height as usize - 1 - y } else { y };
                out.copy_from_slice(&pixels[from * stride..from * stride + row]);
            }
        });
        Ok(())
    }
}

/// Wait for events and handle them, up to `deadline`.
fn dispatch(conn: &Connection, queue: &mut EventQueue<State>, state: &mut State, deadline: Instant) -> Result<()> {
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
        bail!("the desktop didn't copy the screen in time");
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

/// Copy monitor `id` into `latest` until `stop`: once at the start, then each
/// time a picture is wanted ([`Latest::want`]). If copying fails (the monitor
/// unplugged), the monitor is looked up again; the picture ends only if that
/// keeps failing.
pub(crate) fn run(mut grab: Grab, id: &str, latest: &Latest, stop: &AtomicBool) {
    let mut failing: Option<Instant> = None;
    let mut first = true;
    while !stop.load(Ordering::Relaxed) {
        if !std::mem::take(&mut first) && !latest.wait_wanted(Duration::from_millis(100)) {
            continue;
        }
        match grab.grab(latest) {
            Ok(()) => failing = None,
            Err(e) => {
                let since = *failing.get_or_insert_with(Instant::now);
                if since.elapsed() > Duration::from_secs(5) {
                    latest.end(format!("the screen can't be copied anymore: {e:#}"));
                    return;
                }
                std::thread::sleep(Duration::from_millis(250));
                if let Ok(g) = Grab::open(id) {
                    grab = g;
                }
                first = true;
            }
        }
    }
}

/// Anonymous shared memory, mapped.
struct Map {
    fd: OwnedFd,
    addr: *mut u8,
    size: usize,
}

// SAFETY: plain memory, used only by the thread that owns the Grab.
unsafe impl Send for Map {}

impl Map {
    fn new(size: usize) -> Result<Self> {
        // SAFETY: a fresh anonymous file, ours alone.
        let fd = unsafe { libc::memfd_create(c"hesteclips-screen".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            bail!("memfd_create: {}", std::io::Error::last_os_error());
        }
        // SAFETY: just made, owned from here on.
        let fd = unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };
        let raw = std::os::fd::AsRawFd::as_raw_fd(&fd);
        // SAFETY: sizing and mapping our own file.
        if unsafe { libc::ftruncate(raw, size as libc::off_t) } < 0 {
            bail!("ftruncate: {}", std::io::Error::last_os_error());
        }
        let addr = unsafe { libc::mmap(std::ptr::null_mut(), size, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, raw, 0) };
        if addr == libc::MAP_FAILED {
            bail!("mmap: {}", std::io::Error::last_os_error());
        }
        Ok(Self { fd, addr: addr.cast(), size })
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: mapped for `size` bytes for as long as self lives.
        unsafe { std::slice::from_raw_parts(self.addr, self.size) }
    }
}

impl Drop for Map {
    fn drop(&mut self) {
        // SAFETY: mapped in `new`, not used anymore.
        unsafe { libc::munmap(self.addr.cast(), self.size) };
    }
}

// ---------------------------------------------------------------------------
// Event handlers
// ---------------------------------------------------------------------------

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(state: &mut Self, wl: &wl_output::WlOutput, event: wl_output::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        let Some(o) = state.outputs.iter_mut().find(|o| o.wl == *wl) else { return };
        match event {
            wl_output::Event::Name { name } => o.name = Some(name),
            wl_output::Event::Description { description } => o.description = Some(description),
            wl_output::Event::Geometry { x, y, make, model, .. } => {
                o.position = (x, y);
                // Before version 4 there's no name: the model stands in.
                if o.description.is_none() {
                    o.description = Some(format!("{make} {model}").trim().to_owned());
                }
            }
            wl_output::Event::Mode { flags: WEnum::Value(f), width, height, .. } if f.contains(wl_output::Mode::Current) => {
                o.size = Some((width, height));
            }
            _ => {}
        }
    }
}

impl Dispatch<frame::ZwlrScreencopyFrameV1, ()> for State {
    fn event(state: &mut Self, _: &frame::ZwlrScreencopyFrameV1, event: frame::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        let f = &mut state.frame;
        match event {
            frame::Event::Buffer { format: WEnum::Value(format), width, height, stride } => f.buffer = Some((format, width, height, stride)),
            frame::Event::BufferDone => f.listed = true,
            frame::Event::Flags { flags: WEnum::Value(flags) } => f.y_invert = flags.contains(frame::Flags::YInvert),
            frame::Event::Ready { .. } => f.ready = true,
            frame::Event::Failed => f.failed = true,
            _ => {}
        }
    }
}

delegate_noop!(State: ignore manager::ZwlrScreencopyManagerV1);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
