//! The screen's picture, read into memory: from the X server on an X11
//! session (`super::x11`), copied from the compositor on wlroots-style
//! Wayland desktops (`super::wlr`), else from a PipeWire video node: KWin's
//! own on KDE Plasma (`super::kwin`), or the portal's.
//!
//! One cast serves everything that wants the screen: the Sources preview and
//! a recording share it, and it lives on for a few seconds after the last one
//! lets go, so the preview handing over to a recording (or back) neither
//! re-asks the desktop nor flashes its "sharing" indicator.
//!
//! From the portal, frames arrive only when something on screen changes; the
//! newest one stays in [`Latest`] for the pacer to repeat at a constant frame
//! rate. From X11 and wlroots they're read at the frame rate. They come as plain memory (no DMA-BUF modifiers are offered, so the compositor
//! copies them out for us): 4 bytes a pixel, BGRx or RGBx.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::os::fd::OwnedFd;
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use pipewire as pw;
use pw::spa;

use super::portal::{self, Cast};
use super::{kwin, wlr, x11};

/// Which way round a frame's colour bytes are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Order {
    /// B, G, R, then alpha or padding.
    Bgr,
    /// R, G, B, then alpha or padding.
    Rgb,
}

/// One picture of the screen.
pub(crate) struct Frame {
    pub width: u32,
    pub height: u32,
    /// Bytes from one row to the next.
    pub stride: usize,
    pub order: Order,
    pub data: Vec<u8>,
}

/// The newest picture, shared with whoever reads it.
#[derive(Default)]
pub(crate) struct Latest {
    frame: Mutex<Option<Arc<Frame>>>,
    /// Counts pictures, so a reader can tell a new one from the one it has.
    seq: AtomicU64,
    /// Why the picture stopped coming, once it has.
    ended: Mutex<Option<String>>,
    /// The picture before the newest, once nobody holds it: the next one is
    /// made in its memory.
    spare: Mutex<Option<Frame>>,
    /// Whether a new picture is wanted, for a source that makes them on
    /// request (X11), and the signal to it.
    wanted: Mutex<bool>,
    wake: Condvar,
}

impl Latest {
    /// The newest picture and its number.
    pub(crate) fn get(&self) -> Option<(Arc<Frame>, u64)> {
        let frame = self.frame.lock().unwrap().clone()?;
        Some((frame, self.seq.load(Ordering::Acquire)))
    }

    /// Ask for a new picture, from a source that makes them on request: it's
    /// made at once, for the next reader. Readers ask once per picture they
    /// use, so pictures come as often as they're used, evenly spaced.
    pub(crate) fn want(&self) {
        *self.wanted.lock().unwrap() = true;
        self.wake.notify_one();
    }

    /// Wait until a picture is wanted (true), or `timeout` passes (false).
    pub(crate) fn wait_wanted(&self, timeout: Duration) -> bool {
        let wanted = self.wanted.lock().unwrap();
        let (mut wanted, _) = self.wake.wait_timeout_while(wanted, timeout, |w| !*w).unwrap();
        std::mem::take(&mut *wanted)
    }

    pub(crate) fn has_frame(&self) -> bool {
        self.frame.lock().unwrap().is_some()
    }

    /// Why the cast stopped (the user stopped sharing, the compositor went
    /// away), if it did.
    pub(crate) fn ended(&self) -> Option<String> {
        self.ended.lock().unwrap().clone()
    }

    pub(crate) fn end(&self, why: String) {
        self.ended.lock().unwrap().get_or_insert(why);
    }

    /// Copy a picture in, rows `stride` bytes apart.
    fn store(&self, width: u32, height: u32, stride: usize, order: Order, rows: &[u8]) {
        let row = width as usize * 4;
        self.fill(width, height, order, |data| {
            for (y, out) in data.chunks_exact_mut(row).enumerate() {
                let at = y * stride;
                out.copy_from_slice(&rows[at..at + row]);
            }
        });
    }

    /// Make the new picture with `draw`, which is given its rows (packed).
    /// The newest stays readable meanwhile.
    pub(crate) fn fill(&self, width: u32, height: u32, order: Order, draw: impl FnOnce(&mut [u8])) {
        let row = width as usize * 4;
        let spare = self.spare.lock().unwrap().take();
        let mut frame = spare.unwrap_or(Frame { width, height, stride: row, order, data: Vec::new() });
        frame.width = width;
        frame.height = height;
        frame.stride = row;
        frame.order = order;
        frame.data.resize(row * height as usize, 0);
        draw(&mut frame.data);
        let old = self.frame.lock().unwrap().replace(Arc::new(frame));
        self.seq.fetch_add(1, Ordering::Release);
        if let Some(Ok(old)) = old.map(Arc::try_unwrap) {
            *self.spare.lock().unwrap() = Some(old);
        }
    }
}

/// A running screen cast.
pub(crate) struct Screen {
    pub latest: Arc<Latest>,
    /// The screen asked for (on X11 and wlroots a monitor; the portal picks its own).
    id: String,
    stop: Stop,
    thread: Mutex<Option<JoinHandle<()>>>,
}

