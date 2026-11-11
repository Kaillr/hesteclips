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
//!
//! KWin leaves the windows of the app that asks out of a monitor's stream
//! (not out of a window's). Ours should be in the recording like any other
//! app's, so a monitor's stream is asked for by a helper: this app started
//! again (`helper`), whose only connection is the one that asks.

use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsFd;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_output, wl_registry};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop};
use wayland_protocols_plasma::output_order::v1::client::kde_output_order_v1 as output_order;
use wayland_protocols_plasma::screencast::v1::client::{
    zkde_screencast_stream_unstable_v1 as stream, zkde_screencast_unstable_v1 as screencast,
};

/// `zkde_screencast_unstable_v1`'s pointer mode: drawn into the picture.
const POINTER_EMBEDDED: u32 = 2;

/// What the app is started with to be the helper (`helper`), then the
/// monitor's id.
const HELPER_ARG: &str = "--kde-screen-stream";

/// Whether this program can be started as the helper: it called `helper`
/// when it started. (One that never does, an example, asks itself.)
static HELPS: AtomicBool = AtomicBool::new(false);

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
    /// Its refresh rate, in Hz (rounded), if KWin says.
    pub refresh: Option<u32>,
    /// The main monitor: first in Plasma's display settings.
    pub primary: bool,
}

/// The monitors, the main one first, then leftmost first. Empty if KWin
/// can't be reached.
pub(crate) fn monitors() -> Vec<Monitor> {
    let Ok(session) = Session::connect() else { return Vec::new() };
    let mut outputs: Vec<&Output> = session.state.outputs.iter().collect();
    outputs.sort_by_key(|o| session.state.rank(o));
    let main = session.state.order.first();
    outputs
        .into_iter()
        .filter_map(|o| {
            let name = o.name.clone()?;
            // KWin streams a turned monitor turned: as tall as it's shown.
            let (w, h) = o.size.map(|(w, h)| if o.sideways { (h, w) } else { (w, h) })?;
            let label = match &o.description {
                Some(d) if !d.trim().is_empty() && d.trim() != name => format!("{name} ({})", d.trim()),
                _ => name.clone(),
            };
            let primary = main == Some(&name);
            let refresh = o.refresh.filter(|r| *r > 0).map(|r| ((r + 500) / 1000) as u32);
            Some(Monitor { id: format!("kde:{name}"), name: label, width: w as u32, height: h as u32, refresh, primary })
        })
        .collect()
}

struct Output {
    wl: wl_output::WlOutput,
    name: Option<String>,
    description: Option<String>,
    /// The current mode's size, in the panel's own pixels (unturned).
    size: Option<(i32, i32)>,
    position: (i32, i32),
    /// The current mode's refresh rate, in mHz.
    refresh: Option<i32>,
    /// Turned a quarter (a portrait monitor): shown, and streamed, with its
    /// sides swapped.
    sideways: bool,
}

#[derive(Default)]
struct State {
    outputs: Vec<Output>,
    /// The outputs' names in Plasma's order (its display settings' priority),
    /// the main one first. Empty if KWin doesn't say.
    order: Vec<String>,
    /// The order being announced, until KWin says it's complete.
    next_order: Vec<String>,
    /// The stream's PipeWire node, once KWin has made it.
    node: Option<u32>,
    /// Why KWin stopped (or never started) the stream.
    ended: Option<String>,
}

impl State {
    /// Where an output sorts: by Plasma's order, then leftmost first.
    fn rank(&self, o: &Output) -> (usize, (i32, i32)) {
        let at = o.name.as_ref().and_then(|n| self.order.iter().position(|m| m == n));
        (at.unwrap_or(usize::MAX), o.position)
    }
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
        // Only to know which monitor is the main one, so it's fine without.
        let _order: Option<output_order::KdeOutputOrderV1> = globals.bind(&qh, 1..=1, ()).ok();
        let mut state = State::default();
        for g in globals.contents().clone_list() {
            if g.interface == wl_output::WlOutput::interface().name {
                let wl = globals.registry().bind::<wl_output::WlOutput, _, _>(g.name, g.version.min(4), &qh, ());
                state.outputs.push(Output { wl, name: None, description: None, size: None, position: (0, 0), refresh: None, sideways: false });
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

    /// The monitor with this id, else the main one.
    fn output(&self, id: &str) -> Result<wl_output::WlOutput> {
        let want = id.strip_prefix("kde:");
        let mut outputs: Vec<&Output> = self.state.outputs.iter().filter(|o| o.size.is_some()).collect();
        outputs.sort_by_key(|o| self.state.rank(o));
        let output = outputs.iter().find(|o| o.name.as_deref() == want).or(outputs.first()).context("the desktop has no monitor")?;
        Ok(output.wl.clone())
    }

    /// Wait (up to 5 seconds) for KWin to make `stream`'s PipeWire node.
    fn wait_for_node(&mut self, stream: &stream::ZkdeScreencastStreamUnstableV1) -> Result<u32> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(node) = self.state.node {
                return Ok(node);
            }
            if let Some(why) = self.state.ended.take() {
                stream.close();
                bail!("KDE didn't start the screen stream: {why}");
            }
            if !self.dispatch(deadline)? && Instant::now() >= deadline {
                stream.close();
                bail!("KDE didn't start the screen stream in time");
            }
        }
    }
}

