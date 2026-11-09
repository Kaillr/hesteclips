//! Desktop Duplication: a display's picture straight from DXGI, as Windows
//! composed it. Unlike Windows Graphics Capture it draws no yellow border
//! around the display on Windows 10 (only Windows 11 can turn that off), and
//! it hands over pictures when asked rather than every time the display
//! refreshes: the compositor does less for it. OBS offers the same choice for
//! display capture.
//!
//! It can only capture a display connected to the graphics card our device is
//! on (the default one), which on a laptop with two often isn't so; and the
//! cursor comes separately, so [`super::cursor`] draws it on, as for the game
//! capture hook. A display that's rotated hands over its picture unrotated,
//! and an HDR one in light (scRGB): both are drawn upright, in SDR.
//!
//! Windows lets a process duplicate each display only once at a time: a
//! preview closing as a recording opens is waited for (see [`START_TIMEOUT`]).
//!
//! A game that goes to the display past the compositor (Minecraft fullscreen,
//! OpenGL, on an NVIDIA card: issue #11) it can't see: the duplication is lost
//! as soon as it starts again, over and over, with no picture. Windows'
//! capture can't see it either, so the game capture hook is used for such a
//! game instead; whether a game is one is told by Desktop Duplication, the
//! recording's (see [`DuplCapture::since_last_picture`]) or one just to look
//! ([`Probe`]).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::{E_ACCESSDENIED, E_INVALIDARG, POINT, RECT};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
use windows::core::Interface;

use super::d3d::{Gpu, Latest, Shared};
use super::quad::{Placement, Quad, fit_within};

/// How a display is captured (see [`crate::ScreenCapture`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Method {
    Duplication,
    Wgc,
}

/// How to capture a display for `choice`, and why (for the log). Automatic is
/// Desktop Duplication where Windows draws a border around a capture it can't
/// turn off (Windows 10, before build 22000), and only where it can work: the
/// display is on our graphics card.
pub(crate) fn choose(choice: crate::ScreenCapture, gpu: &Gpu, monitor: HMONITOR) -> (Method, &'static str) {
    use crate::ScreenCapture::*;
    match choice {
        Wgc => (Method::Wgc, "chosen in Settings"),
        Duplication => (Method::Duplication, "chosen in Settings"),
        Auto if !border_always_shown() => (Method::Wgc, "Windows 11"),
        Auto if output_for(gpu, monitor).is_none() => (Method::Wgc, "the display is on another graphics card"),
        Auto => (Method::Duplication, "Windows 10: no yellow border"),
    }
}

/// Windows Graphics Capture draws its yellow border around a captured display
/// here whatever we ask: Windows 10 (before build 22000, Windows 11).
pub(crate) fn border_always_shown() -> bool {
    windows_build().is_some_and(|b| b < 22000)
}

/// Windows' build number (19045 is Windows 10 22H2), from the system itself:
/// what `GetVersionEx` says depends on the app's manifest.
pub(crate) fn windows_build() -> Option<u32> {
    static BUILD: OnceLock<Option<u32>> = OnceLock::new();
    *BUILD.get_or_init(|| {
        windows_core::link!("ntdll.dll" "system" fn RtlGetVersion(info: *mut windows::Win32::System::SystemInformation::OSVERSIONINFOW) -> i32);
        let mut info = windows::Win32::System::SystemInformation::OSVERSIONINFOW {
            dwOSVersionInfoSize: std::mem::size_of::<windows::Win32::System::SystemInformation::OSVERSIONINFOW>() as u32,
            ..Default::default()
        };
        (unsafe { RtlGetVersion(&mut info) } == 0).then_some(info.dwBuildNumber)
    })
}

/// `monitor`'s output on our graphics card, if it's connected to it.
fn output_for(gpu: &Gpu, monitor: HMONITOR) -> Option<IDXGIOutput> {
    let dxgi: IDXGIDevice = gpu.device.cast().ok()?;
    let adapter = unsafe { dxgi.GetAdapter() }.ok()?;
    (0..).map_while(|i| unsafe { adapter.EnumOutputs(i) }.ok()).find(|o| unsafe { o.GetDesc() }.is_ok_and(|d| d.Monitor == monitor))
}

