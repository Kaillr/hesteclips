//! Web pages in the video (`LayerSource::Browser`) on Linux: each drawn by
//! Chromium's engine in the page host (`super::web_host`), a process of its
//! own, its frames handed back into a [`Latest`] the layer reads: on the
//! graphics card (imported and copied as the screen's pictures are), or in
//! memory. Their colours come premultiplied by alpha, as Chromium draws
//! them (`super::layers::Pixels::Page`).
//!
//! A page is shared by every capture showing its layer (the preview, a
//! recording), so it loads once and its alerts sound once. Let go of by
//! all, it's kept a few seconds while its layer is still listed (the
//! preview handing over to a recording, or back); taken out, loaded again
//! or changed, it closes at once.
//!
//! The host starts with the first page, once the engine's here
//! (`super::web_engine`), and ends a while after the last. If it stops,
//! it's started again and its pages opened again.

use std::collections::HashMap;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use super::screen::{Frame, Latest, Order};
use super::web_host::{self, MAX_FPS};
use super::web_wire::{MAX_MESSAGE, ToApp, ToHost, Wire};
use super::{gpu, web_engine};
use crate::layers::{LayerSource, SharedLayers};

/// What a page shows: its layer's address, size and style.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Spec {
    /// The layer's id (another after a Reload).
    pub layer: u64,
    pub url: String,
    pub width: u32,
    pub height: u32,
    pub css: String,
}

impl Spec {
    /// Whether `source` (a layer's) is still this page.
    fn is(&self, id: u64, source: &LayerSource) -> bool {
        matches!(source, LayerSource::Browser { url, width, height, css, .. } if id == self.layer && *url == self.url && (*width, *height) == (self.width, self.height) && *css == self.css)
    }
}

/// How long a page let go of by every capture is kept, while its layer's
/// still listed.
const LINGER: Duration = Duration::from_secs(4);
/// How long the host is kept with no pages.
const HOST_IDLE: Duration = Duration::from_secs(30);

/// One page, shared by the captures showing it.
struct Page {
    spec: Spec,
    latest: Latest,
    /// The first of its pictures' numbers.
    seq: u64,
    /// The handles holding it, and the frame rate each wants.
    holders: Mutex<Vec<(u64, u32)>>,
    /// Let go of by all: closed at this time (unless taken up again).
    close_at: Mutex<Option<Instant>>,
    /// As it's open in the host: its id there, its rate, and whether its
    /// frames come on the graphics card.
    open: Mutex<Option<Opened>>,
    /// Whether its frames can come on the card (false once one couldn't be
    /// taken there: it's opened again with frames in memory).
    card: AtomicBool,
    /// The engine's buffers taken in (a few, used in turn): by what they are.
    imports: Mutex<Vec<(ImportKey, wgpu::Texture)>>,
    /// Said why it doesn't load (once each time it's another reason).
    said: Mutex<Option<String>>,
}

#[derive(Clone, Copy, PartialEq)]
struct Opened {
    id: u64,
    fps: u32,
    card: bool,
}

/// A DMA-BUF and how it's laid out: the same buffer handed over again
/// is the same import.
type ImportKey = (u64, u64, u32, u32, u32, u64, u64, u32);

impl Page {
    /// The frame rate it's drawn at: what the fastest capture holding it wants.
    fn fps(&self) -> u32 {
        self.holders.lock().unwrap().iter().map(|h| h.1).max().unwrap_or(30).clamp(1, MAX_FPS)
    }
}

/// A capture's hold on a page: its pictures while it's held.
pub(crate) struct PageHandle {
    page: Arc<Page>,
    id: u64,
    layers: SharedLayers,
}

/// The pages, and the host drawing them.
#[derive(Default)]
struct Pages {
    list: Vec<Arc<Page>>,
    conn: Option<Arc<Conn>>,
    /// The thread looking after them runs.
    looking: bool,
    /// Chromium's sandbox couldn't be made: the host runs without it.
    no_sandbox: bool,
    /// Starts in a row that failed, and when to try again.
    failures: u32,
    retry_at: Option<Instant>,
    /// Since when there have been no pages.
    idle_since: Option<Instant>,
}

static PAGES: Mutex<Option<Pages>> = Mutex::new(None);
static WAKE: Condvar = Condvar::new();

