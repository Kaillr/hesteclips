//! Windows on the X server, for games and apps (`super::apps`): on an X11
//! session every window, on a Wayland one XWayland's (where Proton and Wine
//! games run, and most games). Listed from the window manager's EWMH
//! properties (`_NET_CLIENT_LIST`, `_NET_ACTIVE_WINDOW`, `_NET_WM_STATE`), a
//! window's process from the server (XRes) or the window (`_NET_WM_PID`).
//!
//! A window is read through XComposite: redirected off-screen, its contents
//! are a pixmap of its own, read into shared memory like the screen
//! (`super::x11`) at the window's size, whatever covers it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use x11rb::connection::Connection;
use x11rb::protocol::composite::{ConnectionExt as _, Redirect};
use x11rb::protocol::res::{ClientIdMask, ClientIdSpec, ConnectionExt as _};
use x11rb::protocol::shm::ConnectionExt as _;
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xproto::{Atom, AtomEnum, ConnectionExt as _, ImageFormat, ImageOrder, Window as Xid};
use x11rb::rust_connection::RustConnection;

use super::apps::Window;
use super::screen::{Latest, Order};

/// Our connection for listing windows, with the atoms it asks about.
struct Lister {
    conn: RustConnection,
    root: Xid,
    atoms: Atoms,
    /// What doesn't change about a window: its process, class and whether
    /// we've looked.
    known: HashMap<Xid, (Option<u32>, String)>,
}

#[derive(Clone, Copy)]
struct Atoms {
    client_list: Atom,
    active: Atom,
    state: Atom,
    hidden: Atom,
    skip_taskbar: Atom,
    wm_pid: Atom,
    wm_name: Atom,
    utf8: Atom,
}

static LISTER: OnceLock<Option<Mutex<Lister>>> = OnceLock::new();

fn lister() -> Option<&'static Mutex<Lister>> {
    LISTER
        .get_or_init(|| {
            let (conn, screen) = x11rb::connect(None).ok()?;
            let root = conn.setup().roots[screen].root;
            let atom = |name: &[u8]| conn.intern_atom(false, name).ok()?.reply().ok().map(|r| r.atom);
            let atoms = Atoms {
                client_list: atom(b"_NET_CLIENT_LIST")?,
                active: atom(b"_NET_ACTIVE_WINDOW")?,
                state: atom(b"_NET_WM_STATE")?,
                hidden: atom(b"_NET_WM_STATE_HIDDEN")?,
                skip_taskbar: atom(b"_NET_WM_STATE_SKIP_TASKBAR")?,
                wm_pid: atom(b"_NET_WM_PID")?,
                wm_name: atom(b"_NET_WM_NAME")?,
                utf8: atom(b"UTF8_STRING")?,
            };
            // XRes and XComposite want our versions first.
            let _ = conn.res_query_version(1, 2).ok()?.reply();
            let composite = conn.composite_query_version(0, 4).ok()?.reply().ok()?;
            if (composite.major_version, composite.minor_version) < (0, 2) {
                return None;
            }
            Some(Mutex::new(Lister { conn, root, atoms, known: HashMap::new() }))
        })
        .as_ref()
}

/// Whether the X server's windows can be listed and read: it has XComposite,
/// and a window manager keeps the EWMH window list.
pub(crate) fn available() -> bool {
    let Some(l) = lister() else { return false };
    let l = l.lock().unwrap();
    l.conn.get_property(false, l.root, l.atoms.client_list, AtomEnum::WINDOW, 0, 0).ok().and_then(|c| c.reply().ok()).is_some_and(|r| r.type_ != 0)
}

/// The windows the window manager lists, as `x11win:<id>`.
pub(crate) fn windows() -> Vec<Window> {
    let Some(l) = lister() else { return Vec::new() };
    let mut l = l.lock().unwrap();
    l.list().unwrap_or_default()
}