/// How long starting may wait for the display to be free: Windows lets a
/// process duplicate a display only once at a time, and the one before (a
/// preview's, closing on its own thread as a recording starts) may not have
/// let go yet. A guess; closing takes a few milliseconds.
const START_TIMEOUT: Duration = Duration::from_secs(2);

/// How long the first picture may take. A new duplication hands over the
/// display's picture at once (WebRTC gives it 500 ms); not getting one means
/// it can't see the display.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(2);

/// One duplication of a display, with what drawing its pictures takes.
struct Duplication {
    dupl: IDXGIOutputDuplication,
    /// The display's size as it's shown (rotated), which is the picture's
    /// once drawn upright.
    size: (u32, u32),
    /// Quarter turns clockwise that bring the picture upright.
    turns: u32,
    /// Scale for an HDR display's light (see [`Quad::draw_hdr`]), else none.
    /// Whether a picture is light is told by its format (see [`draw`]).
    hdr: Option<f32>,
    /// Where the display is on the desktop, for the cursor.
    origin: (i32, i32),
}

impl Duplication {
    fn open(gpu: &Gpu, monitor: HMONITOR) -> Result<Self> {
        let output = output_for(gpu, monitor).context("the display isn't connected to the graphics card HasteClip uses")?;
        let desc = unsafe { output.GetDesc() }?;
        // HDR is only told by the newer interfaces (Windows 10 1703 and later).
        let output6 = output.cast::<IDXGIOutput6>().ok();
        let hdr = output6
            .as_ref()
            .and_then(|o| unsafe { o.GetDesc1() }.ok())
            .is_some_and(|d| d.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020);
        let started = Instant::now();
        let dupl = loop {
            let old = || output.cast::<IDXGIOutput1>().and_then(|o1| unsafe { o1.DuplicateOutput(&gpu.device) });
            let tried = match output.cast::<IDXGIOutput5>() {
                // The formats a fullscreen game may scan out in come as they
                // are (an HDR desktop as 16-bit float), not converted to 8-bit
                // by Windows first. Said to need the process per-monitor DPI
                // aware: where it's refused, the old call (always 8-bit).
                Ok(o5) => match unsafe { o5.DuplicateOutput1(&gpu.device, 0, &[DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT]) } {
                    Err(e) if e.code() == DXGI_ERROR_UNSUPPORTED => old(),
                    r => r,
                },
                Err(_) => old(),
            };
            match tried {
                Ok(d) => break d,
                // Still duplicated by our capture before this one, closing.
                Err(e) if e.code() == E_INVALIDARG && started.elapsed() < START_TIMEOUT => thread::sleep(Duration::from_millis(20)),
                Err(e) if e.code() == DXGI_ERROR_NOT_CURRENTLY_AVAILABLE => bail!("too many apps are capturing the screen this way at once (Windows allows four)"),
                Err(e) if e.code() == E_ACCESSDENIED => return Err(e).context("Windows won't let it see the screen right now (a UAC prompt or the lock screen?)"),
                Err(e) if e.code() == DXGI_ERROR_UNSUPPORTED => bail!("Windows can't duplicate this display here ({})", e.message()),
                Err(e) => return Err(e).context("couldn't start Desktop Duplication"),
            }
        };
        let dd = unsafe { dupl.GetDesc() };
        let turns = match dd.Rotation {
            DXGI_MODE_ROTATION_ROTATE90 => 1,
            DXGI_MODE_ROTATION_ROTATE180 => 2,
            DXGI_MODE_ROTATION_ROTATE270 => 3,
            _ => 0,
        };
        let r = desc.DesktopCoordinates;
        let size = ((r.right - r.left).max(2) as u32, (r.bottom - r.top).max(2) as u32);
        let hdr = hdr.then(|| 80.0 / sdr_white_nits(monitor).unwrap_or(80.0));
        Ok(Self { dupl, size, turns, hdr, origin: (r.left, r.top) })
    }
}

/// A running Desktop Duplication of a display, copying its pictures into
/// `latest` on its own thread.
pub(crate) struct DuplCapture {
    stop: Arc<AtomicBool>,
    /// Over: it failed for good (why).
    ended: Arc<Mutex<Option<String>>>,
    /// When the last new picture came up (see [`DuplCapture::since_last_picture`]).
    last_picture: Arc<Mutex<Instant>>,
    thread: Option<JoinHandle<()>>,
}