/// A KWin stream of one monitor or window: its PipeWire node is live for as
/// long as this is held.
pub(crate) struct Cast {
    pub node: u32,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// The helper that asked for it, and its stdin: closed to let go.
    helper: Option<(Child, ChildStdin)>,
}

/// Whom the helper's reader tells if KWin stops the stream.
type Ended = Box<dyn FnOnce(String) + Send>;

/// A helper with its stream.
struct Helper {
    node: u32,
    child: Child,
    stdin: ChildStdin,
    /// Reads what it says; `tell` hands it `Ended` once the stream is wanted.
    reader: JoinHandle<()>,
    tell: mpsc::Sender<Ended>,
    stop: Arc<AtomicBool>,
}

impl Cast {
    /// Ask KWin to stream the monitor with this id (the main one if there's
    /// none such). `ended` is called if KWin stops the stream. Through the
    /// helper, so our windows are in it; here, without them, if the helper
    /// can't be started.
    pub(crate) fn open(id: &str, ended: impl FnOnce(String) + Send + 'static) -> Result<Self> {
        let helper = if HELPS.load(Ordering::Relaxed) { Self::open_helper(id) } else { Err(anyhow!("this program can't be one")) };
        match helper {
            Ok(Ok(h)) => {
                let _ = h.tell.send(Box::new(ended));
                Ok(Self { node: h.node, stop: h.stop, thread: Some(h.reader), helper: Some((h.child, h.stdin)) })
            }
            Ok(Err(refused)) => Err(refused),
            Err(e) => {
                if HELPS.load(Ordering::Relaxed) {
                    eprintln!("kde screen stream: no helper ({e:#}), so asked here: our windows aren't in it");
                }
                let session = Session::connect()?;
                let output = session.output(id)?;
                let qh = session.queue.handle();
                let stream = session.screencast.stream_output(&output, POINTER_EMBEDDED, &qh, ());
                Self::stream(session, stream, ended)
            }
        }
    }

    /// Ask KWin to stream one window (by its uuid in KWin's window list,
    /// `super::kwin_windows`), at its own size. `ended` is called if KWin
    /// stops the stream (the window closed).
    pub(crate) fn open_window(uuid: &str, ended: impl FnOnce(String) + Send + 'static) -> Result<Self> {
        let session = Session::connect()?;
        let qh = session.queue.handle();
        let stream = session.screencast.stream_window(uuid.to_owned(), POINTER_EMBEDDED, &qh, ());
        Self::stream(session, stream, ended)
    }

    /// Wait for KWin to make `stream`'s PipeWire node, then keep the
    /// connection open on a thread for as long as this is held.
    fn stream(mut session: Session, stream: stream::ZkdeScreencastStreamUnstableV1, ended: impl FnOnce(String) + Send + 'static) -> Result<Self> {
        let node = session.wait_for_node(&stream)?;
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
        Ok(Self { node, stop, thread: Some(thread), helper: None })
    }

