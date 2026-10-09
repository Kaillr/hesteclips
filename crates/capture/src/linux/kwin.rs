//! The screen on KDE Plasma's Wayland session, from KWin's own screencast
//! protocol (what Plasma's portal uses underneath): no dialog, and every
//! monitor is known, so they're listed in the app.
//!
//! KWin hands it (and its window list, `super::kwin_windows`) only to apps
//! whose `.desktop` file lists it under
//! `X-KDE-Wayland-Interfaces` (found by matching its `Exec=` to our
//! executable), which ours does (`linux_desktop.rs` in the app writes it,
//! pointing at wherever we run from). Plasma 6.8 and later let any app that
//! isn't sandboxed have it.
//!
//! KWin answers with a PipeWire node on the session's PipeWire, read like the
//! portal's (`super::screen`). It streams for as long as the Wayland
//! connection that asked lives, so that's kept open alongside, on its own
//! thread, which also notices when KWin stops the stream.

use std::os::fd::AsFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_output, wl_registry};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop};
use wayland_protocols_plasma::screencast::v1::client::{
    zkde_screencast_stream_unstable_v1 as stream, zkde_screencast_unstable_v1 as screencast,
};

/// `zkde_screencast_unstable_v1`'s pointer mode: drawn into the picture.
const POINTER_EMBEDDED: u32 = 2;

/// Whether this is a KDE Plasma Wayland session that lets us have its
/// screencast. Asked once: that doesn't change while the app runs (a new
/// `.desktop` file is only seen by an app started after it).
pub(crate) fn available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        let plasma = std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|d| d.split(':').any(|d| d.eq_ignore_ascii_case("KDE")));
        plasma && std::env::var_os("WAYLAND_DISPLAY").is_some() && Session::connect().is_ok()
    })
}

/// One monitor, as KWin describes it.
#[derive(Debug, Clone)]
pub(crate) struct Monitor {
    /// `kde:` and the output's name (`kde:DP-1`), which stays the same while
    /// it's plugged in.
    pub id: String,
    pub name: String,
    pub width: u32,
    pub height: u32,
}

/// The monitors, leftmost first. Empty if KWin can't be reached.
pub(crate) fn monitors() -> Vec<Monitor> {
    let Ok(session) = Session::connect() else { return Vec::new() };
    let mut outputs: Vec<&Output> = session.state.outputs.iter().collect();
    outputs.sort_by_key(|o| o.position);
    outputs
        .into_iter()
        .filter_map(|o| {
            let name = o.name.clone()?;
            let (w, h) = o.size?;
            let label = match &o.description {
                Some(d) if !d.trim().is_empty() && d.trim() != name => format!("{name} ({})", d.trim()),
                _ => name.clone(),
            };
            Some(Monitor { id: format!("kde:{name}"), name: label, width: w as u32, height: h as u32 })
        })
        .collect()
}

struct Output {
    wl: wl_output::WlOutput,
    name: Option<String>,
    description: Option<String>,
    size: Option<(i32, i32)>,
    position: (i32, i32),
}

#[derive(Default)]
struct State {
    outputs: Vec<Output>,
    /// The stream's PipeWire node, once KWin has made it.
    node: Option<u32>,
    /// Why KWin stopped (or never started) the stream.
    ended: Option<String>,
}

struct Session {
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
    screencast: screencast::ZkdeScreencastUnstableV1,
}

impl Session {
    fn connect() -> Result<Self> {
        let conn = Connection::connect_to_env().context("can't connect to the Wayland compositor")?;
        let (globals, mut queue) = registry_queue_init::<State>(&conn).context("the Wayland compositor didn't answer")?;
        let qh = queue.handle();
        let screencast: screencast::ZkdeScreencastUnstableV1 = globals
            .bind(&qh, 1..=4, ())
            .map_err(|_| anyhow!("KDE doesn't let this app record the screen directly (its .desktop file isn't known yet)"))?;
        let mut state = State::default();
        for g in globals.contents().clone_list() {
            if g.interface == wl_output::WlOutput::interface().name {
                let wl = globals.registry().bind::<wl_output::WlOutput, _, _>(g.name, g.version.min(4), &qh, ());
                state.outputs.push(Output { wl, name: None, description: None, size: None, position: (0, 0) });
            }
        }
        queue.roundtrip(&mut state).context("the Wayland compositor didn't answer")?;
        Ok(Self { conn, queue, state, screencast })
    }