impl DuplCapture {
    /// Start, and wait for the first picture: a duplication that can't start
    /// or sees nothing fails here, for Windows Graphics Capture to be used
    /// instead.
    pub(crate) fn start(gpu: &Gpu, monitor: HMONITOR, latest: &Arc<Latest>, fps: u32) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let ended = Arc::new(Mutex::new(None));
        let last_picture = Arc::new(Mutex::new(Instant::now()));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let (stop2, ended2, last_picture2, gpu, latest) = (stop.clone(), ended.clone(), last_picture.clone(), gpu.clone(), latest.clone());
        let monitor = Shared(monitor);
        let thread = thread::Builder::new().name("desktop duplication".into()).spawn(move || {
            // On time, like the pacer: a picture taken late is timed late.
            super::system::multimedia_thread("Capture");
            let monitor = monitor;
            let result = run(&gpu, monitor.0, &latest, fps, &stop2, &last_picture2, &ready_tx);
            if let Err(e) = &result {
                eprintln!("desktop duplication: {e:#}");
                let _ = ready_tx.send(Err(format!("{e:#}")));
            }
            *ended2.lock().unwrap() = Some(result.map_or_else(|e| format!("{e:#}"), |()| "stopped".into()));
        })?;
        let capture = Self { stop, ended, last_picture, thread: Some(thread) };
        match ready_rx.recv_timeout(START_TIMEOUT + FIRST_FRAME_TIMEOUT + Duration::from_secs(1)) {
            Ok(Ok(())) => Ok(capture),
            Ok(Err(e)) => bail!("{e}"),
            Err(_) => bail!("Desktop Duplication didn't start"),
        }
    }

    /// The capture is over (it failed and couldn't start again): why.
    pub(crate) fn ended(&self) -> Option<String> {
        self.ended.lock().unwrap().clone()
    }

    /// How long since the display's last new picture (since it started,
    /// before one): the cursor moving brings none, nor does a duplication's
    /// first picture (all there is when it starts again, new or not), nor one
    /// that hasn't lasted [`STEADY`]. A game the duplication can't see brings
    /// none.
    pub(crate) fn since_last_picture(&self) -> Duration {
        self.last_picture.lock().unwrap().elapsed()
    }
}

/// Formats a probe takes pictures in: any a fullscreen game may scan out in
/// (a duplication that doesn't take the one shown is lost over and over).
const PROBE_FORMATS: [DXGI_FORMAT; 4] = [DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_FORMAT_R10G10B10A2_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT];

/// How long a duplication has to have lasted for its pictures to show the
/// display's picture changing. A game that goes to the display past the
/// compositor takes each one away as it starts, ten times a second (each let
/// go of before the next starts, issue #16), and each hands over a picture or
/// two first: that isn't the game seen (measured: 80 a second, from a game
/// showing none). A guess.
const STEADY: Duration = Duration::from_millis(500);

/// Whether the compositor puts new pictures of a display up: a Desktop
/// Duplication of it, asked now and then and never copied from, to tell
/// whether a fullscreen game recorded with Windows' capture reaches it (see
/// `super::Sight`). The cursor moving brings it none, unlike Windows'
/// capture; a game that goes to the display past the compositor brings none
/// either, once lost (see [`STEADY`]; measured with Minecraft frozen in
/// Windows' capture, the cursor moving: no new picture in 5 s; shown, 500 a
/// second).
pub(crate) struct Probe {
    gpu: Gpu,
    output: IDXGIOutput,
    dupl: Option<IDXGIOutputDuplication>,
    /// The duplication's first picture is still to come: all there is when
    /// it starts, not new.
    first: bool,
    /// When the duplication asked started, and whether one was lost before
    /// it (its pictures count once it's lasted [`STEADY`]).
    opened: Instant,
    lost: bool,
    /// When it started, or the last new picture came up, and whether one has.
    last: Instant,
    seen: bool,
}