    /// Start the helper and wait for its stream's node. An error if it can't
    /// be used (then it's asked here); inside, KWin's refusal.
    fn open_helper(id: &str) -> Result<Result<Helper>> {
        // This app, even if its file has been replaced since (an update).
        let mut child = Command::new("/proc/self/exe")
            .arg(HELPER_ARG)
            .arg(id)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .context("couldn't start it")?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = child.kill();
            let _ = child.wait();
            bail!("no pipes to it");
        };
        let (first_tx, first_rx) = mpsc::channel::<Option<String>>();
        let (tell, told) = mpsc::channel::<Ended>();
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        // Its answer; then, once the stream is wanted, whether it ends: KWin
        // stopped it, or the helper went while it was still wanted.
        let reader = thread::Builder::new().name("kwin-cast".into()).spawn(move || {
            let mut lines = BufReader::new(stdout).lines();
            let _ = first_tx.send(lines.next().and_then(|l| l.ok()));
            let Ok(ended) = told.recv() else { return };
            let why = loop {
                match lines.next() {
                    Some(Ok(line)) => match line.strip_prefix("ended ") {
                        Some(why) => break why.to_owned(),
                        None => continue,
                    },
                    _ => break "the screen stream's helper stopped".to_owned(),
                }
            };
            if !stop2.load(Ordering::Relaxed) {
                ended(why);
            }
        })?;
        let unusable = |mut child: Child, why: &str| -> Result<Result<Helper>> {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{why}")
        };
        // It waits up to 5 seconds for KWin.
        let first = match first_rx.recv_timeout(Duration::from_secs(8)) {
            Ok(Some(line)) => line,
            Ok(None) => return unusable(child, "it stopped without answering"),
            Err(_) => return unusable(child, "it didn't answer"),
        };
        if let Some(node) = first.strip_prefix("node ").and_then(|n| n.trim().parse().ok()) {
            return Ok(Ok(Helper { node, child, stdin, reader, tell, stop }));
        }
        match first.strip_prefix("failed ") {
            Some(why) => {
                // It exits after saying so.
                drop(stdin);
                let _ = child.wait();
                Ok(Err(anyhow!("{why}")))
            }
            None => unusable(child, &format!("it answered {first:?}")),
        }
    }
}

impl Drop for Cast {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some((mut child, stdin)) = self.helper.take() {
            // Its stdin closing is its cue to let go of the stream and exit.
            drop(stdin);
            let deadline = Instant::now() + Duration::from_secs(2);
            while matches!(child.try_wait(), Ok(None)) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
            if matches!(child.try_wait(), Ok(None)) {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Called first thing by a program that can be the helper (the app): be the
/// helper `Cast::open` starts, if that's what this process was started as.
/// Ask KWin to stream the monitor, print `node <n>` (or `failed <why>`), keep
/// it streaming until stdin closes (the app let go of it, or quit) and print
/// `ended <why>` if KWin stops it first. Returns whether this was the
/// helper; the program exits then.
pub fn helper() -> bool {
    HELPS.store(true, Ordering::Relaxed);
    // `args_os`: the app may be opened with a path that isn't UTF-8.
    let mut args = std::env::args_os().skip(1);
    if args.next().is_none_or(|a| a != HELPER_ARG) {
        return false;
    }
    let id = args.next().map(|a| a.to_string_lossy().into_owned()).unwrap_or_default();
    let say = |what: &str, why: &str| {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{what} {}", why.replace(['\n', '\r'], " "));
        let _ = out.flush();
    };
    let asked = Session::connect().and_then(|mut session| {
        let output = session.output(&id)?;
        let qh = session.queue.handle();
        let stream = session.screencast.stream_output(&output, POINTER_EMBEDDED, &qh, ());
        let node = session.wait_for_node(&stream)?;
        Ok((session, stream, node))
    });
    let (mut session, stream, node) = match asked {
        Ok(asked) => asked,
        Err(e) => {
            say("failed", &format!("{e:#}"));
            return true;
        }
    };
    say("node", &node.to_string());
    let gone = Arc::new(AtomicBool::new(false));
    let gone2 = gone.clone();
    let _ = thread::Builder::new().name("stdin".into()).spawn(move || {
        let _ = std::io::copy(&mut std::io::stdin().lock(), &mut std::io::sink());
        gone2.store(true, Ordering::Relaxed);
    });
    while !gone.load(Ordering::Relaxed) {
        let next = Instant::now() + Duration::from_millis(100);
        if let Err(e) = session.dispatch(next) {
            session.state.ended = Some(format!("lost the connection to KDE: {e}"));
        }
        if let Some(why) = session.state.ended.take() {
            say("ended", &why);
            break;
        }
    }
    stream.close();
    let _ = session.conn.flush();
    true
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
            wl_output::Event::Geometry { x, y, make, model, transform, .. } => {
                use wl_output::Transform as T;
                o.sideways = matches!(transform, WEnum::Value(T::_90 | T::_270 | T::Flipped90 | T::Flipped270));
                o.position = (x, y);
                if o.description.is_none() {
                    o.description = Some(format!("{make} {model}").trim().to_owned());
                }
            }
            wl_output::Event::Mode { flags: WEnum::Value(f), width, height, refresh } if f.contains(wl_output::Mode::Current) => {
                o.size = Some((width, height));
                o.refresh = Some(refresh);
            }
            _ => {}
        }
    }
}

impl Dispatch<output_order::KdeOutputOrderV1, ()> for State {
    fn event(state: &mut Self, _: &output_order::KdeOutputOrderV1, event: output_order::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            output_order::Event::Output { output_name } => state.next_order.push(output_name),
            output_order::Event::Done => state.order = std::mem::take(&mut state.next_order),
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