impl Lister {
    fn list(&mut self) -> Option<Vec<Window>> {
        let (conn, root, a) = (&self.conn, self.root, self.atoms);
        let ids: Vec<Xid> = conn.get_property(false, root, a.client_list, AtomEnum::WINDOW, 0, 4096).ok()?.reply().ok()?.value32()?.collect();
        let active = conn.get_property(false, root, a.active, AtomEnum::WINDOW, 0, 1).ok()?.reply().ok().and_then(|r| r.value32()?.next()).unwrap_or(0);
        // Every window's state and title asked for before any answer is waited on.
        let asks: Vec<_> = ids
            .iter()
            .map(|&w| (w, conn.get_property(false, w, a.state, AtomEnum::ATOM, 0, 64).ok(), conn.get_property(false, w, a.wm_name, a.utf8, 0, 256).ok()))
            .collect();
        let mut out = Vec::with_capacity(ids.len());
        for (w, state, name) in asks {
            let state: Vec<Atom> = state.and_then(|c| c.reply().ok()).and_then(|r| r.value32().map(Iterator::collect)).unwrap_or_default();
            let title = name.and_then(|c| c.reply().ok()).map(|r| String::from_utf8_lossy(&r.value).into_owned()).unwrap_or_default();
            out.push((w, state, title));
        }
        // Gone windows forgotten; new ones' process and class looked up once.
        self.known.retain(|w, _| ids.contains(w));
        let mut windows = Vec::with_capacity(out.len());
        for (w, state, title) in out {
            let (pid, class) = match self.known.get(&w) {
                Some(k) => k.clone(),
                None => {
                    let k = (self.pid_of(w), self.class_of(w));
                    self.known.insert(w, k.clone());
                    k
                }
            };
            windows.push(Window {
                id: format!("x11win:{w}"),
                pid,
                title,
                app_id: class,
                active: w == active,
                minimized: state.contains(&self.atoms.hidden),
                skip_taskbar: state.contains(&self.atoms.skip_taskbar),
            });
        }
        Some(windows)
    }

    /// The process that made window `w`: as the server knows it, else as
    /// the window says.
    fn pid_of(&self, w: Xid) -> Option<u32> {
        let spec = ClientIdSpec { client: w, mask: ClientIdMask::LOCAL_CLIENT_PID };
        let by_server = self.conn.res_query_client_ids(&[spec]).ok().and_then(|c| c.reply().ok()).and_then(|r| r.ids.into_iter().find_map(|id| id.value.first().copied()));
        by_server.or_else(|| self.conn.get_property(false, w, self.atoms.wm_pid, AtomEnum::CARDINAL, 0, 1).ok()?.reply().ok()?.value32()?.next()).filter(|p| *p != 0)
    }

    /// A window's class (`WM_CLASS`'s second half): what it calls its app.
    fn class_of(&self, w: Xid) -> String {
        let value = self.conn.get_property(false, w, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 256).ok().and_then(|c| c.reply().ok()).map(|r| r.value).unwrap_or_default();
        let mut parts = value.split(|b| *b == 0).filter(|p| !p.is_empty());
        let instance = parts.next().unwrap_or_default();
        String::from_utf8_lossy(parts.next().unwrap_or(instance)).into_owned()
    }
}

/// A connection reading one window.
pub(crate) struct Grab {
    conn: RustConnection,
    root: Xid,
    window: Xid,
    order: Order,
    /// The window's pixmap and its size; named again when the window's resized.
    pixmap: Option<(u32, u16, u16)>,
    shm: Option<super::x11::Shm>,
    pointer: bool,
    last: Vec<u8>,
    last_pointer: Option<(i16, i16, u32)>,
}

impl Grab {
    /// Connect and redirect window `x11win:<id>`, to read it.
    pub(crate) fn open(id: &str) -> Result<Self> {
        let window: Xid = id.strip_prefix("x11win:").and_then(|n| n.parse().ok()).context("not an X11 window")?;
        let (conn, screen) = x11rb::connect(None).context("can't connect to the X server")?;
        let root = conn.setup().roots[screen].root;
        conn.composite_query_version(0, 4)?.reply().context("the X server has no XComposite")?;
        let attrs = conn.get_window_attributes(window)?.reply().context("the window is gone")?;
        let setup = conn.setup();
        let visual = setup.roots.iter().flat_map(|r| &r.allowed_depths).flat_map(|d| &d.visuals).find(|v| v.visual_id == attrs.visual);
        let order = match (visual.map(|v| (v.red_mask, v.blue_mask)), setup.image_byte_order) {
            (Some((0xff0000, 0xff)), ImageOrder::LSB_FIRST) => Order::Bgr,
            (Some((0xff, 0xff0000)), ImageOrder::LSB_FIRST) => Order::Rgb,
            _ => bail!("the window's pixel format can't be recorded"),
        };
        // Its contents kept off-screen for us (and still shown as before).
        conn.composite_redirect_window(window, Redirect::AUTOMATIC)?.check().context("XComposite won't redirect the window")?;
        let pointer = conn.xfixes_query_version(4, 0).ok().and_then(|c| c.reply().ok()).is_some_and(|v| v.major_version >= 1);
        Ok(Self { conn, root, window, order, pixmap: None, shm: None, pointer, last: Vec::new(), last_pointer: None })
    }