fn pages<R>(f: impl FnOnce(&mut Pages) -> R) -> R {
    f(PAGES.lock().unwrap().get_or_insert_with(Default::default))
}

fn fresh_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

impl PageHandle {
    /// Hold the page `spec` says (opened if no capture holds it), drawn at
    /// up to `fps` frames a second. `layers` is the list it's in: let go
    /// of while it's still there, it's kept a moment for another capture.
    pub(crate) fn open(spec: Spec, fps: u32, layers: SharedLayers) -> Self {
        let id = fresh_id();
        let page = pages(|p| {
            let page = p.list.iter().find(|pg| pg.spec == spec).cloned().unwrap_or_else(|| {
                let page = Arc::new(Page {
                    spec,
                    latest: Latest::default(),
                    seq: super::layers::fresh_seq(),
                    holders: Mutex::new(Vec::new()),
                    close_at: Mutex::new(None),
                    open: Mutex::new(None),
                    card: AtomicBool::new(true),
                    imports: Mutex::new(Vec::new()),
                    said: Mutex::new(None),
                });
                p.list.push(page.clone());
                page
            });
            page.holders.lock().unwrap().push((id, fps));
            *page.close_at.lock().unwrap() = None;
            if !p.looking {
                p.looking = true;
                let _ = thread::Builder::new().name("web pages".into()).spawn(look_after);
            }
            page
        });
        WAKE.notify_all();
        Self { page, id, layers }
    }

    /// Its newest picture, and its number.
    pub(crate) fn picture(&self) -> Option<(Arc<Frame>, u64)> {
        self.page.latest.get().map(|(f, seq)| (f, self.page.seq + seq))
    }
}

impl Drop for PageHandle {
    fn drop(&mut self) {
        let page = &self.page;
        let empty = {
            let mut holders = page.holders.lock().unwrap();
            holders.retain(|h| h.0 != self.id);
            holders.is_empty()
        };
        if empty {
            let listed = self.layers.lock().map(|l| l.iter().any(|l| page.spec.is(l.id, &l.source))).unwrap_or(false);
            *page.close_at.lock().unwrap() = Some(Instant::now() + if listed { LINGER } else { Duration::ZERO });
        }
        WAKE.notify_all();
    }
}

/// The host: its process, and the socket to it.
struct Conn {
    wire: Wire,
    child: Mutex<Child>,
    started: Instant,
    /// It said its engine is up.
    ready: AtomicBool,
    /// It's gone (its socket closed).
    gone: AtomicBool,
    /// Its pages by their ids there.
    by_id: Mutex<HashMap<u64, Arc<Page>>>,
}

impl Conn {
    /// Say `m`; a host that can't take it (stuck) is ended, and started again.
    fn send(&self, m: &ToHost) {
        if self.wire.send(&m.encode(), None).is_err() {
            self.stop();
        }
    }

    /// End it: its socket closed, so it closes its pages and quits.
    fn stop(&self) {
        // SAFETY: our socket; shut down, not closed (the reader holds it).
        unsafe { libc::shutdown(self.wire.fd().as_raw_fd(), libc::SHUT_RDWR) };
    }
}

