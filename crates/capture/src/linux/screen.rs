//! The screen's picture, read into memory: from the X server on an X11
//! session (`super::x11`), copied from the compositor on wlroots-style
//! Wayland desktops (`super::wlr`), else from a PipeWire video node: KWin's
//! own on KDE Plasma (`super::kwin`), Mutter's on GNOME (`super::mutter`), or
//! the portal's.
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
use std::collections::HashMap;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use pipewire as pw;
use pw::spa;

use super::portal::{self, Cast};
use super::{gpu, kwin, mutter, wlr, x11};

/// Which way round a frame's colour bytes are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Order {
    /// B, G, R, then alpha or padding.
    Bgr,
    /// R, G, B, then alpha or padding.
    Rgb,
}

/// One picture of the screen: in our memory, or on the graphics card.
pub(crate) struct Frame {
    pub width: u32,
    pub height: u32,
    /// Bytes from one row to the next.
    pub stride: usize,
    pub order: Order,
    /// The pixels, unless the picture is on the graphics card.
    pub data: Vec<u8>,
    /// The picture on the graphics card (`super::gpu`), which `data` then isn't.
    pub texture: Option<wgpu::Texture>,
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
    /// Textures of pictures nobody holds anymore, for the next ones on the card.
    spare_textures: Mutex<Vec<wgpu::Texture>>,
    /// Whether a new picture is wanted, for a source that makes them on
    /// request (X11), and the signal to it.
    wanted: Mutex<bool>,
    wake: Condvar,
    /// For frame blending: every picture as it arrives, not just the newest
    /// (`queue_arrivals`), and the signal that one has.
    arrivals: Mutex<Option<std::collections::VecDeque<Arrival>>>,
    arrived: Condvar,
}

/// A picture as it arrived, for frame blending.
pub(crate) struct Arrival {
    /// When (CLOCK_MONOTONIC seconds).
    pub at: f64,
    pub frame: Arc<Frame>,
    pub seq: u64,
}

/// The most arrivals kept unread: past that the oldest go (a reader that
/// stopped reading shouldn't hold every picture).
const ARRIVALS_KEPT: usize = 256;

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
        let mut frame = spare.unwrap_or(Frame { width, height, stride: row, order, data: Vec::new(), texture: None });
        frame.width = width;
        frame.height = height;
        frame.stride = row;
        frame.order = order;
        frame.data.resize(row * height as usize, 0);
        draw(&mut frame.data);
        self.publish(frame);
    }

    /// A texture to copy the next picture on the card into: one nobody holds
    /// anymore of this size and format, if there is one.
    pub(crate) fn spare_texture(&self, width: u32, height: u32, format: wgpu::TextureFormat) -> Option<wgpu::Texture> {
        let mut spare = self.spare_textures.lock().unwrap();
        let at = spare.iter().position(|t| t.width() == width && t.height() == height && t.format() == format)?;
        Some(spare.swap_remove(at))
    }

    /// The newest picture is this texture, on the card.
    pub(crate) fn store_texture(&self, texture: wgpu::Texture) {
        let (width, height) = (texture.width(), texture.height());
        self.publish(Frame { width, height, stride: width as usize * 4, order: Order::Bgr, data: Vec::new(), texture: Some(texture) });
    }

    /// Keep every picture as it arrives too, for `drain` (frame blending).
    pub(crate) fn queue_arrivals(&self) {
        self.arrivals.lock().unwrap().get_or_insert_with(Default::default);
    }

    /// The pictures arrived since the last drain, oldest first.
    pub(crate) fn drain(&self) -> Vec<Arrival> {
        self.arrivals.lock().unwrap().as_mut().map(|q| q.drain(..).collect()).unwrap_or_default()
    }

    /// Wait up to `timeout` for a picture newer than number `seen`.
    pub(crate) fn wait(&self, seen: u64, timeout: Duration) {
        let q = self.arrivals.lock().unwrap();
        if self.seq.load(Ordering::Acquire) > seen {
            return;
        }
        let _ = self.arrived.wait_timeout(q, timeout);
    }

    /// Make `frame` the newest; keep the one before for reuse once nobody holds it.
    fn publish(&self, frame: Frame) {
        let frame = Arc::new(frame);
        let old = self.frame.lock().unwrap().replace(frame.clone());
        let seq = self.seq.fetch_add(1, Ordering::Release) + 1;
        {
            let mut q = self.arrivals.lock().unwrap();
            if let Some(q) = q.as_mut() {
                if q.len() >= ARRIVALS_KEPT {
                    // Frame blending fell behind: that picture missed its frame.
                    q.pop_front();
                    crate::count_missed(1);
                }
                q.push_back(Arrival { at: super::host_now(), frame, seq });
            }
        }
        self.arrived.notify_all();
        if let Some(Ok(mut old)) = old.map(Arc::try_unwrap) {
            match old.texture.take() {
                Some(t) => {
                    let mut spare = self.spare_textures.lock().unwrap();
                    // A few are plenty: older ones (another size, say) go.
                    if spare.len() >= 4 {
                        spare.remove(0);
                    }
                    spare.push(t);
                }
                None => *self.spare.lock().unwrap() = Some(old),
            }
        }
    }
}