    /// Read the window (its pointer drawn in) into `latest`, unless nothing changed.
    fn grab(&mut self, latest: &Latest) -> Result<()> {
        let geo = self.conn.get_geometry(self.window)?.reply().context("the window is gone")?;
        let (w, h) = (geo.width, geo.height);
        if w == 0 || h == 0 {
            return Ok(());
        }
        if self.pixmap.is_none_or(|(_, pw, ph)| (pw, ph) != (w, h)) {
            if let Some((p, ..)) = self.pixmap.take() {
                let _ = self.conn.free_pixmap(p);
            }
            let p = self.conn.generate_id()?;
            self.conn.composite_name_window_pixmap(self.window, p)?.check().context("can't read the window (is it shown?)")?;
            self.pixmap = Some((p, w, h));
            let size = w as usize * h as usize * 4;
            if self.shm.as_ref().is_none_or(|s| s.size < size) {
                self.shm = super::x11::Shm::attach(&self.conn, size).ok();
            }
        }
        let (pixmap, ..) = self.pixmap.unwrap();
        let pointer = if self.pointer { Some(self.conn.xfixes_get_cursor_image()?) } else { None };
        let origin = self.conn.translate_coordinates(self.window, self.root, 0, 0)?;
        let need = w as usize * h as usize * 4;
        let owned;
        let pixels: &[u8] = match &self.shm {
            Some(shm) => {
                self.conn.shm_get_image(pixmap, 0, 0, w, h, !0, ImageFormat::Z_PIXMAP.into(), shm.seg, 0)?.reply()?;
                // SAFETY: mapped while `shm` lives; the server is done writing (it answered).
                unsafe { std::slice::from_raw_parts(shm.addr, need) }
            }
            None => {
                owned = self.conn.get_image(ImageFormat::Z_PIXMAP, pixmap, 0, 0, w, h, !0)?.reply()?.data;
                &owned
            }
        };
        if pixels.len() < need {
            bail!("the X server sent a short picture");
        }
        let pointer = pointer.and_then(|c| c.reply().ok());
        let origin = origin.reply().map(|o| (o.dst_x as i32, o.dst_y as i32)).unwrap_or((geo.x as i32, geo.y as i32));
        let at = pointer.as_ref().map(|p| (p.x, p.y, p.cursor_serial));
        if self.last.as_slice() == pixels && self.last_pointer == at && latest.has_frame() {
            return Ok(());
        }
        self.last.clear();
        self.last.extend_from_slice(pixels);
        self.last_pointer = at;
        let order = self.order;
        latest.fill(w as u32, h as u32, order, |data| {
            data.copy_from_slice(pixels);
            // Opaque: a window's alpha byte can be anything.
            data.chunks_exact_mut(4).for_each(|p| p[3] = 255);
            if let Some(p) = &pointer {
                super::x11::draw_pointer(data, (w as usize, h as usize), order, p, origin);
            }
        });
        Ok(())
    }
}

impl Drop for Grab {
    fn drop(&mut self) {
        if let Some((p, ..)) = self.pixmap.take() {
            let _ = self.conn.free_pixmap(p);
        }
        let _ = self.conn.composite_unredirect_window(self.window, Redirect::AUTOMATIC);
        if let Some(shm) = self.shm.take() {
            let _ = self.conn.shm_detach(shm.seg).map(|c| c.ignore_error());
            let _ = self.conn.flush();
            shm.detach();
        }
        let _ = self.conn.flush();
    }
}

/// Read the window into `latest` until `stop`, each time a picture is
/// wanted (as the screen, `super::x11::run`). It ends when the window's gone.
pub(crate) fn run(mut grab: Grab, _id: &str, latest: &Latest, stop: &AtomicBool) {
    let mut first = true;
    while !stop.load(Ordering::Relaxed) {
        if !std::mem::take(&mut first) && !latest.wait_wanted(Duration::from_millis(100)) {
            continue;
        }
        match grab.grab(latest) {
            Ok(()) => {}
            Err(e) => {
                // Gone: the end. Minimized, it can't be read: its last picture
                // stays until it's back.
                if grab.conn.get_window_attributes(grab.window).ok().and_then(|c| c.reply().ok()).is_none() {
                    latest.end(format!("the window closed: {e:#}"));
                    return;
                }
                grab.pixmap = None;
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
}
