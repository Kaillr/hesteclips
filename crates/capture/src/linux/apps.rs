//! Games and apps (`VideoSource::Apps`): whichever of the picked programs
//! was last in focus, one window at a time, at the window's own size. On KDE
//! Plasma every window, listed by KWin (`super::kwin_windows`) and streamed
//! by it (`super::kwin`); elsewhere the X server's windows
//! (`super::x11_windows`): every window on an X11 session, and on a Wayland
//! one XWayland's, where Proton and Wine games run. A native Wayland window
//! on GNOME can't be followed (GNOME keeps its window list to itself).
//!
//! A thread follows the window list. When a window of a picked program comes
//! into focus, its stream replaces the one before; clicking into something
//! else keeps it. With the away screen on, it shows whenever none of them
//! does: minimized, closed, or none open (as on macOS). Off, a minimized
//! one's last picture stays, and black shows while none is open.
//!
//! A program is known by its file name (`cs2`, or `Game.exe` for a Proton
//! game), as games are everywhere else (`super::focus::program_of`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::screen::{self, Frame, Order, Screen};
use super::{ext_windows, focus, kwin, kwin_windows, wlr_windows, x11_windows};
use crate::{Device, StillImage};

/// A window, as the desktop describes it.
#[derive(Debug, Clone, Default)]
pub(crate) struct Window {
    /// What it's read by: KWin's uuid, or `x11win:<id>`.
    pub id: String,
    pub pid: Option<u32>,
    pub title: String,
    /// Its Wayland app id or X11 class: often its `.desktop` file's name.
    pub app_id: String,
    pub active: bool,
    pub minimized: bool,
    /// Not in the taskbar: a panel, a popup, a tooltip.
    pub skip_taskbar: bool,
}

/// Where windows are listed and read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Kde,
    /// Sway and its kin: the ext window list and capture, with the wlroots
    /// list for which window is active.
    Wlr,
    X11,
}

/// The way windows are followed here, if any: asked once.
fn source() -> Option<Source> {
    static SOURCE: std::sync::OnceLock<Option<Source>> = std::sync::OnceLock::new();
    *SOURCE.get_or_init(|| {
        // `HESTECLIPS_APPS_X11`: the X server's windows even on KDE (to try them).
        let x11_first = std::env::var_os("HESTECLIPS_APPS_X11").is_some();
        if x11_first && x11_windows::available() {
            Some(Source::X11)
        } else if kwin::available() && kwin_windows::following() {
            Some(Source::Kde)
        } else if ext_windows::available() && wlr_windows::windows().is_some() {
            Some(Source::Wlr)
        } else if x11_windows::available() {
            Some(Source::X11)
        } else {
            None
        }
    })
}

impl Source {
    fn windows(self) -> Vec<Window> {
        match self {
            Self::Kde => kwin_windows::windows()
                .unwrap_or_default()
                .into_iter()
                .map(|w| Window { id: w.uuid, pid: w.pid, title: w.title, app_id: w.app_id, active: w.active, minimized: w.minimized, skip_taskbar: w.skip_taskbar })
                .collect(),
            Self::Wlr => wlr_listed(),
            Self::X11 => x11_windows::windows(),
        }
    }

    fn open(self, id: &str) -> anyhow::Result<Screen> {
        match self {
            Self::Kde => screen::open_kwin_window(id),
            Self::Wlr => screen::open_ext_window(id),
            Self::X11 => screen::open_x11_window(id),
        }
    }
}

/// The ext list's windows, with which is active or minimized from the
/// wlroots list (matched by app id and title), and their processes found by
/// app id (remembered a few seconds: that reads every process's file).
fn wlr_listed() -> Vec<Window> {
    static PIDS: Mutex<Option<(Instant, HashMap<String, Option<u32>>)>> = Mutex::new(None);
    let states = wlr_windows::windows().unwrap_or_default();
    let mut pids = PIDS.lock().unwrap();
    let (at, cache) = pids.get_or_insert_with(|| (Instant::now(), HashMap::new()));
    if at.elapsed() > Duration::from_secs(5) {
        cache.clear();
        *at = Instant::now();
    }
    ext_windows::windows()
        .unwrap_or_default()
        .into_iter()
        .map(|w| {
            let state = states.iter().find(|s| s.app_id == w.app_id && s.title == w.title);
            let pid = *cache.entry(w.app_id.clone()).or_insert_with(|| focus::pid_of_app_id(&w.app_id));
            Window {
                id: w.id,
                pid,
                title: w.title,
                app_id: w.app_id,
                active: state.is_some_and(|s| s.active),
                minimized: state.is_some_and(|s| s.minimized),
                skip_taskbar: false,
            }
        })
        .collect()
}