/// Look after the pages till there are none and the host has ended: close
/// those let go of, start the host (again), open each page in it.
fn look_after() {
    loop {
        let mut guard = PAGES.lock().unwrap();
        let p = guard.get_or_insert_with(Default::default);
        let now = Instant::now();
        // Let go of past their time: closed.
        let (gone, kept): (Vec<_>, Vec<_>) = p.list.drain(..).partition(|pg| pg.holders.lock().unwrap().is_empty() && pg.close_at.lock().unwrap().is_some_and(|t| t <= now));
        p.list = kept;
        for pg in gone {
            if let (Some(conn), Some(o)) = (&p.conn, pg.open.lock().unwrap().take()) {
                conn.send(&ToHost::Close { page: o.id });
                conn.by_id.lock().unwrap().remove(&o.id);
            }
        }
        // The host stopped: its pages open again in the next one, after a
        // moment (longer each time in a row).
        if p.conn.as_ref().is_some_and(|c| c.gone.load(Ordering::Acquire)) {
            let conn = p.conn.take().expect("a host");
            if conn.ready.load(Ordering::Acquire) {
                // It ran, then stopped (a page crashed it): again soon, and
                // later each time it stops soon after starting.
                p.failures = if conn.started.elapsed() > Duration::from_secs(60) { 0 } else { p.failures + 1 };
                p.retry_at = Some(now + Duration::from_secs((1u64 << p.failures.min(5)).min(30)));
            } else if !p.no_sandbox {
                // Chromium ends as it starts where the system can't make its
                // sandbox (Ubuntu's AppArmor rules, say): without it, then.
                p.no_sandbox = true;
                p.retry_at = None;
            } else {
                p.failures += 1;
                p.retry_at = Some(now + Duration::from_secs((1u64 << p.failures.min(5)).min(30)));
                if !matches!(web_engine::status(), web_engine::Engine::Failed(_)) {
                    web_engine::broken("it stops as it starts (its log is web.log, with the app's)");
                }
            }
            for pg in &p.list {
                pg.open.lock().unwrap().take();
                pg.imports.lock().unwrap().clear();
                pg.latest.clear();
            }
        }
        if !p.list.is_empty() {
            p.idle_since = None;
            if p.conn.is_none()
                && p.retry_at.is_none_or(|t| t <= now)
                && let Some(dir) = web_engine::ensure()
            {
                match start_host(&dir, p.no_sandbox) {
                    Ok(conn) => p.conn = Some(conn),
                    Err(e) => {
                        eprintln!("web pages: {e:#}");
                        web_engine::broken(&format!("{e:#}"));
                        p.retry_at = Some(now + Duration::from_secs(30));
                    }
                }
            }
            if let Some(conn) = p.conn.clone().filter(|c| c.ready.load(Ordering::Acquire)) {
                let card_here = gpu::gpu().is_some();
                for pg in &p.list {
                    if pg.holders.lock().unwrap().is_empty() {
                        continue;
                    }
                    let want = Opened { id: 0, fps: pg.fps(), card: card_here && pg.card.load(Ordering::Acquire) };
                    let mut open = pg.open.lock().unwrap();
                    match *open {
                        Some(o) if o.card == want.card && o.fps == want.fps => {}
                        Some(o) if o.card == want.card => {
                            conn.send(&ToHost::Rate { page: o.id, fps: want.fps });
                            open.as_mut().expect("open").fps = want.fps;
                        }
                        _ => {
                            if let Some(o) = open.take() {
                                conn.send(&ToHost::Close { page: o.id });
                                conn.by_id.lock().unwrap().remove(&o.id);
                            }
                            let id = fresh_id();
                            let s = &pg.spec;
                            conn.by_id.lock().unwrap().insert(id, pg.clone());
                            conn.send(&ToHost::Open { page: id, url: s.url.clone(), width: s.width, height: s.height, css: s.css.clone(), fps: want.fps, card: want.card });
                            *open = Some(Opened { id, ..want });
                        }
                    }
                }
            }
        } else {
            let since = *p.idle_since.get_or_insert(now);
            if let Some(conn) = &p.conn
                && now.duration_since(since) >= HOST_IDLE
            {
                conn.stop();
            }
            if p.conn.is_none() {
                p.looking = false;
                return;
            }
        }
        let _ = WAKE.wait_timeout(guard, Duration::from_millis(250));
    }
}

/// Try starting the host again now (the engine's Try again), not after the
/// wait its failures made.
pub(crate) fn retry_now() {
    pages(|p| {
        p.failures = 0;
        p.retry_at = None;
    });
    WAKE.notify_all();
}