impl Probe {
    /// Why not, where the display can't be duplicated: it's on another
    /// graphics card, or this app duplicates it already, or as many apps as
    /// Windows allows do. Where Windows refuses for another reason, it's
    /// asked again at each look, and nothing new is seen meanwhile: the game
    /// to be looked at may be why (one going to the display past the
    /// compositor; Windows may refuse the duplication rather than start one
    /// and take it away).
    pub(crate) fn start(gpu: &Gpu, monitor: HMONITOR) -> Result<Self, String> {
        let output = output_for(gpu, monitor).ok_or("the display isn't on the graphics card HasteClip uses")?;
        let dupl = match probe_duplicate(gpu, &output) {
            Ok(d) => Some(d),
            Err(e) if [E_INVALIDARG, DXGI_ERROR_NOT_CURRENTLY_AVAILABLE, DXGI_ERROR_SESSION_DISCONNECTED].contains(&e.code()) => return Err(e.message()),
            Err(_) => None,
        };
        let now = Instant::now();
        Ok(Self { gpu: gpu.clone(), output, lost: dupl.is_none(), dupl, first: true, opened: now, last: now, seen: false })
    }

    /// How long since a new picture came up (since it started, before one),
    /// and whether one has.
    pub(crate) fn since_last_picture(&mut self) -> (Duration, bool) {
        // What's new since the last ask comes as one picture; a few asks, in
        // case one more comes meanwhile.
        for _ in 0..4 {
            let Some(dupl) = &self.dupl else {
                // Lost: started again, to ask next time.
                self.dupl = probe_duplicate(&self.gpu, &self.output).ok();
                self.first = true;
                self.opened = Instant::now();
                self.lost = true;
                break;
            };
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource = None;
            match unsafe { dupl.AcquireNextFrame(0, &mut info, &mut resource) } {
                Ok(()) => {
                    drop(resource);
                    let _ = unsafe { dupl.ReleaseFrame() };
                    // Without a present time, only the cursor moved.
                    if info.LastPresentTime != 0 {
                        if !self.first && (!self.lost || self.opened.elapsed() >= STEADY) {
                            self.last = Instant::now();
                            self.seen = true;
                        }
                        self.first = false;
                    }
                }
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => break,
                Err(_) => self.dupl = None,
            }
        }
        (self.last.elapsed(), self.seen)
    }
}

/// A duplication of `output` for a [`Probe`]: the newer call, which takes
/// any format, wants the thread per-monitor DPI aware; else the old one.
fn probe_duplicate(gpu: &Gpu, output: &IDXGIOutput) -> windows::core::Result<IDXGIOutputDuplication> {
    use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext};
    let previous = unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let dupl = output
        .cast::<IDXGIOutput5>()
        .and_then(|o5| unsafe { o5.DuplicateOutput1(&gpu.device, 0, &PROBE_FORMATS) })
        .or_else(|_| output.cast::<IDXGIOutput1>().and_then(|o1| unsafe { o1.DuplicateOutput(&gpu.device) }));
    unsafe { SetThreadDpiAwarenessContext(previous) };
    dupl
}

impl Drop for DuplCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// How often to ask for a new picture, for `fps` looks a second: four times
/// per look (as Windows Graphics Capture's limit, and the game capture hook's
/// copies), so when each came up is known to a quarter of a look, but at most
/// 2400 a second. Asking hands over only what's new, with when it came up.
fn asks_per_second(fps: u32) -> f64 {
    (4.0 * fps.max(1) as f64).min(2400.0)
}

/// After the display's picture or mode changes under it (a game going
/// exclusive fullscreen, the resolution), how long to keep trying to start
/// again before giving up (see [`reopen`]): Windows' capture stands in then,
/// until Desktop Duplication can start (see `Video::follow_screen`).
const RESTART_FOR: Duration = Duration::from_secs(10);

