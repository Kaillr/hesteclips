//! The screen on GNOME's Wayland session, from Mutter's own screencast
//! service on the session bus (`org.gnome.Mutter.ScreenCast`, what GNOME's
//! portal and its Remote Desktop use underneath): no dialog, and every
//! monitor is known (`org.gnome.Mutter.DisplayConfig`), so they're listed in
//! the app. Mutter checks nothing about who asks (true from GNOME 46 to 51
//! at least); GNOME shows its "sharing the screen" indicator meanwhile.
//!
//! Mutter calls this API private: it can change without notice, so every
//! failure falls back to the portal (`super::screen`).
//!
//! It answers with a PipeWire node on the session's PipeWire, read like the
//! portal's. The stream lives for as long as the bus connection that asked,
//! so that's kept in the [`Cast`]; Mutter's `Closed` signal ends the picture.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use ashpd::zbus::{self, Connection, Proxy, zvariant::OwnedObjectPath, zvariant::OwnedValue, zvariant::Value};
use futures_util::StreamExt;
use futures_util::future::{Either, select};

const SCREEN_CAST: &str = "org.gnome.Mutter.ScreenCast";
const DISPLAY_CONFIG: &str = "org.gnome.Mutter.DisplayConfig";
/// `cursor-mode`: drawn into the picture.
const CURSOR_EMBEDDED: u32 = 1;

/// Whether this is a GNOME Wayland session with Mutter's screencast. Asked
/// once: that doesn't change while the app runs.
pub(crate) fn available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        let gnome = std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|d| d.split(':').any(|d| d.eq_ignore_ascii_case("GNOME")));
        gnome
            && std::env::var_os("WAYLAND_DISPLAY").is_some()
            && pollster::block_on(async {
                let bus = Connection::session().await?;
                let proxy = Proxy::new(&bus, SCREEN_CAST, "/org/gnome/Mutter/ScreenCast", SCREEN_CAST).await?;
                proxy.get_property::<i32>("Version").await
            })
            .is_ok()
    })
}

/// One monitor, as Mutter describes it.
#[derive(Debug, Clone)]
pub(crate) struct Monitor {
    /// `gnome:` and its connector (`gnome:DP-1`), which stays the same while
    /// it's plugged in.
    pub id: String,
    pub connector: String,
    pub name: String,
    pub primary: bool,
    pub width: u32,
    pub height: u32,
    pub x: i32,
    pub y: i32,
}

/// The monitors in use, the main one first. Empty if Mutter can't be asked.
pub(crate) fn monitors() -> Vec<Monitor> {
    pollster::block_on(monitors_async()).inspect_err(|e| eprintln!("GNOME: can't list the monitors: {e:#}")).unwrap_or_default()
}

/// A monitor's description in `GetCurrentState`: connector, vendor,
/// product, serial; its modes; its properties.
type MonitorState = ((String, String, String, String), Vec<ModeState>, HashMap<String, OwnedValue>);
/// A mode: id, width, height, refresh rate, preferred scale, supported scales, properties.
type ModeState = (String, i32, i32, f64, f64, Vec<f64>, HashMap<String, OwnedValue>);
/// A logical monitor: x, y, scale, transform, primary, its monitors, properties.
type LogicalState = (i32, i32, f64, u32, bool, Vec<(String, String, String, String)>, HashMap<String, OwnedValue>);