/// How to stop a cast's thread.
enum Stop {
    /// A PipeWire stream (the portal's, KWin's).
    Portal(Mutex<Option<pw::channel::Sender<()>>>),
    /// A source that reads the screen itself (X11, wlroots).
    Direct(Arc<AtomicBool>),
}

static SHARED: Mutex<Option<Weak<Screen>>> = Mutex::new(None);

/// The screen cast of `id`, started if none is running. Blocks while the
/// desktop asks which screen to share (the portal, and only when no choice
/// is remembered), then until the first picture arrives.
pub(crate) fn acquire(id: &str) -> Result<Arc<Screen>> {
    // Held throughout, so two starting together share one dialog and cast.
    let mut shared = SHARED.lock().unwrap();
    if let Some(screen) = shared.as_ref().and_then(Weak::upgrade).filter(|s| s.latest.ended().is_none() && s.id == id) {
        return Ok(screen);
    }
    // Should reading the screen directly fail, the portal may still work.
    let direct = if x11::session() {
        Screen::start_direct(id, x11::Grab::open, x11::run).inspect_err(|e| eprintln!("X11 screen capture: {e:#}; trying the desktop portal")).ok()
    } else if wlr::available() {
        Screen::start_direct(id, wlr::Grab::open, wlr::run).inspect_err(|e| eprintln!("Wayland screen copy: {e:#}; trying the desktop portal")).ok()
    } else if kwin::available() {
        Screen::start_kwin(id).inspect_err(|e| eprintln!("KDE screen stream: {e:#}; trying the desktop portal")).ok()
    } else {
        None
    };
    let screen = Arc::new(match direct {
        Some(s) => s,
        None => Screen::start_portal(id)?,
    });
    *shared = Some(Arc::downgrade(&screen));
    drop(shared);
    // Fails here, visibly, rather than recording a black file.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !screen.latest.has_frame() {
        if let Some(why) = screen.latest.ended() {
            bail!("{why}");
        }
        if Instant::now() >= deadline {
            bail!("the screen isn't delivering frames");
        }
        thread::sleep(Duration::from_millis(10));
    }
    Ok(screen)
}

/// Don't hand out the running cast anymore: the next [`acquire`] starts a new
/// one (it ends once its holders let go).
pub(crate) fn forget() {
    *SHARED.lock().unwrap() = None;
}

/// Let go of the cast, keeping it running a little longer in case someone
/// takes it up again right away (the preview after a recording, or the other
/// way round).
pub(crate) fn release(screen: Arc<Screen>) {
    thread::spawn(move || {
        thread::sleep(Duration::from_secs(3));
        drop(screen);
    });
}

impl Screen {
    fn start_portal(id: &str) -> Result<Self> {
        let (cast, fd) = Cast::open()?;
        let node = cast.node;
        Self::start_pipewire(id, Some(fd), node, Arc::new(Latest::default()), move || cast.close())
    }

    /// KWin's stream: the node is on the session's own PipeWire.
    fn start_kwin(id: &str) -> Result<Self> {
        let latest = Arc::new(Latest::default());
        let ended = Arc::downgrade(&latest);
        let cast = kwin::Cast::open(id, move |why| {
            if let Some(l) = ended.upgrade() {
                l.end(why);
            }
        })?;
        let node = cast.node;
        Self::start_pipewire(id, None, node, latest, move || drop(cast))
    }

    /// Read a PipeWire video node on its own thread: on the remote behind
    /// `fd` (the portal's), else the session's PipeWire. `done` runs when the
    /// reading stops.
    fn start_pipewire(id: &str, fd: Option<OwnedFd>, node: u32, latest: Arc<Latest>, done: impl FnOnce() + Send + 'static) -> Result<Self> {
        let (ready_tx, ready_rx) = mpsc::channel::<Result<pw::channel::Sender<()>>>();
        let latest2 = latest.clone();
        let thread = thread::Builder::new().name("screen".into()).spawn(move || {
            if let Err(e) = run(fd, node, &latest2, &ready_tx) {
                latest2.end(format!("{e:#}"));
                let _ = ready_tx.send(Err(e));
            }
            done();
        })?;
        let stop = ready_rx.recv_timeout(Duration::from_secs(10)).map_err(|_| anyhow!("PipeWire didn't open the screen"))??;
        Ok(Self { latest, id: id.to_owned(), stop: Stop::Portal(Mutex::new(Some(stop))), thread: Mutex::new(Some(thread)) })
    }