fn run(gpu: &Gpu, monitor: HMONITOR, latest: &Arc<Latest>, fps: u32, stop: &AtomicBool, last_picture: &Mutex<Instant>, ready: &std::sync::mpsc::Sender<Result<(), String>>) -> Result<()> {
    super::system::com_init();
    // Needed for the newer duplication call, and for the cursor's position
    // in the display's real pixels.
    unsafe {
        windows::Win32::UI::HiDpi::SetThreadDpiAwarenessContext(windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let mut d = Duplication::open(gpu, monitor)?;
    let quad = Quad::new(gpu)?;
    let target = super::quad::target_view(gpu, &latest.texture)?;
    let mut cursor = super::cursor::CursorLayer::new();
    eprintln!(
        "desktop duplication: {}×{}{}{}",
        d.size.0,
        d.size.1,
        if d.turns > 0 { format!(", turned {}°", d.turns * 90) } else { String::new() },
        d.hdr.map_or(String::new(), |s| format!(", HDR (SDR white at {:.0} nits)", 80.0 / s))
    );
    let every = Duration::from_secs_f64(1.0 / asks_per_second(fps));
    // A still screen is asked only often enough that a new picture is noticed
    // in time for its frame (see `BLEND_LATE`): it's timed by when it came
    // up, however late it's noticed. Asking at the full rate finds nothing
    // nearly every time, and each ask costs about 45 µs of a core, waking
    // included (measured: 5% of one at 540 fps, 1% at 60). Still is nothing
    // new (no picture, the cursor still) for a look, and for twice as long
    // as between those asks: a game showing pictures faster than that, two
    // could come between asks and one be lost.
    let still_every = every.max(Duration::from_secs_f64(super::BLEND_LATE / 2.0));
    let still_after = Duration::from_secs_f64(1.0 / fps.max(1) as f64).max(2 * still_every);
    let mut changed = Instant::now();
    let mut next = Instant::now();
    let mut first = true;
    // The picture held (since its last frame was released), so the cursor
    // can be drawn on it again when only the cursor moves.
    let mut held: Option<(ID3D11Texture2D, windows::Win32::Graphics::Direct3D11::ID3D11ShaderResourceView)> = None;
    let mut last_cursor = None;
    // When the last new picture came up, and how long without one before the
    // cursor moving counts as one for frame blending (a still screen): a
    // guess, as for the game capture hook, two frames' looks.
    let mut last_new = 0.0;
    let quiet = 2.0 / fps.max(1) as f64;
    // When this duplication started (again), for its first picture's timeout.
    let mut started = Instant::now();
    // When the duplication stopped working (it was lost, or one started again
    // sent no picture), until one sends a picture again (see `reopen`).
    let mut lost: Option<Instant> = None;
    // Its being lost was said, and its working again isn't yet (once it
    // lasts, see `STEADY`).
    let mut said_lost = false;
    // `HASTECLIP_DUPL_LOG=1`: asks, new pictures, cursor-only updates, since `counted.0`.
    let mut counted = (Instant::now(), 0u32, 0u32, 0u32);
    let log = std::env::var_os("HASTECLIP_DUPL_LOG").is_some();
    while !stop.load(Ordering::Relaxed) {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource = None;
        counted.1 += 1;
        // When a new picture came up, if there's one.
        let mut got = None;
        // Why to start a new duplication, if it's to be.
        let mut again = None;
        // Not waiting for one: a waiting ask holds our device's lock, so the
        // pacer and encoder would wait too (measured: up to the whole wait).
        match unsafe { d.dupl.AcquireNextFrame(0, &mut info, &mut resource) } {
            Ok(()) => {
                let at = super::system::host_now();
                // The first picture of a duplication counts whatever it says:
                // it's all there is until something changes.
                if info.LastPresentTime != 0 || first {
                    // Copied now, while the frame's held: it's Windows' own,
                    // and goes back on release.
                    let copied = match resource.and_then(|r| r.cast::<ID3D11Texture2D>().ok()) {
                        Some(tex) => copy_frame(gpu, &tex, &mut held).map(Some),
                        None => Ok(None),
                    };
                    let _ = unsafe { d.dupl.ReleaseFrame() };
                    if let Some(h) = copied? {
                        held = Some(h);
                        let shown = qpc_seconds(info.LastPresentTime).filter(|&t| t <= at && t > at - 0.1).unwrap_or(at);
                        got = Some(shown);
                        if !first && started.elapsed() >= STEADY {
                            *last_picture.lock().unwrap() = Instant::now();
                        }
                    }
                } else {
                    // Only the hardware cursor changed: drawn below, from
                    // where it is now.
                    let _ = unsafe { d.dupl.ReleaseFrame() };
                }
            }
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                if first && started.elapsed() > FIRST_FRAME_TIMEOUT {
                    // The first duplication can't see the display. One started
                    // again may have come in a hitch (a game going fullscreen,
                    // the screen coming back): another is started.
                    if lost.is_none() {
                        bail!("Desktop Duplication sent no picture");
                    }
                    again = Some("no picture".to_string());
                }
            }
            // The display's picture changed kind (a mode change, a desktop
            // switch, a game going exclusive fullscreen): start again.
            // (Windows words access lost as "The keyed mutex was abandoned.")
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => again = Some("access lost".to_string()),
            Err(e) if e.code() == DXGI_ERROR_INVALID_CALL => again = Some(e.message()),
            Err(e) => return Err(e).context("Desktop Duplication failed"),
        }
        if let Some(why) = again {
            // Said once for a run of them (a game it can't see takes each one
            // away as it starts, ten times a second, see `STEADY`).
            if !said_lost {
                eprintln!("desktop duplication: lost ({why}); starting again");
                said_lost = true;
            }
            // Still lost after as long as starting again may take (each one
            // started is lost too, or sends nothing): given up for now, as
            // when it can't start, for Windows' capture to stand in.
            if lost.is_some_and(|l| l.elapsed() > RESTART_FOR) {
                bail!("Desktop Duplication keeps being lost ({why})");
            }
            // Let go of before another starts: one started while it was still
            // held was lost at its first ask, and so was each one after it,
            // for good (measured after a mode change: a new one every 100 ms,
            // the picture frozen; issue #16).
            drop(d);
            // Not at once when the one before never worked: Windows may be
            // taking each away as it starts.
            if lost.is_some() {
                thread::sleep(Duration::from_millis(100));
            }
            d = match reopen(gpu, monitor, stop, lost.get_or_insert_with(Instant::now))? {
                Some(new) => new,
                None => return Ok(()),
            };
            // Its first picture, whether or not anything changed.
            first = true;
            held = None;
            started = Instant::now();
            continue;
        }
        // The picture goes up when it's new, or when the cursor moved over
        // it: looked at every time, as Windows only says when its own
        // (hardware) cursor moves, not a cursor a game draws in software.
        let mut pos = POINT::default();
        let _ = unsafe { GetCursorPos(&mut pos) };
        let moved = last_cursor != Some((pos.x, pos.y));
        if let Some((texture, view)) = held.as_ref().filter(|_| got.is_some() || moved) {
            last_cursor = Some((pos.x, pos.y));
            changed = Instant::now();
            let at = got.unwrap_or_else(super::system::host_now);
            let drawn = draw(gpu, &quad, &target, latest, texture, view, &d);
            cursor.draw_on_screen(gpu, &quad, latest, &target, d.origin, d.size, drawn);
            // The same picture with the cursor moved only counts as a new one
            // for frame blending once nothing else has changed for a while (a
            // still screen), as with the game capture hook: else a moving
            // cursor would add a picture between the screen's own.
            let counts = got.is_some() || at - last_new > quiet;
            if counts {
                last_new = at;
            }
            latest.mark(drawn, at, counts);
            unsafe { gpu.context.Flush() };
            if got.is_some() {
                counted.2 += 1;
            } else {
                counted.3 += 1;
            }
            if first {
                first = false;
                lost = None;
                let _ = ready.send(Ok(()));
            }
            if said_lost && got.is_some() && started.elapsed() >= STEADY {
                said_lost = false;
                eprintln!("desktop duplication: working again ({}×{})", d.size.0, d.size.1);
            }
        }
        if log && counted.0.elapsed() >= Duration::from_secs(5) {
            let secs = counted.0.elapsed().as_secs_f64();
            eprintln!(
                "desktop duplication: {:.0} asks/s, {:.0} new pictures/s, {:.0} cursor moves/s",
                counted.1 as f64 / secs,
                counted.2 as f64 / secs,
                counted.3 as f64 / secs
            );
            counted = (Instant::now(), 0, 0, 0);
        }
        next += if changed.elapsed() < still_after { every } else { still_every };
        let now = Instant::now();
        if next > now {
            thread::sleep(next - now);
        } else {
            next = now;
        }
    }
    Ok(())
}

/// Start duplicating the display again after it stopped working at `lost`:
/// none if told to stop meanwhile. Gives up after [`RESTART_FOR`], not
/// counting while Windows lets no one see the screen (the lock screen, a UAC
/// prompt, Ctrl+Alt+Del): nothing can capture those (the recording keeps the
/// last picture), and the screen comes back when they go, however long that is.
fn reopen(gpu: &Gpu, monitor: HMONITOR, stop: &AtomicBool, lost: &mut Instant) -> Result<Option<Duplication>> {
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(None);
        }
        match Duplication::open(gpu, monitor) {
            Ok(d) => return Ok(Some(d)),
            Err(e) if denied(&e) => *lost = Instant::now(),
            Err(e) if lost.elapsed() > RESTART_FOR => return Err(e.context("couldn't start Desktop Duplication again")),
            Err(_) => {}
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// Windows lets no one see the screen now (see [`reopen`]).
fn denied(e: &anyhow::Error) -> bool {
    e.downcast_ref::<windows::core::Error>().is_some_and(|w| w.code() == E_ACCESSDENIED)
}

/// Copy a frame of Windows' into a texture of our own (`held`, reused while
/// it's the same size and format).
fn copy_frame(
    gpu: &Gpu,
    frame: &ID3D11Texture2D,
    held: &mut Option<(ID3D11Texture2D, windows::Win32::Graphics::Direct3D11::ID3D11ShaderResourceView)>,
) -> Result<(ID3D11Texture2D, windows::Win32::Graphics::Direct3D11::ID3D11ShaderResourceView)> {
    use windows::Win32::Graphics::Direct3D11::{D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT};
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { frame.GetDesc(&mut desc) };
    let fits = held.as_ref().is_some_and(|(t, _)| {
        let mut have = D3D11_TEXTURE2D_DESC::default();
        unsafe { t.GetDesc(&mut have) };
        (have.Width, have.Height, have.Format) == (desc.Width, desc.Height, desc.Format)
    });
    let (texture, view) = match held.take().filter(|_| fits) {
        Some(h) => h,
        None => {
            let texture = gpu.texture(desc.Width, desc.Height, desc.Format, D3D11_BIND_SHADER_RESOURCE.0 as u32, D3D11_USAGE_DEFAULT, 0)?;
            let view = super::quad::shader_view(gpu, &texture)?;
            (texture, view)
        }
    };
    unsafe { gpu.context.CopyResource(&texture, frame) };
    Ok((texture, view))
}

/// Draw the held picture into `latest`, upright, in SDR: a 16-bit float one
/// is light (an HDR desktop's, or a fullscreen game's that scans out in it),
/// whatever the display said when the duplication started. Shrunk to fit,
/// keeping its shape, when the display has grown bigger than `latest` (its
/// mode changed after the capture started). Its size there.
fn draw(
    gpu: &Gpu,
    quad: &Quad,
    target: &windows::Win32::Graphics::Direct3D11::ID3D11RenderTargetView,
    latest: &Latest,
    texture: &ID3D11Texture2D,
    view: &windows::Win32::Graphics::Direct3D11::ID3D11ShaderResourceView,
    d: &Duplication,
) -> (u32, u32) {
    let mut desc = windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC::default();
    unsafe { texture.GetDesc(&mut desc) };
    let light = desc.Format == DXGI_FORMAT_R16G16B16A16_FLOAT;
    let (w, h) = fit_within(d.size.0, d.size.1, latest.width, latest.height);
    let at = Placement {
        dest: [0.0, 0.0, w as f32, h as f32],
        uv: [0.0, 0.0, 1.0, 1.0],
        clip: RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 },
        turns: d.turns,
    };
    if light {
        // SDR white at white: an SDR desktop's light is 1 at white already.
        quad.draw_hdr(gpu, target, (latest.width, latest.height), view, &at, d.hdr.unwrap_or(1.0));
    } else {
        quad.draw(gpu, target, (latest.width, latest.height), view, &at, true);
    }
    (w, h)
}

/// A QueryPerformanceCounter reading in host seconds (see `host_now`).
fn qpc_seconds(ticks: i64) -> Option<f64> {
    static FREQ: OnceLock<i64> = OnceLock::new();
    let freq = *FREQ.get_or_init(|| {
        let mut f = 0;
        let _ = unsafe { windows::Win32::System::Performance::QueryPerformanceFrequency(&mut f) };
        f
    });
    (freq > 0 && ticks > 0).then(|| (ticks as i128 * 10_000_000 / freq as i128) as f64 / 1e7)
}

/// The SDR white level Windows shows an HDR display's SDR content at, in nits
/// (its "SDR content brightness" slider), by monitor.
fn sdr_white_nits(monitor: HMONITOR) -> Option<f32> {
    use windows::Win32::Devices::Display::*;
    use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITORINFOEXW};
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    unsafe { GetMonitorInfoW(monitor, &mut info as *mut _ as *mut _) }.ok().ok()?;
    let (mut paths_n, mut modes_n) = (0u32, 0u32);
    unsafe { GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut paths_n, &mut modes_n) }.ok().ok()?;
    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); paths_n as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); modes_n as usize];
    unsafe { QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS, &mut paths_n, paths.as_mut_ptr(), &mut modes_n, modes.as_mut_ptr(), None) }.ok().ok()?;
    paths.truncate(paths_n as usize);
    paths.iter().find_map(|p| {
        let mut name = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                size: std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                adapterId: p.sourceInfo.adapterId,
                id: p.sourceInfo.id,
            },
            ..Default::default()
        };
        if unsafe { DisplayConfigGetDeviceInfo(&mut name.header) } != 0 || name.viewGdiDeviceName != info.szDevice {
            return None;
        }
        let mut white = DISPLAYCONFIG_SDR_WHITE_LEVEL {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL,
                size: std::mem::size_of::<DISPLAYCONFIG_SDR_WHITE_LEVEL>() as u32,
                adapterId: p.targetInfo.adapterId,
                id: p.targetInfo.id,
            },
            ..Default::default()
        };
        // In thousandths of 80 nits.
        (unsafe { DisplayConfigGetDeviceInfo(&mut white.header) } == 0 && white.SDRWhiteLevel > 0).then(|| white.SDRWhiteLevel as f32 / 1000.0 * 80.0)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asks_four_times_per_look_up_to_a_limit() {
        assert_eq!(asks_per_second(60), 240.0);
        assert_eq!(asks_per_second(600), 2400.0);
        assert_eq!(asks_per_second(0), 4.0);
    }

    /// The lock screen's refusal is told apart from other failures, through
    /// what `Duplication::open` adds to it.
    #[test]
    fn tells_the_lock_screen_apart() {
        let failed = |code| Err::<(), _>(windows::core::Error::from(code)).context("Windows won't let it see the screen right now").unwrap_err();
        assert!(denied(&failed(E_ACCESSDENIED)));
        assert!(!denied(&failed(E_INVALIDARG)));
        assert!(!denied(&anyhow::anyhow!("Desktop Duplication sent no picture")));
    }

    /// Desktop Duplication that can't start (another of ours duplicates the
    /// display) has Windows' capture stand in, and takes over once it can.
    #[test]
    #[ignore = "captures the screen: cargo test -p capture stands_in_until_duplication_can -- --ignored"]
    fn stands_in_until_duplication_can() {
        unsafe { windows::Win32::UI::HiDpi::SetThreadDpiAwarenessContext(windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        let screen = super::super::system::list_screens().into_iter().next().expect("a screen");
        let gpu = Gpu::new().unwrap();
        let held = Duplication::open(&gpu, super::super::system::find_monitor(&screen.id)).unwrap();
        let video = crate::VideoSource::Screen { id: screen.id, hook: crate::GameHook { auto: false, allowed: Vec::new() }, method: crate::ScreenCapture::Duplication };
        let whole = std::sync::Arc::new(std::sync::Mutex::new(crate::ScreenPlacement::whole(crate::Framing::Fit)));
        let preview = crate::preview::VideoPreview::start(&video, crate::VideoSize { short_side: Some(360), ..Default::default() }, 30, None, None, crate::layers::main_only(), whole);
        let becomes = |want: crate::ScreenCaptureInUse, within: Duration| {
            let t = Instant::now();
            while crate::screen_capture_in_use() != Some(want) {
                assert!(t.elapsed() < within, "still {:?} after {within:?}", crate::screen_capture_in_use());
                thread::sleep(Duration::from_millis(50));
            }
            eprintln!("{want:?} after {:.1?}", t.elapsed());
        };
        becomes(crate::ScreenCaptureInUse { duplication: false, fell_back: true }, Duration::from_secs(10));
        drop(held);
        becomes(crate::ScreenCaptureInUse { duplication: true, fell_back: false }, Duration::from_secs(15));
        drop(preview);
    }

    #[test]
    fn knows_the_windows_build() {
        assert!(windows_build().is_some_and(|b| b >= 10240));
    }
}