async fn monitors_async() -> Result<Vec<Monitor>> {
    let bus = Connection::session().await?;
    let proxy = Proxy::new(&bus, DISPLAY_CONFIG, "/org/gnome/Mutter/DisplayConfig", DISPLAY_CONFIG).await?;
    let reply = proxy.call_method("GetCurrentState", &()).await?;
    let (_serial, monitors, logical, _props): (u32, Vec<MonitorState>, Vec<LogicalState>, HashMap<String, OwnedValue>) =
        reply.body().deserialize()?;
    let mut out = Vec::new();
    for (x, y, _scale, _transform, primary, members, _) in logical {
        for (connector, ..) in members {
            let Some((ids, modes, props)) = monitors.iter().find(|(ids, ..)| ids.0 == connector) else { continue };
            // The mode in use is the one marked current; its size is in pixels.
            let current = modes.iter().find(|m| m.6.get("is-current").is_some_and(|v| bool::try_from(v).unwrap_or(false)));
            let Some((_, w, h, ..)) = current else { continue };
            let display = props.get("display-name").and_then(|v| <&str>::try_from(v).ok()).map(str::to_owned);
            let name = match display.filter(|d| !d.is_empty()) {
                Some(d) => format!("{d} ({connector})"),
                None => format!("{connector} ({} {})", ids.1, ids.2).replace(" ( )", ""),
            };
            out.push(Monitor {
                id: format!("gnome:{connector}"),
                connector: connector.clone(),
                name,
                primary,
                width: *w as u32,
                height: *h as u32,
                x,
                y,
            });
        }
    }
    out.sort_by_key(|m| (!m.primary, m.x, m.y));
    Ok(out)
}

/// A Mutter stream of one monitor: its PipeWire node is live for as long as
/// this is held.
pub(crate) struct Cast {
    pub node: u32,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Cast {
    /// Ask Mutter to stream the monitor with this id (the main one if there's
    /// none such). `ended` is called if Mutter stops the stream.
    pub(crate) fn open(id: &str, ended: impl FnOnce(String) + Send + 'static) -> Result<Self> {
        let monitors = monitors();
        let monitor = monitors.iter().find(|m| m.id == id).or(monitors.first()).context("GNOME reports no monitor")?;
        let connector = monitor.connector.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<u32>>();
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        // Everything on one thread, which owns the bus connection (the
        // stream's life) and waits for Mutter to close it.
        let thread = thread::Builder::new().name("mutter-cast".into()).spawn(move || {
            pollster::block_on(async {
                let session = match start(&connector).await {
                    Ok((session, node, closed)) => {
                        let _ = ready_tx.send(Ok(node));
                        Some((session, closed))
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        None
                    }
                };
                let Some((session, mut closed)) = session else { return };
                loop {
                    if stop2.load(Ordering::Relaxed) {
                        break;
                    }
                    let tick = async_io::Timer::after(Duration::from_millis(200));
                    if let Either::Left(_) = select(closed.next(), tick).await {
                        ended("GNOME stopped the screen stream".into());
                        return;
                    }
                }
                let _ = session.call_method("Stop", &()).await;
            });
        })?;
        let node = ready_rx
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| anyhow!("GNOME didn't start the screen stream in time"))??;
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

/// Start streaming the monitor on this connector: the session (to stop it
/// later), the PipeWire node, and the session's `Closed` signals.
async fn start(connector: &str) -> Result<(Proxy<'static>, u32, zbus::proxy::SignalStream<'static>)> {
    let bus = Connection::session().await?;
    let cast = Proxy::new(&bus, SCREEN_CAST, "/org/gnome/Mutter/ScreenCast", SCREEN_CAST).await?;
    let session_path: OwnedObjectPath = cast.call_method("CreateSession", &HashMap::<&str, Value>::new()).await?.body().deserialize()?;
    let session = Proxy::new_owned(bus.clone(), SCREEN_CAST, session_path, format!("{SCREEN_CAST}.Session")).await?;
    let closed = session.receive_signal("Closed").await?;
    let props = HashMap::from([("cursor-mode", Value::from(CURSOR_EMBEDDED)), ("is-recording", Value::from(true))]);
    let stream_path: OwnedObjectPath = session.call_method("RecordMonitor", &(connector, props)).await?.body().deserialize()?;
    let stream = Proxy::new_owned(bus.clone(), SCREEN_CAST, stream_path, format!("{SCREEN_CAST}.Stream")).await?;
    // The node comes in a signal once the session starts: listen first.
    let mut added = stream.receive_signal("PipeWireStreamAdded").await?;
    session.call_method("Start", &()).await?;
    let timeout = async_io::Timer::after(Duration::from_secs(5));
    let node = match select(added.next(), timeout).await {
        Either::Left((Some(msg), _)) => msg.body().deserialize::<u32>()?,
        _ => {
            let _ = session.call_method("Stop", &()).await;
            bail!("GNOME didn't hand over the screen stream");
        }
    };
    Ok((session, node, closed))
}