    /// Wait for events and handle them, up to `deadline` (false if it passed).
    fn dispatch(&mut self, deadline: Instant) -> Result<bool> {
        self.queue.flush()?;
        if self.queue.dispatch_pending(&mut self.state)? > 0 {
            return Ok(true);
        }
        let Some(guard) = self.queue.prepare_read() else {
            self.queue.dispatch_pending(&mut self.state)?;
            return Ok(true);
        };
        let left = deadline.saturating_duration_since(Instant::now());
        let mut fd = libc::pollfd { fd: std::os::fd::AsRawFd::as_raw_fd(&self.conn.as_fd()), events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd.
        let n = unsafe { libc::poll(&mut fd, 1, left.as_millis().min(i32::MAX as u128) as i32) };
        if n > 0 {
            guard.read()?;
        }
        self.queue.dispatch_pending(&mut self.state)?;
        Ok(n > 0)
    }
}

/// A KWin stream of one monitor: its PipeWire node is live for as long as
/// this is held.
pub(crate) struct Cast {
    pub node: u32,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Cast {
    /// Ask KWin to stream the monitor with this id (the leftmost if there's
    /// none such). `ended` is called if KWin stops the stream.
    pub(crate) fn open(id: &str, ended: impl FnOnce(String) + Send + 'static) -> Result<Self> {
        let mut session = Session::connect()?;
        let want = id.strip_prefix("kde:");
        let mut outputs: Vec<&Output> = session.state.outputs.iter().filter(|o| o.size.is_some()).collect();
        outputs.sort_by_key(|o| o.position);
        let output = outputs.iter().find(|o| o.name.as_deref() == want).or(outputs.first()).context("the desktop has no monitor")?;
        let output = output.wl.clone();
        let qh = session.queue.handle();
        let stream = session.screencast.stream_output(&output, POINTER_EMBEDDED, &qh, ());
        let deadline = Instant::now() + Duration::from_secs(5);
        let node = loop {
            if let Some(node) = session.state.node {
                break node;
            }
            if let Some(why) = session.state.ended.take() {
                stream.close();
                bail!("KDE didn't start the screen stream: {why}");
            }
            if !session.dispatch(deadline)? && Instant::now() >= deadline {
                stream.close();
                bail!("KDE didn't start the screen stream in time");
            }
        };
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let thread = thread::Builder::new().name("kwin-cast".into()).spawn(move || {
            while !stop2.load(Ordering::Relaxed) {
                let next = Instant::now() + Duration::from_millis(200);
                if let Err(e) = session.dispatch(next) {
                    session.state.ended = Some(format!("lost the connection to KDE: {e}"));
                }
                if let Some(why) = session.state.ended.take() {
                    ended(why);
                    break;
                }
            }
            stream.close();
            let _ = session.conn.flush();
        })?;
        Ok(Self { node, stop, thread: Some(thread) })
    }
}

impl Drop for Cast {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

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

impl Dispatch<stream::ZkdeScreencastStreamUnstableV1, ()> for State {
    fn event(state: &mut Self, _: &stream::ZkdeScreencastStreamUnstableV1, event: stream::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            stream::Event::Created { node } => state.node = Some(node),
            stream::Event::Failed { error } => state.ended = Some(error),
            stream::Event::Closed => state.ended = Some("KDE stopped the screen stream".into()),
            _ => {}
        }
    }
}

delegate_noop!(State: ignore screencast::ZkdeScreencastUnstableV1);