    /// A source that reads the screen itself: `open` connects (here, so a
    /// failure is reported), `run` reads on its own thread.
    fn start_direct<G: Send + 'static>(
        id: &str,
        open: fn(&str) -> Result<G>,
        run: fn(G, &str, &Latest, &AtomicBool),
    ) -> Result<Self> {
        let grab = open(id)?;
        let latest = Arc::new(Latest::default());
        let stop = Arc::new(AtomicBool::new(false));
        let (latest2, stop2, id2) = (latest.clone(), stop.clone(), id.to_owned());
        let thread = thread::Builder::new().name("screen".into()).spawn(move || run(grab, &id2, &latest2, &stop2))?;
        Ok(Self { latest, id: id.to_owned(), stop: Stop::Direct(stop), thread: Mutex::new(Some(thread)) })
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        match &self.stop {
            Stop::Portal(stop) => {
                if let Some(stop) = stop.lock().unwrap().take() {
                    let _ = stop.send(());
                }
            }
            Stop::Direct(stop) => {
                stop.store(true, Ordering::Relaxed);
                self.latest.want();
            }
        }
        if let Some(t) = self.thread.lock().unwrap().take() {
            let _ = t.join();
        }
    }
}

/// What the stream's callbacks keep between calls.
#[derive(Default)]
struct StreamState {
    width: u32,
    height: u32,
    order: Option<Order>,
}

/// Read the cast until told to stop (or it ends). Sends the stopper once the
/// stream is connected.
fn run(fd: Option<OwnedFd>, node: u32, latest: &Arc<Latest>, ready: &mpsc::Sender<Result<pw::channel::Sender<()>>>) -> Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = match fd {
        Some(fd) => context.connect_fd_rc(fd, None)?,
        None => context.connect_rc(None)?,
    };
    let stream = pw::stream::StreamRc::new(
        core.clone(),
        "hesteclips-screen",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )?;

    let (latest_p, latest_s) = (latest.clone(), latest.clone());
    let quit = mainloop.downgrade();
    let _listener = stream
        .add_local_listener_with_user_data(StreamState::default())
        .state_changed(move |_, _, _, new| {
            let why = match new {
                pw::stream::StreamState::Error(e) => format!("the screen capture failed: {e}"),
                pw::stream::StreamState::Unconnected => "screen sharing was stopped".to_owned(),
                _ => return,
            };
            latest_s.end(why);
            if let Some(l) = quit.upgrade() {
                l.quit();
            }
        })
        .param_changed(|_, st, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let mut info = spa::param::video::VideoInfoRaw::new();
            if info.parse(param).is_err() {
                return;
            }
            use spa::param::video::VideoFormat as F;
            st.order = match info.format() {
                F::BGRx | F::BGRA => Some(Order::Bgr),
                F::RGBx | F::RGBA => Some(Order::Rgb),
                _ => None,
            };
            st.width = info.size().width;
            st.height = info.size().height;
            if st.width > 0 && st.height > 0 {
                portal::remember_size((st.width, st.height));
            }
        })
        .process(move |stream, st| {
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            let Some(order) = st.order else { return };
            let Some(data) = buffer.datas_mut().first_mut() else { return };
            let (size, offset, stride) = (data.chunk().size() as usize, data.chunk().offset() as usize, data.chunk().stride());
            // No new picture: only the pointer moved (or nothing at all).
            if size == 0 || data.chunk().flags().contains(spa::buffer::ChunkFlags::CORRUPTED) {
                return;
            }
            let (w, h) = (st.width, st.height);
            let stride = if stride > 0 { stride as usize } else { w as usize * 4 };
            let Some(bytes) = data.data() else { return };
            let need = offset + stride * (h as usize).saturating_sub(1) + w as usize * 4;
            if w == 0 || h == 0 || bytes.len() < need {
                return;
            }
            latest_p.store(w, h, stride, order, &bytes[offset..]);
        })
        .register()?;

    let format = video_format_pod();
    let mut params = [spa::pod::Pod::from_bytes(&format).ok_or_else(|| anyhow!("bad video format"))?];
    stream.connect(
        spa::utils::Direction::Input,
        Some(node),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;

    let (stop_tx, stop_rx) = pw::channel::channel::<()>();
    let quit = mainloop.downgrade();
    let _stop = stop_rx.attach(mainloop.loop_(), move |()| {
        if let Some(l) = quit.upgrade() {
            l.quit();
        }
    });
    let _ = ready.send(Ok(stop_tx));
    mainloop.run();
    let _ = stream.disconnect();
    Ok(())
}

/// What we take: 4-byte RGB in either order, any size, up to 360 fps (the
/// compositor sends at most one per screen refresh, and only on changes).
fn video_format_pod() -> Vec<u8> {
    use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    use spa::param::video::VideoFormat;
    use spa::pod::{Value, property, serialize::PodSerializer};
    use spa::utils::{Fraction, Rectangle, SpaTypes};
    let obj = spa::pod::object!(
        SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        property!(FormatProperties::MediaType, Id, MediaType::Video),
        property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA,
            VideoFormat::RGBx,
            VideoFormat::RGBA
        ),
        property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            Rectangle { width: 1920, height: 1080 },
            Rectangle { width: 1, height: 1 },
            Rectangle { width: 16384, height: 16384 }
        ),
        property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            Fraction { num: 60, denom: 1 },
            Fraction { num: 0, denom: 1 },
            Fraction { num: 360, denom: 1 }
        ),
    );
    PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(obj)).map(|(c, _)| c.into_inner()).unwrap_or_default()
}