/// Start the host on the engine in `dir`, and read what it says.
fn start_host(dir: &Path, no_sandbox: bool) -> Result<Arc<Conn>> {
    let (ours, theirs) = Wire::pair().context("can't make a socket for the web page host")?;
    // The pages' lock is held while the host's told things: never for long.
    ours.send_timeout(Duration::from_secs(2));
    // `HASTECLIP_WEB_HOST_EXE=<the app>`: the host for tests, which run in
    // a binary of their own.
    let exe = match std::env::var_os("HASTECLIP_WEB_HOST_EXE") {
        Some(exe) => exe.into(),
        None => std::env::current_exe().context("no path to the app")?,
    };
    let base = web_engine::data_dir().context("no home folder")?.join("hasteclip");
    let data = base.join("web-pages");
    let _ = std::fs::create_dir_all(&data);
    let _ = std::fs::create_dir_all(base.join("logs"));
    // What Chromium says (much of it noise) goes into a log of its own,
    // with the app's, started afresh each time.
    let log = std::fs::File::create(base.join("logs").join("web.log")).map(Stdio::from).unwrap_or_else(|_| Stdio::null());
    let mut cmd = Command::new(exe);
    cmd.arg(web_host::ARG).env(web_host::ENGINE_ENV, dir).env(web_host::DATA_ENV, &data).stdin(Stdio::null()).stdout(Stdio::null()).stderr(log);
    if no_sandbox {
        cmd.env(web_host::NO_SANDBOX_ENV, "1");
    }
    let raw = theirs.fd().as_raw_fd();
    // SAFETY: between fork and exec only async-signal-safe calls: the
    // host's end of the socket as fd 3, kept open across exec.
    unsafe {
        cmd.pre_exec(move || {
            if raw == web_host::SOCKET_FD {
                if libc::fcntl(raw, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            } else if libc::dup2(raw, web_host::SOCKET_FD) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        })
    };
    let child = cmd.spawn().context("can't start the web page host")?;
    drop(theirs);
    let conn = Arc::new(Conn { wire: ours, child: Mutex::new(child), started: Instant::now(), ready: AtomicBool::new(false), gone: AtomicBool::new(false), by_id: Mutex::new(HashMap::new()) });
    let conn2 = conn.clone();
    thread::Builder::new().name("web pages reader".into()).spawn(move || receive(&conn2)).context("can't read the web page host")?;
    Ok(conn)
}

/// Take what the host says till it's gone: each frame copied (then the
/// host told so), the engine's state for the app to show.
fn receive(conn: &Conn) {
    let mut buf = vec![0u8; MAX_MESSAGE];
    loop {
        let (n, fd) = match conn.wire.recv(&mut buf) {
            Ok(Some(m)) => m,
            Ok(None) => break,
            Err(e) => {
                eprintln!("web pages: {e:#}");
                break;
            }
        };
        let msg = match ToApp::decode(&buf[..n]) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("web pages: {e:#}");
                continue;
            }
        };
        match msg {
            ToApp::Ready => {
                conn.ready.store(true, Ordering::Release);
                web_engine::started();
                WAKE.notify_all();
            }
            ToApp::Failed { why } => {
                eprintln!("web pages: the engine didn't start: {why}");
                web_engine::broken(&why);
            }
            ToApp::Card { page, width, height, visible, fourcc, modifier, offset, stride } => {
                let pg = conn.by_id.lock().unwrap().get(&page).cloned();
                if let (Some(pg), Some(fd)) = (pg, fd)
                    && let Err(e) = take_card(&pg, (width, height), visible, fourcc, modifier, offset, stride, &fd)
                {
                    // Not on the card, then: opened again with frames in memory.
                    if pg.card.swap(false, Ordering::AcqRel) {
                        eprintln!("web page: its frames can't be taken on the graphics card ({e:#}); in memory instead");
                        WAKE.notify_all();
                    }
                }
                conn.send(&ToHost::Done { page });
            }
            ToApp::Memory { page, width, height } => {
                let pg = conn.by_id.lock().unwrap().get(&page).cloned();
                if let (Some(pg), Some(fd)) = (pg, fd) {
                    take_memory(&pg, width, height, &fd);
                }
                conn.send(&ToHost::Done { page });
            }
            ToApp::Loaded { page, error } => {
                if let Some(pg) = conn.by_id.lock().unwrap().get(&page) {
                    let mut said = pg.said.lock().unwrap();
                    if let Some(e) = &error
                        && said.as_ref() != Some(e)
                    {
                        eprintln!("web page {}: {e}", pg.spec.url);
                    }
                    *said = error;
                }
            }
        }
    }
    conn.gone.store(true, Ordering::Release);
    let _ = conn.child.lock().unwrap().wait();
    WAKE.notify_all();
}

