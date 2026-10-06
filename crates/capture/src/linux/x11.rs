//! The screen on an X11 session, read straight from the X server (as
//! ffmpeg's x11grab does): no dialog, since X11 lets any app read the screen,
//! and every monitor is known, so they're listed in the app like on Windows
//! and macOS. Works on any X11 desktop, portal or not (XFCE, Cinnamon, MATE…).
//!
//! Pictures come through shared memory (MIT-SHM) when the server is on this
//! machine: the server writes each one into memory we both see. Without it (a
//! remote display) they come over the connection instead, which is slower.
//! The pointer is drawn in from XFixes, which knows its image and position.
//!
//! On a Wayland session the X server is XWayland, which only sees X11 apps,
//! so the portal is used there instead (`super::portal`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::shm::ConnectionExt as _;
use x11rb::protocol::xfixes::{ConnectionExt as _, GetCursorImageReply};
use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat, ImageOrder};
use x11rb::rust_connection::RustConnection;

use super::screen::{Latest, Order};

/// Whether this is an X11 session, whose screen is read from the X server.
pub(crate) fn session() -> bool {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var("XDG_SESSION_TYPE").is_ok_and(|t| t == "wayland");
    !wayland && std::env::var_os("DISPLAY").is_some()
}

/// One monitor: its part of the root window, in pixels.
#[derive(Debug, Clone)]
pub(crate) struct Monitor {
    /// `x11:` and its RandR name (`x11:DP-1`), which stays the same while
    /// it's plugged in.
    pub id: String,
    pub name: String,
    pub primary: bool,
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
}

/// The monitors, the main one marked. Empty when the X server can't be reached.
pub(crate) fn monitors() -> Vec<Monitor> {
    match x11rb::connect(None) {
        Ok((conn, screen)) => monitors_on(&conn, screen),
        Err(e) => {
            eprintln!("X11: can't connect to the X server: {e}");
            Vec::new()
        }
    }
}

/// The monitor with this id, else the main one, else the first.
pub(crate) fn find(monitors: &[Monitor], id: &str) -> Option<Monitor> {
    monitors.iter().find(|m| m.id == id).or_else(|| monitors.iter().find(|m| m.primary)).or(monitors.first()).cloned()
}

/// RandR's monitors, or the whole screen as one where RandR can't say.
fn monitors_on(conn: &RustConnection, screen: usize) -> Vec<Monitor> {
    let root = &conn.setup().roots[screen];
    let whole = Monitor {
        id: "x11:screen".into(),
        name: "Screen".into(),
        primary: true,
        x: 0,
        y: 0,
        width: root.width_in_pixels,
        height: root.height_in_pixels,
    };
    // RandR wants to know which version we speak before anything else.
    let randr = conn.randr_query_version(1, 5).ok().and_then(|c| c.reply().ok()).filter(|v| (v.major_version, v.minor_version) >= (1, 5));
    let reply = randr.and_then(|_| conn.randr_get_monitors(root.root, true).ok()?.reply().ok());
    let Some(reply) = reply.filter(|r| !r.monitors.is_empty()) else { return vec![whole] };
    let mut out: Vec<Monitor> = Vec::new();
    for m in reply.monitors {
        let name = conn
            .get_atom_name(m.name)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| String::from_utf8_lossy(&r.name).into_owned())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| format!("Monitor {}", out.len() + 1));
        out.push(Monitor { id: format!("x11:{name}"), name, primary: m.primary, x: m.x, y: m.y, width: m.width, height: m.height });
    }
    if !out.iter().any(|m| m.primary) {
        out[0].primary = true;
    }
    out
}

/// A connection reading one monitor.
pub(crate) struct Grab {
    conn: RustConnection,
    root: u32,
    area: Monitor,
    order: Order,
    shm: Option<Shm>,
    /// Whether XFixes can tell us the pointer.
    pointer: bool,
    /// The last picture read (without the pointer) and where the pointer
    /// was, to tell whether anything changed.
    last: Vec<u8>,
    last_pointer: Option<(i16, i16, u32)>,
}