/// A running screen cast.
pub(crate) struct Screen {
    pub latest: Arc<Latest>,
    /// The screen asked for (a monitor; the portal picks its own).
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

/// The casts running, each shared by whoever records or shows its screen
/// (the recording's picture, another display's layer, the preview).
static SHARED: Mutex<Vec<Weak<Screen>>> = Mutex::new(Vec::new());

/// The screen cast of `id`, started if none is running. Blocks while the
/// desktop asks which screen to share (the portal, and only when no choice
/// is remembered), then until the first picture arrives.
pub(crate) fn acquire(id: &str) -> Result<Arc<Screen>> {
    // Held throughout, so two starting together share one dialog and cast.
    let mut shared = SHARED.lock().unwrap();
    shared.retain(|w| w.strong_count() > 0);
    if let Some(screen) = shared.iter().filter_map(Weak::upgrade).find(|s| s.latest.ended().is_none() && s.id == id) {
        return Ok(screen);
    }
    // Should reading the screen directly fail, the portal may still work.
    let direct = if x11::session() {
        Screen::start_direct(id, x11::Grab::open, x11::run).inspect_err(|e| eprintln!("X11 screen capture: {e:#}; trying the desktop portal")).ok()
    } else if wlr::available() {
        Screen::start_direct(id, wlr::Grab::open, wlr::run).inspect_err(|e| eprintln!("Wayland screen copy: {e:#}; trying the desktop portal")).ok()
    } else if kwin::available() {
        Screen::start_kwin(id).inspect_err(|e| eprintln!("KDE screen stream: {e:#}; trying the desktop portal")).ok()
    } else if mutter::available() {
        Screen::start_mutter(id).inspect_err(|e| eprintln!("GNOME screen stream: {e:#}; trying the desktop portal")).ok()
    } else {
        None
    };
    let screen = Arc::new(match direct {
        Some(s) => s,
        None => Screen::start_portal(id)?,
    });
    shared.push(Arc::downgrade(&screen));
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

/// A stream of one window on KDE Plasma (by its uuid in KWin's window list),
/// at its own size; not shared. Blocks until its first picture (for up to 2
/// seconds: a window that isn't drawing yet still opens).
pub(crate) fn open_kwin_window(uuid: &str) -> Result<Screen> {
    let latest = Arc::new(Latest::default());
    let ended = Arc::downgrade(&latest);
    let cast = kwin::Cast::open_window(uuid, move |why| {
        if let Some(l) = ended.upgrade() {
            l.end(why);
        }
    })?;
    let node = cast.node;
    let screen = Screen::start_pipewire(uuid, None, node, latest, move || drop(cast))?;
    let deadline = Instant::now() + Duration::from_secs(2);
    while !screen.latest.has_frame() && screen.latest.ended().is_none() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    Ok(screen)
}

/// A window on the X server (`x11win:<id>`, see `super::x11_windows`), read
/// through XComposite at its own size; not shared.
pub(crate) fn open_x11_window(id: &str) -> Result<Screen> {
    let screen = Screen::start_direct(id, super::x11_windows::Grab::open, super::x11_windows::run)?;
    let deadline = Instant::now() + Duration::from_secs(2);
    while !screen.latest.has_frame() && screen.latest.ended().is_none() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    Ok(screen)
}

/// A window on Sway and its kin (`ext:<identifier>`, see
/// `super::ext_windows`), captured at its own size; not shared.
pub(crate) fn open_ext_window(id: &str) -> Result<Screen> {
    let screen = Screen::start_direct(id, super::ext_windows::Grab::open, super::ext_windows::run)?;
    let deadline = Instant::now() + Duration::from_secs(2);
    while !screen.latest.has_frame() && screen.latest.ended().is_none() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    Ok(screen)
}

/// Don't hand out the running cast anymore: the next [`acquire`] starts a new
/// one (it ends once its holders let go).
pub(crate) fn forget() {
    SHARED.lock().unwrap().clear();
}

/// Let go of the cast, keeping it running a little longer in case someone
/// takes it up again right away (the preview after a recording, or the other
/// way round).
pub(crate) fn release(screen: Arc<Screen>) {
    static REAPER: std::sync::Once = std::sync::Once::new();
    RELEASED.lock().unwrap().push((Instant::now() + Duration::from_secs(3), screen));
    REAPER.call_once(|| {
        let _ = thread::Builder::new().name("screen-release".into()).spawn(|| loop {
            thread::sleep(Duration::from_millis(250));
            let now = Instant::now();
            let expired: Vec<_> = {
                let mut released = RELEASED.lock().unwrap();
                let (gone, kept) = std::mem::take(&mut *released).into_iter().partition(|(at, _)| *at <= now);
                *released = kept;
                gone
            };
            drop(expired);
        });
    });
}

/// Casts let go of, still running a moment in case they're taken up again.
static RELEASED: Mutex<Vec<(Instant, Arc<Screen>)>> = Mutex::new(Vec::new());

/// Stop the casts let go of now (the app is quitting): one left running
/// would still be drawing on the graphics card while the process exits and
/// its driver is unloaded.
pub(crate) fn shutdown() {
    let released = std::mem::take(&mut *RELEASED.lock().unwrap());
    drop(released);
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

    /// Mutter's stream: the node is on the session's own PipeWire.
    fn start_mutter(id: &str) -> Result<Self> {
        let latest = Arc::new(Latest::default());
        let ended = Arc::downgrade(&latest);
        let cast = mutter::Cast::open(id, move |why| {
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
    /// On the graphics card: the picture's DRM format and layout.
    dmabuf: Option<(u32, u64)>,
    /// The compositor's buffers, imported, by file descriptor.
    imported: HashMap<i64, wgpu::Texture>,
    /// An import failed (said once).
    failed: bool,
}

/// The graphics card, when pictures are taken and worked on it (whenever
/// Vulkan can import DMA-BUFs; `HASTECLIP_NO_GPU_CAPTURE` turns it off).
pub(crate) fn gpu_frames() -> Option<Arc<gpu::Gpu>> {
    gpu::gpu()
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
        "hasteclip-screen",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )?;

    let card = gpu_frames();
    let modifiers = card.as_ref().map(|g| g.modifiers.clone()).unwrap_or_default();
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
        .param_changed(|stream, st, id, param| {
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
            st.imported.clear();
            st.dmabuf = None;
            // On the card: the compositor may leave the layout to us (any of
            // the ones offered); then pick one and agree again on just it.
            if let Some((modifier, choose)) = modifier_of(param) {
                if choose {
                    let pod = format_pod(Modifiers::Fixed(modifier));
                    if let Some(p) = spa::pod::Pod::from_bytes(&pod) {
                        let _ = stream.update_params(&mut [p]);
                    }
                    return;
                }
                let fourcc = match info.format() {
                    F::BGRx => gpu::DRM_XRGB8888,
                    F::BGRA => gpu::DRM_ARGB8888,
                    F::RGBx => gpu::DRM_XBGR8888,
                    F::RGBA => gpu::DRM_ABGR8888,
                    _ => return,
                };
                st.dmabuf = Some((fourcc, modifier));
            }
            let pod = buffers_pod(st.dmabuf.is_some());
            if let Some(p) = spa::pod::Pod::from_bytes(&pod) {
                let _ = stream.update_params(&mut [p]);
            }
        })
        .process(move |stream, st| {
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            let Some(order) = st.order else { return };
            let Some(data) = buffer.datas_mut().first_mut() else { return };
            let (size, offset, stride) = (data.chunk().size() as usize, data.chunk().offset() as usize, data.chunk().stride());
            if data.type_() == spa::buffer::DataType::DmaBuf {
                if let (Some(card), Some((fourcc, modifier))) = (&card, st.dmabuf) {
                    let fd = data.as_raw().fd;
                    let corrupted = data.chunk().flags().contains(spa::buffer::ChunkFlags::CORRUPTED);
                    if !corrupted && st.width > 0 && st.height > 0 {
                        take_dmabuf(card, st, &latest_p, fd, fourcc, modifier, offset as u64, stride.max(0) as u64);
                    }
                }
                return;
            }
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

    // On the card if it can, else in our memory.
    let mut pods = Vec::new();
    if !modifiers.is_empty() {
        pods.push(format_pod(Modifiers::Any(&modifiers)));
    }
    pods.push(format_pod(Modifiers::None));
    let mut params: Vec<&spa::pod::Pod> = pods.iter().map(|p| spa::pod::Pod::from_bytes(p).ok_or_else(|| anyhow!("bad video format"))).collect::<Result<_>>()?;
    stream.connect(
        spa::utils::Direction::Input,
        Some(node),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params[..],
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
/// `SPA_POD_PROP_FLAG_DONT_FIXATE`: a choice left open for the other side
/// (libspa only names it behind a feature).
const DONT_FIXATE: spa::pod::PropertyFlags = spa::pod::PropertyFlags::from_bits_retain(1 << 4);

/// What a format offers about the picture's layout on the card.
enum Modifiers<'a> {
    /// In our memory.
    None,
    /// On the card, in any of these layouts (the compositor or we pick).
    Any(&'a [u64]),
    /// On the card, in this one.
    Fixed(u64),
}

/// A video format we take: 4 bytes a pixel, any size, up to 1000 a second.
fn format_pod(modifiers: Modifiers) -> Vec<u8> {
    use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    use spa::param::video::VideoFormat;
    use spa::pod::{ChoiceValue, Object, Property, PropertyFlags, Value, serialize::PodSerializer};
    use spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Id, Rectangle, SpaTypes};
    let id_enum = |ids: &[VideoFormat]| Value::Choice(ChoiceValue::Id(Choice(ChoiceFlags::empty(), ChoiceEnum::Enum { default: Id(ids[0].as_raw()), alternatives: ids.iter().map(|f| Id(f.as_raw())).collect() })));
    let prop = |key: FormatProperties, value: Value| Property { key: key.as_raw(), flags: PropertyFlags::empty(), value };
    let formats: &[VideoFormat] = match modifiers {
        // What the card imports.
        Modifiers::Any(_) | Modifiers::Fixed(_) => &[VideoFormat::BGRx, VideoFormat::BGRA],
        Modifiers::None => &[VideoFormat::BGRx, VideoFormat::BGRA, VideoFormat::RGBx, VideoFormat::RGBA],
    };
    let mut properties = vec![
        prop(FormatProperties::MediaType, Value::Id(Id(MediaType::Video.as_raw()))),
        prop(FormatProperties::MediaSubtype, Value::Id(Id(MediaSubtype::Raw.as_raw()))),
        prop(FormatProperties::VideoFormat, id_enum(formats)),
    ];
    match modifiers {
        Modifiers::None => {}
        Modifiers::Any(list) => properties.push(Property {
            key: FormatProperties::VideoModifier.as_raw(),
            flags: PropertyFlags::MANDATORY | DONT_FIXATE,
            value: Value::Choice(ChoiceValue::Long(Choice(ChoiceFlags::empty(), ChoiceEnum::Enum { default: list[0] as i64, alternatives: list.iter().map(|m| *m as i64).collect() }))),
        }),
        Modifiers::Fixed(m) => properties.push(Property { key: FormatProperties::VideoModifier.as_raw(), flags: PropertyFlags::MANDATORY, value: Value::Long(m as i64) }),
    }
    properties.push(prop(
        FormatProperties::VideoSize,
        Value::Choice(ChoiceValue::Rectangle(Choice(ChoiceFlags::empty(), ChoiceEnum::Range { default: Rectangle { width: 1920, height: 1080 }, min: Rectangle { width: 1, height: 1 }, max: Rectangle { width: 16384, height: 16384 } }))),
    ));
    properties.push(prop(
        FormatProperties::VideoFramerate,
        Value::Choice(ChoiceValue::Fraction(Choice(ChoiceFlags::empty(), ChoiceEnum::Range { default: Fraction { num: 60, denom: 1 }, min: Fraction { num: 0, denom: 1 }, max: Fraction { num: 1000, denom: 1 } }))),
    ));
    let obj = Object { type_: SpaTypes::ObjectParamFormat.as_raw(), id: spa::param::ParamType::EnumFormat.as_raw(), properties };
    PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(obj)).map(|(c, _)| c.into_inner()).unwrap_or_default()
}

/// The layout of a picture on the card that a format says, and whether
/// it's left to us to pick (then the first offered). `None` for a picture
/// in our memory.
fn modifier_of(param: &spa::pod::Pod) -> Option<(u64, bool)> {
    use spa::pod::{ChoiceValue, Value, deserialize::PodDeserializer};
    use spa::utils::ChoiceEnum;
    let (_, Value::Object(obj)) = PodDeserializer::deserialize_any_from(param.as_bytes()).ok()? else { return None };
    let p = obj.properties.iter().find(|p| p.key == spa::param::format::FormatProperties::VideoModifier.as_raw())?;
    let open = p.flags.contains(DONT_FIXATE);
    match &p.value {
        Value::Long(m) => Some((*m as u64, false)),
        Value::Choice(ChoiceValue::Long(c)) => match &c.1 {
            ChoiceEnum::Enum { default, alternatives } => Some((alternatives.first().copied().unwrap_or(*default) as u64, true)),
            ChoiceEnum::None(m) => Some((*m as u64, open)),
            _ => None,
        },
        _ => None,
    }
}

/// The buffers we take: on the card, or in memory we can map.
fn buffers_pod(dmabuf: bool) -> Vec<u8> {
    use spa::pod::{Object, Property, PropertyFlags, Value, serialize::PodSerializer};
    let types = if dmabuf { 1 << spa::sys::SPA_DATA_DmaBuf } else { (1 << spa::sys::SPA_DATA_MemFd) | (1 << spa::sys::SPA_DATA_MemPtr) };
    let obj = Object {
        type_: spa::utils::SpaTypes::ObjectParamBuffers.as_raw(),
        id: spa::param::ParamType::Buffers.as_raw(),
        properties: vec![Property { key: spa::sys::SPA_PARAM_BUFFERS_dataType, flags: PropertyFlags::empty(), value: Value::Int(types as i32) }],
    };
    PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(obj)).map(|(c, _)| c.into_inner()).unwrap_or_default()
}

/// A picture on the card: imported (once per buffer), copied into a texture
/// of ours on the card, which becomes the newest picture; waited for, so the
/// compositor can draw into its buffer again once it's handed back.
#[allow(clippy::too_many_arguments)]
fn take_dmabuf(card: &gpu::Gpu, st: &mut StreamState, latest: &Latest, fd: i64, fourcc: u32, modifier: u64, offset: u64, stride: u64) {
    let (w, h) = (st.width, st.height);
    if !st.imported.contains_key(&fd) {
        // SAFETY: the buffer's fd, valid while PipeWire holds the buffer (it's dup'ed to import).
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd as i32) };
        match card.import(borrowed, w, h, fourcc, modifier, offset, stride) {
            Ok(t) => {
                st.imported.insert(fd, t);
            }
            Err(e) => {
                if !std::mem::replace(&mut st.failed, true) {
                    eprintln!("can't take the picture on the graphics card: {e:#}");
                }
                return;
            }
        }
    }
    let input = &st.imported[&fd];
    let format = input.format();
    let out = latest.spare_texture(w, h, format).unwrap_or_else(|| {
        card.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("screen picture"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        })
    });
    let mut enc = card.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("take picture") });
    enc.copy_texture_to_texture(input.as_image_copy(), out.as_image_copy(), wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 });
    let done = card.queue.submit([enc.finish()]);
    let _ = card.device.poll(wgpu::PollType::Wait { submission_index: Some(done), timeout: Some(Duration::from_secs(1)) });
    latest.store_texture(out);
}