/// Whether games and apps can be recorded here: KDE Plasma's Wayland
/// session (its screencast and window list ours), or an X server's windows
/// (an X11 session, or XWayland's on any Wayland desktop).
pub(crate) fn available() -> bool {
    source().is_some()
}

/// Running programs with a window in the taskbar (not ours), by program,
/// sorted by name.
pub(crate) fn list() -> Vec<Device> {
    let me = std::process::id();
    let mut programs = Programs::default();
    let mut out: Vec<Device> = Vec::new();
    let Some(source) = source() else { return out };
    for w in source.windows() {
        if w.skip_taskbar || w.pid.is_none_or(|p| p == me) {
            continue;
        }
        let Some(id) = programs.of(w.pid) else { continue };
        if out.iter().any(|d| d.id.eq_ignore_ascii_case(&id)) {
            continue;
        }
        let name = desktop_name(&w.app_id).or_else(|| Some(w.title.trim().to_owned()).filter(|t| !t.is_empty())).unwrap_or_else(|| id.clone());
        out.push(Device { id, name });
    }
    out.sort_by_key(|d| d.name.to_lowercase());
    out
}

/// The window a layer of program `app` (its file name, as [`list`] names
/// it) shows: the one in focus, else one that isn't minimized, else any
/// (never a panel's or ours). Its id.
pub(crate) fn find_window(app: &str) -> Option<String> {
    let source = source()?;
    let me = std::process::id();
    let mut programs = Programs::default();
    let theirs: Vec<Window> = source
        .windows()
        .into_iter()
        .filter(|w| !w.skip_taskbar && w.pid.is_some_and(|p| p != me))
        .filter(|w| programs.of(w.pid).is_some_and(|p| p.eq_ignore_ascii_case(app)))
        .collect();
    let pick = theirs.iter().find(|w| w.active).or_else(|| theirs.iter().find(|w| !w.minimized)).or(theirs.first())?;
    Some(pick.id.clone())
}

/// A stream of window `id` ([`find_window`]'s), at its own size.
pub(crate) fn open_window(id: &str) -> anyhow::Result<Screen> {
    source().ok_or_else(|| anyhow::anyhow!("windows can't be captured on this desktop"))?.open(id)
}

/// Whether window `id` is still open.
pub(crate) fn window_open(id: &str) -> bool {
    source().is_some_and(|s| s.windows().iter().any(|w| w.id == id))
}

/// Program `app`'s window's size, where the desktop says (KDE Plasma).
pub(crate) fn window_size(app: &str) -> Option<(u32, u32)> {
    if source()? != Source::Kde {
        return None;
    }
    let id = find_window(app)?;
    kwin_windows::size(&id)
}

/// The name in an app's `.desktop` file (`Firefox` for `org.mozilla.firefox`).
fn desktop_name(app_id: &str) -> Option<String> {
    if app_id.is_empty() || app_id.contains('/') {
        return None;
    }
    let home = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    let dirs = std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    let file = format!("{app_id}.desktop");
    home.into_iter().chain(dirs.split(':').filter(|d| !d.is_empty()).map(PathBuf::from)).find_map(|dir| {
        let text = std::fs::read_to_string(dir.join("applications").join(&file)).ok()?;
        let entry = text.split("\n[").next()?;
        entry.lines().find_map(|l| l.strip_prefix("Name=")).map(|n| n.trim().to_owned()).filter(|n| !n.is_empty())
    })
}

/// Which program each process runs, remembered (reading `/proc` for every
/// window every few milliseconds would add up).
#[derive(Default)]
struct Programs(HashMap<u32, Option<String>>);

impl Programs {
    fn of(&mut self, pid: Option<u32>) -> Option<String> {
        let pid = pid?;
        self.0
            .entry(pid)
            .or_insert_with(|| focus::program_of(pid).and_then(|p| Some(p.file_name()?.to_string_lossy().into_owned())))
            .clone()
    }
}