impl Grab {
    /// Connect and get ready to read the monitor with this id (the main one
    /// if there's none such). Fails if the screen's pixels aren't 4 bytes of
    /// RGB, which every desktop today uses.
    pub(crate) fn open(id: &str) -> Result<Self> {
        let (conn, screen) = x11rb::connect(None).context("can't connect to the X server")?;
        let area = find(&monitors_on(&conn, screen), id).context("the X server has no screen")?;
        let setup = conn.setup();
        let root = &setup.roots[screen];
        let bits = setup.pixmap_formats.iter().find(|f| f.depth == root.root_depth).map(|f| f.bits_per_pixel);
        let visual = root.allowed_depths.iter().flat_map(|d| &d.visuals).find(|v| v.visual_id == root.root_visual);
        let order = match (bits, visual.map(|v| (v.red_mask, v.blue_mask)), setup.image_byte_order) {
            (Some(32), Some((0xff0000, 0xff)), ImageOrder::LSB_FIRST) => Order::Bgr,
            (Some(32), Some((0xff, 0xff0000)), ImageOrder::LSB_FIRST) => Order::Rgb,
            _ => bail!("the screen's pixel format ({}-bit) can't be recorded", root.root_depth),
        };
        let root = root.root;
        // XFixes, too, wants our version first.
        let pointer = conn.xfixes_query_version(4, 0).ok().and_then(|c| c.reply().ok()).is_some_and(|v| v.major_version >= 1);
        let size = area.width as usize * area.height as usize * 4;
        let shm = Shm::attach(&conn, size).inspect_err(|e| eprintln!("X11: no shared memory ({e:#}): reading the screen the slow way")).ok();
        Ok(Self { conn, root, area, order, shm, pointer, last: Vec::new(), last_pointer: None })
    }

    /// Read the monitor, pointer drawn in, into `latest` — unless nothing
    /// changed: a still screen then costs a comparison, not a new picture to
    /// scale and encode.
    fn grab(&mut self, latest: &Latest) -> Result<()> {
        let Monitor { x, y, width, height, .. } = self.area;
        let format = ImageFormat::Z_PIXMAP.into();
        // Both asked for before either answer is waited on.
        let pointer = if self.pointer { Some(self.conn.xfixes_get_cursor_image()?) } else { None };
        let owned;
        let pixels: &[u8] = match &self.shm {
            Some(shm) => {
                self.conn.shm_get_image(self.root, x, y, width, height, !0, format, shm.seg, 0)?.reply()?;
                // SAFETY: the segment is mapped for as long as `shm` lives, and the
                // server is done writing it (it has answered).
                unsafe { std::slice::from_raw_parts(shm.addr, shm.size) }
            }
            None => {
                owned = self.conn.get_image(ImageFormat::Z_PIXMAP, self.root, x, y, width, height, !0)?.reply()?.data;
                &owned
            }
        };
        let need = width as usize * height as usize * 4;
        if pixels.len() < need {
            bail!("the X server sent a short picture");
        }
        let pointer = pointer.and_then(|c| c.reply().ok());
        let pixels = &pixels[..need];
        let at = pointer.as_ref().map(|p| (p.x, p.y, p.cursor_serial));
        if self.last.as_slice() == pixels && self.last_pointer == at && latest.has_frame() {
            return Ok(());
        }
        self.last.clear();
        self.last.extend_from_slice(pixels);
        self.last_pointer = at;
        latest.fill(width as u32, height as u32, self.order, |data| {
            data.copy_from_slice(pixels);
            if let Some(p) = &pointer {
                draw_pointer(data, (width as usize, height as usize), self.order, p, (x as i32, y as i32));
            }
        });
        Ok(())
    }
}

/// Read monitor `id` into `latest` until `stop`: once at the start, then
/// each time a picture is wanted ([`Latest::want`]), so the recording gets a
/// fresh one every frame, evenly spaced. If reading fails (a monitor
/// unplugged, the resolution changed) the monitor is looked up again; the
/// picture ends only if that keeps failing.
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
                    latest.end(format!("the screen can't be read anymore: {e:#}"));
                    return;
                }
                thread::sleep(Duration::from_millis(250));
                if let Ok(g) = Grab::open(id) {
                    grab = g;
                }
                first = true;
            }
        }
    }
}