#[cfg(test)]
mod gpu_tests {
    use super::*;

    /// With the graphics card taking pictures, they arrive as textures, as
    /// fast as the screen changes, and read back as the screen.
    #[test]
    #[ignore = "captures the screen on the graphics card: cargo test -p capture pictures_on_the_card -- --ignored"]
    fn pictures_on_the_card() {
        let card = gpu_frames().expect("a graphics card that imports DMA-BUFs");
        let id = super::super::list_screens().into_iter().next().unwrap().id;
        let scr = acquire(&id).unwrap();
        thread::sleep(Duration::from_millis(500));
        let (f, s0) = scr.latest.get().unwrap();
        let texture = f.texture.clone().expect("a picture on the card");
        let t = Instant::now();
        thread::sleep(Duration::from_secs(3));
        let (_, s1) = scr.latest.get().unwrap();
        let rate = (s1 - s0) as f64 / t.elapsed().as_secs_f64();
        let px = card.read(&texture);
        let lit = px.chunks_exact(4).filter(|p| p[0] > 16 || p[1] > 16 || p[2] > 16).count();
        eprintln!("{}x{} {:?}: {rate:.1} pictures/s, {lit} lit pixels", texture.width(), texture.height(), texture.format());
        if let Some(out) = std::env::var_os("HASTECLIP_SHOT") {
            let (w, h) = (texture.width(), texture.height());
            let rgb: Vec<u8> = px.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0]]).collect();
            std::fs::write(out, [format!("P6\n{w} {h}\n255\n").as_bytes(), &rgb].concat()).unwrap();
        }
        assert!(lit > 0, "all black");
    }
}