/// A frame on the graphics card: its buffer taken in (once: the engine
/// uses a few in turn), the page's part of it copied into a texture of
/// ours, which becomes the page's newest picture; waited for, as the host
/// draws in the buffer again once told.
#[allow(clippy::too_many_arguments)]
fn take_card(page: &Page, (w, h): (u32, u32), visible: [u32; 4], fourcc: u32, modifier: u64, offset: u64, stride: u32, fd: &OwnedFd) -> Result<()> {
    let card = gpu::gpu().context("no graphics card")?;
    // SAFETY: zeroed is a valid stat to fill; our fd.
    let st = unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        libc::fstat(fd.as_raw_fd(), &mut st);
        st
    };
    let key: ImportKey = (st.st_dev, st.st_ino, w, h, fourcc, modifier, offset, stride);
    let mut imports = page.imports.lock().unwrap();
    let at = match imports.iter().position(|(k, _)| *k == key) {
        Some(at) => at,
        None => {
            use std::os::fd::AsFd;
            let texture = card.import(fd.as_fd(), w, h, fourcc, modifier, offset, u64::from(stride))?;
            // A few are plenty: the oldest (another size, say) goes.
            if imports.len() >= 6 {
                imports.remove(0);
            }
            imports.push((key, texture));
            imports.len() - 1
        }
    };
    let input = &imports[at].1;
    let [x, y, vw, vh] = visible;
    let (x, y) = (x.min(w.saturating_sub(1)), y.min(h.saturating_sub(1)));
    let (vw, vh) = (vw.clamp(1, w - x), vh.clamp(1, h - y));
    let format = input.format();
    let out = page.latest.spare_texture(vw, vh, format).unwrap_or_else(|| {
        card.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("web page picture"),
            size: wgpu::Extent3d { width: vw, height: vh, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        })
    });
    let mut enc = card.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("take web page") });
    let from = wgpu::TexelCopyTextureInfo { origin: wgpu::Origin3d { x, y, z: 0 }, ..input.as_image_copy() };
    enc.copy_texture_to_texture(from, out.as_image_copy(), wgpu::Extent3d { width: vw, height: vh, depth_or_array_layers: 1 });
    let done = card.queue.submit([enc.finish()]);
    let _ = card.device.poll(wgpu::PollType::Wait { submission_index: Some(done), timeout: Some(Duration::from_secs(1)) });
    page.latest.store_texture(out);
    Ok(())
}