/// Blend the pointer's image (premultiplied ARGB) over the picture of the
/// monitor at `origin`.
fn draw_pointer(data: &mut [u8], (w, h): (usize, usize), order: Order, p: &GetCursorImageReply, origin: (i32, i32)) {
    let left = p.x as i32 - p.xhot as i32 - origin.0;
    let top = p.y as i32 - p.yhot as i32 - origin.1;
    let (ri, bi) = match order {
        Order::Bgr => (2, 0),
        Order::Rgb => (0, 2),
    };
    let (pw, ph) = (p.width as usize, p.height as usize);
    for py in 0..ph {
        let Ok(y) = usize::try_from(top + py as i32) else { continue };
        if y >= h {
            break;
        }
        for px in 0..pw {
            let Ok(x) = usize::try_from(left + px as i32) else { continue };
            if x >= w {
                break;
            }
            let Some(&argb) = p.cursor_image.get(py * pw + px) else { return };
            let a = argb >> 24;
            if a == 0 {
                continue;
            }
            let at = (y * w + x) * 4;
            let out = &mut data[at..at + 4];
            let over = |s: u32, d: u8| (s + (d as u32 * (255 - a) + 127) / 255).min(255) as u8;
            out[ri] = over((argb >> 16) & 0xff, out[ri]);
            out[1] = over((argb >> 8) & 0xff, out[1]);
            out[bi] = over(argb & 0xff, out[bi]);
        }
    }
}

/// A shared memory segment the X server writes pictures into.
struct Shm {
    seg: u32,
    addr: *const u8,
    size: usize,
}

// SAFETY: the mapping is plain memory, read only by the thread that owns the Grab.
unsafe impl Send for Shm {}

impl Shm {
    fn attach(conn: &RustConnection, size: usize) -> Result<Self> {
        if conn.extension_information(x11rb::protocol::shm::X11_EXTENSION_NAME)?.is_none() {
            bail!("the X server doesn't have it");
        }
        // SAFETY: a new private segment, nobody else's.
        let id = unsafe { libc::shmget(libc::IPC_PRIVATE, size, libc::IPC_CREAT | 0o600) };
        if id < 0 {
            bail!("shmget: {}", std::io::Error::last_os_error());
        }
        // SAFETY: attaching the segment just made, wherever the kernel likes.
        let addr = unsafe { libc::shmat(id, std::ptr::null(), libc::SHM_RDONLY) };
        let attached = if addr as isize == -1 {
            Err(anyhow::anyhow!("shmat: {}", std::io::Error::last_os_error()))
        } else {
            (|| -> Result<u32> {
                let seg = conn.generate_id()?;
                conn.shm_attach(seg, id as u32, false)?.check()?;
                Ok(seg)
            })()
        };
        // Removed once both sides let go: the server has it attached by now (or never will).
        // SAFETY: our own segment's id.
        unsafe { libc::shmctl(id, libc::IPC_RMID, std::ptr::null_mut()) };
        match attached {
            Ok(seg) => Ok(Self { seg, addr: addr.cast(), size }),
            Err(e) => {
                if addr as isize != -1 {
                    // SAFETY: attached above, used by nothing.
                    unsafe { libc::shmdt(addr) };
                }
                Err(e)
            }
        }
    }
}

impl Drop for Grab {
    fn drop(&mut self) {
        if let Some(shm) = self.shm.take() {
            let _ = self.conn.shm_detach(shm.seg).map(|c| c.ignore_error());
            let _ = self.conn.flush();
            // SAFETY: mapped in `Shm::attach`; nothing reads it anymore.
            unsafe { libc::shmdt(shm.addr.cast()) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pointer(x: i16, y: i16, image: Vec<u32>, size: u16) -> GetCursorImageReply {
        GetCursorImageReply { sequence: 0, length: 0, x, y, width: size, height: size, xhot: 1, yhot: 1, cursor_serial: 0, cursor_image: image }
    }

    #[test]
    fn pointer_is_blended_at_its_hotspot_and_clipped() {
        let mut data = vec![100u8; 4 * 4 * 4];
        // Opaque red, then half-transparent white (premultiplied), in a 2×2 pointer.
        let p = pointer(10, 10, vec![0xffff0000, 0x80808080, 0, 0], 2);
        // The monitor starts at (9, 9): the hotspot (1, 1) puts the pointer's corner at (0, 0).
        draw_pointer(&mut data, (4, 4), Order::Bgr, &p, (9, 9));
        assert_eq!(&data[0..4], &[0, 0, 255, 100]);
        assert_eq!(&data[4..8], &[178, 178, 178, 100]);
        assert_eq!(&data[8..12], &[100, 100, 100, 100]);
        // Mostly off the bottom-right edge: only the corner pixel lands.
        let mut data = vec![0u8; 4 * 4 * 4];
        draw_pointer(&mut data, (4, 4), Order::Rgb, &pointer(4, 4, vec![0xff0000ff; 4], 2), (0, 0));
        assert_eq!(&data[60..64], &[0, 0, 255, 0]);
        assert_eq!(data.iter().filter(|&&b| b != 0).count(), 1);
    }
}