/// How often the window list is looked at.
const FOLLOW_EVERY: Duration = Duration::from_millis(50);

/// After a window's stream fails to open, how long until it's tried again.
const RETRY_AFTER: Duration = Duration::from_secs(5);

/// Games and apps being followed: the picture to record now, whichever
/// window it's from.
pub(crate) struct Apps {
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

struct Shared {
    /// The programs to record.
    ids: Mutex<Vec<String>>,
    /// Show the away screen while none of them shows (else a minimized one's
    /// last picture, or black).
    away_when_unfocused: AtomicBool,
    /// The away screen (black if there's none), and black.
    away: Arc<Frame>,
    black: Arc<Frame>,
    now: Mutex<Now>,
    /// Counts changes of what's shown, so a new window's (or the away
    /// screen's) pictures read as new.
    generation: AtomicU64,
    /// Keep every window picture as it arrives (frame blending).
    queue: AtomicBool,
}

/// What's shown now.
#[derive(Default)]
struct Now {
    /// The window streamed: its uuid, its program and the stream.
    window: Option<(String, String, Arc<Screen>)>,
    /// It's minimized: nothing of it shows.
    minimized: bool,
}

impl Apps {
    /// Start following `ids`' windows. Returns at once: the away screen
    /// shows until one of them is found.
    pub(crate) fn start(ids: &[String], away_when_unfocused: bool, away_screen: Option<&StillImage>) -> Arc<Self> {
        let shared = Arc::new(Shared {
            ids: Mutex::new(ids.to_vec()),
            away_when_unfocused: AtomicBool::new(away_when_unfocused),
            away: Arc::new(away_screen.map_or_else(black, still_frame)),
            black: Arc::new(black()),
            now: Mutex::new(Now::default()),
            generation: AtomicU64::new(1),
            queue: AtomicBool::new(false),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let (shared2, stop2) = (shared.clone(), stop.clone());
        let thread = thread::Builder::new().name("apps".into()).spawn(move || follow(&shared2, &stop2)).ok();
        // The window in focus now is picked up before the first picture's asked for.
        thread::sleep(FOLLOW_EVERY * 2);
        Arc::new(Self { shared, stop, thread: Mutex::new(thread) })
    }

    /// Record these programs instead, without restarting.
    pub(crate) fn set(&self, ids: &[String], away_when_unfocused: bool) {
        *self.shared.ids.lock().unwrap() = ids.to_vec();
        // What shows changes with it: numbered on, so it's taken as new.
        if self.shared.away_when_unfocused.swap(away_when_unfocused, Ordering::Relaxed) != away_when_unfocused {
            self.shared.generation.fetch_add(1, Ordering::Release);
        }
    }

    /// Shown while none of them is: the away screen, if it's on.
    fn away(&self) -> Arc<Frame> {
        if self.shared.away_when_unfocused.load(Ordering::Relaxed) { self.shared.away.clone() } else { self.shared.black.clone() }
    }

    /// The picture to record now and a number that changes whenever it does.
    pub(crate) fn get(&self) -> Option<(Arc<Frame>, u64)> {
        let now = self.shared.now.lock().unwrap();
        let generation = self.shared.generation.load(Ordering::Acquire) << 40;
        let away = (generation, self.away());
        match &now.window {
            Some((_, _, screen)) if !(now.minimized && self.shared.away_when_unfocused.load(Ordering::Relaxed)) => {
                Some(screen.latest.get().map_or((away.1, away.0), |(f, seq)| (f, generation | seq)))
            }
            _ => Some((away.1, away.0)),
        }
    }

    /// Ask a window read on request (an X11 window) for its next picture.
    pub(crate) fn want(&self) {
        if let Some((_, _, s)) = &self.shared.now.lock().unwrap().window {
            s.latest.want();
        }
    }

    /// Keep every picture of the windows recorded as it arrives, for `drain`.
    pub(crate) fn queue_arrivals(&self) {
        self.shared.queue.store(true, Ordering::Relaxed);
        if let Some((_, _, s)) = &self.shared.now.lock().unwrap().window {
            s.latest.queue_arrivals();
        }
    }

    /// The window's pictures arrived since the last drain, numbered as `get`
    /// numbers them; none while the away screen shows.
    pub(crate) fn drain(&self) -> Vec<screen::Arrival> {
        let now = self.shared.now.lock().unwrap();
        let generation = self.shared.generation.load(Ordering::Acquire) << 40;
        match &now.window {
            Some((_, _, s)) if !(now.minimized && self.shared.away_when_unfocused.load(Ordering::Relaxed)) => {
                s.latest.drain().into_iter().map(|a| screen::Arrival { seq: generation | a.seq, ..a }).collect()
            }
            _ => Vec::new(),
        }
    }

    /// Whether none of them shows (the away screen, or a minimized one's last picture).
    pub(crate) fn waiting(&self) -> bool {
        let now = self.shared.now.lock().unwrap();
        now.window.is_none() || now.minimized
    }

    /// The program whose window is recorded, while it shows.
    pub(crate) fn app(&self) -> Option<String> {
        let now = self.shared.now.lock().unwrap();
        now.window.as_ref().filter(|_| !now.minimized).map(|(_, id, _)| id.clone())
    }
}

impl Drop for Apps {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.lock().unwrap().take() {
            let _ = t.join();
        }
    }
}

/// Follow the window list until told to stop.
fn follow(shared: &Shared, stop: &AtomicBool) {
    let mut programs = Programs::default();
    let mut failed: HashMap<String, Instant> = HashMap::new();
    let me = std::process::id();
    let Some(source) = source() else { return };
    while !stop.load(Ordering::Relaxed) {
        let windows = source.windows();
        let ids = shared.ids.lock().unwrap().clone();
        let mut program = |w: &Window| programs.of(w.pid).filter(|p| ids.iter().any(|i| i.eq_ignore_ascii_case(p)));
        let current = shared.now.lock().unwrap().window.as_ref().map(|(uuid, _, s)| (uuid.clone(), s.latest.ended().is_some()));
        // Its window, if it's still there (and its stream still runs) and still picked.
        let kept = current.as_ref().and_then(|(uuid, ended)| windows.iter().find(|w| &w.id == uuid && !ended)).filter(|w| program(w).is_some());
        // One of theirs in focus takes over.
        let focused = windows.iter().find(|w| w.active && !w.skip_taskbar && w.pid != Some(me)).and_then(|w| Some((w, program(w)?)));
        let minimized = kept.is_some_and(|w| w.minimized);
        match focused {
            Some((w, id)) if current.as_ref().is_none_or(|(uuid, _)| uuid != &w.id) && failed.get(&w.id).is_none_or(|t| t.elapsed() > RETRY_AFTER) => {
                match source.open(&w.id) {
                    Ok(s) => {
                        failed.remove(&w.id);
                        if shared.queue.load(Ordering::Relaxed) {
                            s.latest.queue_arrivals();
                        }
                        let old = std::mem::replace(&mut *shared.now.lock().unwrap(), Now { window: Some((w.id.clone(), id, Arc::new(s))), minimized: false });
                        shared.generation.fetch_add(1, Ordering::Release);
                        drop(old);
                    }
                    Err(e) => {
                        eprintln!("couldn't record {id}'s window: {e:#}");
                        failed.insert(w.id.clone(), Instant::now());
                    }
                }
            }
            _ if current.is_some() && kept.is_none() => {
                // Closed (or no longer picked): the away screen.
                let old = std::mem::take(&mut *shared.now.lock().unwrap());
                shared.generation.fetch_add(1, Ordering::Release);
                drop(old);
            }
            _ => {
                let mut now = shared.now.lock().unwrap();
                if now.minimized != minimized {
                    now.minimized = minimized;
                    shared.generation.fetch_add(1, Ordering::Release);
                }
            }
        }
        thread::sleep(FOLLOW_EVERY);
    }
}

/// A picture from a still image.
fn still_frame(img: &StillImage) -> Frame {
    Frame { width: img.width, height: img.height, stride: img.width as usize * 4, order: Order::Bgr, data: img.bgra.clone(), texture: None }
}

/// A small black picture: scaled to the frame, all black.
fn black() -> Frame {
    let (width, height) = (16, 9);
    let data = [0, 0, 0, 255].repeat(width * height);
    Frame { width: width as u32, height: height as u32, stride: width * 4, order: Order::Bgr, data, texture: None }
}