/// A frame in memory: read from the host's file into the page's newest picture.
fn take_memory(page: &Page, w: u32, h: u32, fd: &OwnedFd) {
    if w == 0 || h == 0 {
        return;
    }
    page.latest.fill(w, h, Order::Bgr, |data| {
        let mut at = 0;
        while at < data.len() {
            // SAFETY: the host's file, a buffer of ours.
            let n = unsafe { libc::pread(fd.as_raw_fd(), data[at..].as_mut_ptr().cast(), data.len() - at, at as libc::off_t) };
            if n <= 0 {
                break;
            }
            at += n as usize;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A page's pixels once its blue square shows (it's loaded): B, G, R, A
    /// rows, and its size.
    fn loaded(h: &PageHandle) -> (Vec<u8>, u32, u32) {
        let until = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some((f, _)) = h.picture() {
                let px = match &f.texture {
                    Some(t) => gpu::gpu().expect("the graphics card").read(t),
                    None => f.data.chunks(f.stride).flat_map(|r| &r[..f.width as usize * 4]).copied().collect(),
                };
                if px.get(..4) == Some(&[255, 0, 0, 255]) {
                    return (px, f.width, f.height);
                }
            }
            assert!(Instant::now() < until, "the page didn't draw");
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn at(px: &[u8], w: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * w + x) * 4) as usize;
        px[i..i + 4].try_into().unwrap()
    }

    /// A page drawn by the engine reaches its layer: its own colours where
    /// it draws, transparent where it's half clear (premultiplied, as
    /// Chromium draws it), its style over the page's. Needs the engine and
    /// the app built: `HASTECLIP_WEB_ENGINE_DIR=<engine> HASTECLIP_WEB_HOST_EXE=<the app>
    /// cargo test -p capture a_page_draws -- --ignored` (with
    /// `HASTECLIP_NO_GPU_CAPTURE=1` too: its frames in memory).
    #[test]
    #[ignore = "needs the web page engine and the app"]
    fn a_page_draws_with_its_transparency() {
        let url = "data:text/html,<html><body style='margin:0;background:rgba(255,0,0,0.5)'><div style='width:100px;height:100px;background:rgb(0,0,255)'></div></body></html>";
        let layers: SharedLayers = Arc::new(Mutex::new(Vec::new()));
        let spec = Spec { layer: 9, url: url.into(), width: 320, height: 240, css: String::new() };
        let page = PageHandle::open(spec.clone(), 30, layers.clone());
        let (px, w, h) = loaded(&page);
        assert_eq!((w, h), (320, 240));
        assert_eq!(at(&px, w, 50, 50), [255, 0, 0, 255], "the square, opaque blue");
        let half = at(&px, w, 200, 200);
        assert!(half[0] <= 1 && half[1] <= 1 && half[2].abs_diff(128) <= 1 && half[3].abs_diff(128) <= 1, "half-clear red, premultiplied: {half:?}");
        // The same page for another capture is the same page.
        let again = PageHandle::open(spec.clone(), 60, layers.clone());
        assert!(Arc::ptr_eq(&page.page, &again.page));
        assert_eq!(page.page.fps(), 60);
        drop(again);
        // Its style goes over its own.
        let styled = PageHandle::open(Spec { layer: 10, css: "body { background: rgb(0, 255, 0) !important; }".into(), ..spec }, 30, layers.clone());
        let (px, w, _) = loaded(&styled);
        assert_eq!(at(&px, w, 200, 200), [0, 255, 0, 255], "the style's green");
        drop((page, styled));
        // Loaded again (Reload: the layer's id changes): another page, which
        // runs its script again, the old one closed at once.
        let random = "data:text/html,<body style='margin:0'><div style='width:100px;height:100px;background:rgb(0,0,255)'></div><script>document.body.style.background='rgb('+[0,0,0].map(()=>Math.floor(Math.random()*256)).join(',')+')'</script></body>";
        let first = PageHandle::open(Spec { layer: 20, url: random.into(), width: 320, height: 240, css: String::new() }, 30, layers.clone());
        let (px, w, _) = loaded(&first);
        let before = at(&px, w, 200, 200);
        let again = PageHandle::open(Spec { layer: 21, url: random.into(), width: 320, height: 240, css: String::new() }, 30, layers.clone());
        drop(first);
        let (px, w, _) = loaded(&again);
        assert_ne!(at(&px, w, 200, 200), before, "loaded again, its script ran again");
        let open = |layer: u64| pages(|p| p.list.iter().any(|pg| pg.spec.layer == layer));
        let until = Instant::now() + Duration::from_secs(5);
        while open(20) {
            assert!(Instant::now() < until, "the page loaded before wasn't closed");
            thread::sleep(Duration::from_millis(50));
        }
        drop(again);
        // Let go of, and not listed: closed at once.
        let until = Instant::now() + Duration::from_secs(5);
        while [9, 10].into_iter().any(open) {
            assert!(Instant::now() < until, "the pages weren't closed");
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// A page that moves comes at the rate asked for, through the host and
    /// the copy each frame waits on (`HASTECLIP_WEB_ENGINE_DIR`,
    /// `HASTECLIP_WEB_HOST_EXE` as above).
    #[test]
    #[ignore = "needs the web page engine and the app"]
    fn a_moving_page_comes_at_its_rate() {
        let url = "data:text/html,<body style='margin:0'><div id=d style='width:100px;height:100px;background:rgb(0,0,255)'></div><script>let x=0;function f(){x=(x+1)&255;d.style.marginLeft=x+'px';requestAnimationFrame(f)};f()</script></body>";
        let layers: SharedLayers = Arc::new(Mutex::new(Vec::new()));
        for fps in [60, 120] {
            let page = PageHandle::open(Spec { layer: 30 + u64::from(fps), url: url.into(), width: 640, height: 360, css: String::new() }, fps, layers.clone());
            let until = Instant::now() + Duration::from_secs(20);
            while page.picture().is_none() {
                assert!(Instant::now() < until, "the page didn't draw");
                thread::sleep(Duration::from_millis(10));
            }
            thread::sleep(Duration::from_millis(500));
            let (start, first) = (Instant::now(), page.picture().unwrap().1);
            thread::sleep(Duration::from_secs(2));
            let rate = (page.picture().unwrap().1 - first) as f64 / start.elapsed().as_secs_f64();
            eprintln!("{fps} fps asked: {rate:.1} came");
            assert!(rate > f64::from(fps) * 0.85, "{fps} fps asked, {rate:.1} came");
        }
    }
}
