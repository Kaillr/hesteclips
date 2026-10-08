//! Game capture through OBS Studio's hook: for games Windows Graphics Capture
//! can't see properly. Exclusive fullscreen OpenGL (osu! stable) never
//! reaches Windows' compositor, so WGC records one frozen frame; Geometry
//! Dash loses its cursor while WGC captures it (a Windows bug).
//!
//! OBS's `graphics-hook64/32.dll` is loaded into the game by its
//! `inject-helper` (through `SetWindowsHookEx`, OBS's "anti-cheat
//! compatibility" way) and copies each frame the game presents (D3D9-12,
//! OpenGL) into a texture shared with us. They're OBS's own files, unchanged
//! and signed, shipped next to the app in `game-hook\` (see
//! `scripts/fetch-game-hook.ps1`) and run as separate programs. We speak
//! their protocol (OBS's `graphics-hook-info.h`, hook version 1): named
//! events, mutexes and shared memory, all ending in the game's process id.
//!
//! The hook doesn't draw the cursor (OBS draws it itself), so neither does
//! this; [`super::cursor`] puts it on.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::{CloseHandle, ERROR_MORE_DATA, ERROR_PIPE_CONNECTED, HANDLE, HWND, RECT, WAIT_ABANDONED, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::System::Memory::{FILE_MAP_ALL_ACCESS, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile};
use windows::Win32::System::Threading::{
    CreateMutexW, EVENT_MODIFY_STATE, IsWow64Process, OpenEventW, OpenMutexW, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE, ReleaseMutex, SYNCHRONIZATION_SYNCHRONIZE, SetEvent, WaitForSingleObject,
};
use windows::Win32::Storage::FileSystem::{CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_GENERIC_WRITE, FILE_SHARE_NONE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadFile};
use windows::Win32::System::Pipes::{ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_MESSAGE, PIPE_TYPE_MESSAGE, PIPE_WAIT};
use windows::Win32::UI::WindowsAndMessaging::{GA_ROOT, GetAncestor, GetWindowThreadProcessId};
use windows::core::PCWSTR;

use super::d3d::{Gpu, Latest};
use super::quad::{Placement, Quad};

/// The hook's files, next to the app (`game-hook\`), or in `target\game-hook`
/// for a development build (`scripts/fetch-game-hook.ps1`).
pub(crate) fn files() -> Option<&'static Path> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        // Next to the app; for a build in target\debug (or its examples\), a
        // folder or two up.
        let exe = std::env::current_exe().ok()?;
        exe.ancestors().skip(1).take(3).map(|d| d.join("game-hook")).find(|d| d.join("graphics-hook64.dll").is_file())
    })
    .as_deref()
}

/// Whether the hook can be used at all (its files are there).
pub fn available() -> bool {
    files().is_some()
}

/// Games known to need the hook, none with anti-cheat: Windows' capture
/// records osu! stable in fullscreen as one frozen frame, and loses the
/// cursor in Geometry Dash.
const NEEDS_HOOK: &[&str] = &["osu!.exe", "GeometryDash.exe"];

/// How to capture a game's window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Plan {
    Hook,
    /// Windows' capture, switching to the hook if its picture freezes.
    Watch,
    Wgc,
}

/// How to capture `exe`'s window (see [`crate::GameHook`]), and why, for the
/// log.
pub(crate) fn plan(exe: &str, window: HWND, settings: &crate::GameHook) -> (Plan, String) {
    if !available() {
        return (Plan::Wgc, "the hook's files aren't there".into());
    }
    // For testing: `HESTECLIPS_FORCE_HOOK=<exe>[,…]` hooks those whatever they are.
    if std::env::var("HESTECLIPS_FORCE_HOOK").is_ok_and(|v| v.split(',').any(|e| e.eq_ignore_ascii_case(exe))) {
        return (Plan::Hook, "forced (HESTECLIPS_FORCE_HOOK)".into());
    }
    if !settings.auto {
        return (Plan::Wgc, "the hook is off".into());
    }
    let allowed = settings.allowed.iter().any(|a| a.eq_ignore_ascii_case(exe));
    let anticheat = super::system::window_app_path(window).and_then(|p| super::anticheat::anticheat(&p));
    if let Some(ac) = anticheat.filter(|_| !allowed) {
        return (Plan::Wgc, format!("uses {ac}"));
    }
    if NEEDS_HOOK.iter().any(|n| n.eq_ignore_ascii_case(exe)) {
        return (Plan::Hook, "a game that needs it".into());
    }
    (Plan::Watch, "no anti-cheat found; hooked if it turns out to be exclusive fullscreen".into())
}

// ---------------------------------------------------------------------------
// OBS's shared structures (graphics-hook-info.h, `#pragma pack(8)`)
// ---------------------------------------------------------------------------

const HOOK_VER_MAJOR: u32 = 1;

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
struct GraphicsOffsets {
    d3d8_present: u32,
    d3d9_present: u32,
    d3d9_present_ex: u32,
    d3d9_present_swap: u32,
    d3d9_clsoff: u32,
    d3d9_is_d3d9ex_clsoff: u32,
    dxgi_present: u32,
    dxgi_resize: u32,
    dxgi_present1: u32,
    ddraw: [u32; 8],
    dxgi2_release: u32,
    d3d12_execute_command_lists: u32,
}

#[repr(C)]
struct HookInfo {
    hook_ver_major: u32,
    hook_ver_minor: u32,
    /// 0: shared memory, 1: shared texture.
    capture_type: u32,
    window: u32,
    format: u32,
    cx: u32,
    cy: u32,
    unused_base_cx: u32,
    unused_base_cy: u32,
    pitch: u32,
    map_id: u32,
    map_size: u32,
    flip: bool,
    frame_interval: u64,
    unused_use_scale: bool,
    force_shmem: bool,
    capture_overlay: bool,
    allow_srgb_alias: bool,
    offsets: GraphicsOffsets,
    reserved: [u32; 126],
}
const _: () = assert!(std::mem::size_of::<HookInfo>() == 648);

#[repr(C)]
struct ShmemData {
    last_tex: i32,
    tex1_offset: u32,
    tex2_offset: u32,
}

/// Where to hook D3D9 and DXGI in this Windows' system DLLs, as OBS's
/// `get-graphics-offsets32/64.exe` say (they change with Windows updates, so
/// asked once a run). OpenGL needs none.
fn offsets(is64: bool) -> GraphicsOffsets {
    static CACHE: OnceLock<Mutex<[Option<GraphicsOffsets>; 2]>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(known) = cache.lock().unwrap()[is64 as usize] {
        return known;
    }
    let found = files()
        .and_then(|dir| {
            let exe = dir.join(if is64 { "get-graphics-offsets64.exe" } else { "get-graphics-offsets32.exe" });
            let out = crate::win::system::hidden_command(&exe).output().ok()?;
            Some(parse_offsets(&String::from_utf8_lossy(&out.stdout)))
        })
        .unwrap_or_default();
    cache.lock().unwrap()[is64 as usize] = Some(found);
    found
}

/// get-graphics-offsets' output: `[section]` then `key=0x…` lines.
fn parse_offsets(text: &str) -> GraphicsOffsets {
    let mut o = GraphicsOffsets::default();
    let mut section = "";
    for line in text.lines().map(str::trim) {
        if let Some(s) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = s;
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        let value = value.trim();
        let Ok(v) = u32::from_str_radix(value.trim_start_matches("0x"), if value.starts_with("0x") { 16 } else { 10 }) else { continue };
        let field = match (section, key.trim()) {
            ("d3d8", "present") => &mut o.d3d8_present,
            ("d3d9", "present") => &mut o.d3d9_present,
            ("d3d9", "present_ex") => &mut o.d3d9_present_ex,
            ("d3d9", "present_swap") => &mut o.d3d9_present_swap,
            ("d3d9", "d3d9_clsoff") => &mut o.d3d9_clsoff,
            ("d3d9", "is_d3d9ex_clsoff") => &mut o.d3d9_is_d3d9ex_clsoff,
            ("dxgi", "present") => &mut o.dxgi_present,
            ("dxgi", "present1") => &mut o.dxgi_present1,
            ("dxgi", "resize") => &mut o.dxgi_resize,
            ("dxgi", "release") => &mut o.dxgi2_release,
            _ => continue,
        };
        *field = v;
    }
    o
}

// ---------------------------------------------------------------------------
// Win32 handles
// ---------------------------------------------------------------------------

/// A handle closed when dropped.
struct Owned(HANDLE);
unsafe impl Send for Owned {}
impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}
impl Owned {
    fn signalled(&self) -> bool {
        unsafe { WaitForSingleObject(self.0, 0) == WAIT_OBJECT_0 }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn open_event(name: &str) -> Option<Owned> {
    let name = wide(name);
    unsafe { OpenEventW(EVENT_MODIFY_STATE | SYNCHRONIZATION_SYNCHRONIZE, false, PCWSTR(name.as_ptr())) }.ok().map(Owned)
}

fn open_mutex(name: &str) -> Option<Owned> {
    let name = wide(name);
    unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, PCWSTR(name.as_ptr())) }.ok().map(Owned)
}

/// A view of named shared memory, unmapped when dropped.
struct View {
    _map: Owned,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
}
unsafe impl Send for View {}
impl View {
    fn open(name: &str, size: usize) -> Option<Self> {
        let w = wide(name);
        let map = Owned(unsafe { OpenFileMappingW(FILE_MAP_ALL_ACCESS.0, false, PCWSTR(w.as_ptr())) }.ok()?);
        let view = unsafe { MapViewOfFile(map.0, FILE_MAP_ALL_ACCESS, 0, 0, size) };
        (!view.Value.is_null()).then_some(Self { _map: map, view })
    }
    fn ptr<T>(&self) -> *mut T {
        self.view.Value.cast()
    }
}
impl Drop for View {
    fn drop(&mut self) {
        let _ = unsafe { UnmapViewOfFile(self.view) };
    }
}

// ---------------------------------------------------------------------------
// The capture
// ---------------------------------------------------------------------------

/// A running hook capture of one game window, copying its frames into
/// `latest` on its own thread.
pub(crate) struct HookCapture {
    stop: Arc<AtomicBool>,
    /// Over: the game closed, or the hook failed (why).
    ended: Arc<Mutex<Option<String>>>,
    thread: Option<JoinHandle<()>>,
}

impl HookCapture {
    pub(crate) fn start(gpu: &Gpu, window: HWND, latest: &Arc<Latest>, fps: u32) -> Result<Self> {
        let dir = files().context("the game capture hook's files aren't there")?.to_path_buf();
        let stop = Arc::new(AtomicBool::new(false));
        let ended = Arc::new(Mutex::new(None));
        let (stop2, ended2, gpu, latest) = (stop.clone(), ended.clone(), gpu.clone(), latest.clone());
        let window = super::d3d::Shared(window);
        let thread = thread::Builder::new().name("game hook".into()).spawn(move || {
            let window = window;
            let result = run(&dir, &gpu, window.0, &latest, fps, &stop2);
            let reason = match result {
                Ok(()) => "stopped".to_owned(),
                Err(e) => {
                    eprintln!("game hook: {e:#}");
                    format!("{e:#}")
                }
            };
            *ended2.lock().unwrap() = Some(reason);
        })?;
        Ok(Self { stop, ended, thread: Some(thread) })
    }

    /// The capture is over (the game closed, or the hook failed): why.
    pub(crate) fn ended(&self) -> Option<String> {
        self.ended.lock().unwrap().clone()
    }
}

impl Drop for HookCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// How long the hook gets to come up in the game. It loads when the game's
/// window thread next handles a message, then waits for the game's next
/// frame. (A guess; OBS retries forever.)
const START_TIMEOUT: Duration = Duration::from_secs(10);

fn run(dir: &Path, gpu: &Gpu, window: HWND, latest: &Arc<Latest>, fps: u32, stop: &AtomicBool) -> Result<()> {
    super::system::com_init();
    // The cursor's position and the window's size in physical pixels.
    unsafe {
        windows::Win32::UI::HiDpi::SetThreadDpiAwarenessContext(windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let mut pid = 0u32;
    let thread_id = unsafe { GetWindowThreadProcessId(window, Some(&mut pid)) };
    if pid == 0 || thread_id == 0 {
        bail!("the game's window is gone");
    }
    let process = Owned(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE, false, pid) }.context("can't open the game's process")?);
    let is64 = {
        let mut wow = windows::core::BOOL(0);
        unsafe { IsWow64Process(process.0, &mut wow) }.context("can't tell whether the game is 32- or 64-bit")?;
        !wow.as_bool()
    };

    // While this mutex exists the hook keeps capturing; it checks every few
    // seconds and stops by itself once it's gone (we closed, or crashed).
    let keepalive = {
        let name = wide(&format!("CaptureHook_KeepAlive{pid}"));
        Owned(unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr())) }.context("can't create the hook's keepalive")?)
    };
    let _log = LogPipe::start(pid);
    let began = Instant::now();

    // Already in the game (captured before, by us or OBS): ask it to start
    // again. Else load it.
    let mut helper = match open_event(&format!("CaptureHook_Restart{pid}")) {
        Some(restart) => {
            unsafe { SetEvent(restart.0) }.context("can't restart the hook")?;
            None
        }
        None => Some(inject(dir, is64, thread_id)?),
    };

    // The hook's objects appear once it's loaded. The helper keeps nudging the
    // game's window thread for a few seconds after (to be sure); no need to
    // wait for it, only to hear if it failed.
    let deadline = Instant::now() + START_TIMEOUT;
    let (mutexes, info) = loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        if let Some(status) = helper.as_mut().and_then(|h| h.try_wait().ok().flatten()) {
            helper = None;
            if status.code() != Some(0) {
                // inject-library.h: -1 OpenProcess, -2…-4 memory, -5 thread,
                // -6 the hook's entry point, -7 SetWindowsHookEx, -8 the
                // window thread never ran it; else a Windows error code.
                bail!("the hook couldn't be loaded into the game (its helper said {:?})", status.code());
            }
        }
        let found = (|| {
            let m1 = open_mutex(&format!("CaptureHook_TextureMutex1{pid}"))?;
            let m2 = open_mutex(&format!("CaptureHook_TextureMutex2{pid}"))?;
            let info = View::open(&format!("CaptureHook_HookInfo{pid}"), std::mem::size_of::<HookInfo>())?;
            Some(([m1, m2], info))
        })();
        if let Some(found) = found {
            eprintln!("game hook: ready in {:.1?}", began.elapsed());
            break found;
        }
        if Instant::now() > deadline {
            if let Some(mut h) = helper {
                let _ = h.kill();
            }
            bail!("the hook didn't load in the game (anti-cheat, or the game runs as administrator?)");
        }
        thread::sleep(Duration::from_millis(50));
    };
    let hook_info = info.ptr::<HookInfo>();
    unsafe {
        (*hook_info).offsets = offsets(is64);
        (*hook_info).capture_overlay = false;
        (*hook_info).force_shmem = false;
        (*hook_info).unused_use_scale = false;
        (*hook_info).allow_srgb_alias = true;
        // Copy at most twice per recorded frame (as OBS does): copying every
        // frame of a game running at 1000 fps would cost it.
        (*hook_info).frame_interval = 1_000_000_000 / (2 * fps.max(1) as u64);
    }
    let event = |name: &str| open_event(&format!("{name}{pid}")).with_context(|| format!("the hook has no {name} event"));
    let (hook_stop, hook_init, hook_ready, hook_exit) = (event("CaptureHook_Stop")?, event("CaptureHook_Initialize")?, event("CaptureHook_HookReady")?, event("CaptureHook_Exit")?);
    unsafe { SetEvent(hook_init.0) }?;

    // Already capturing for someone else (a preview closing as a recording
    // starts): its picture, until the hook says there's a new one.
    let mut source: Option<Source> = if unsafe { (*hook_info).hook_ver_major } != 0 { Source::open(gpu, unsafe { &*hook_info }, window).ok() } else { None };
    if let Some(s) = &source {
        eprintln!("game hook: capturing {}×{} ({}), already running", s.cx, s.cy, s.kind());
    }
    // Nothing yet a few seconds in: the other one may have taken the hook's
    // "ready" signal. Asking it to stop makes it start over, and signal again.
    let mut nudged = false;
    let quad = Quad::new(gpu)?;
    let target = super::quad::target_view(gpu, &latest.texture)?;
    let mut cursor = super::cursor::CursorLayer::new();
    let every = Duration::from_secs_f64(1.0 / (2.0 * fps.max(1) as f64));
    let mut next = Instant::now();
    let result = loop {
        if stop.load(Ordering::Relaxed) {
            break Ok(());
        }
        if process.signalled() || hook_exit.signalled() {
            break Ok(());
        }
        if !super::system::window_alive(window) {
            break Ok(());
        }
        // A new picture to copy from: the first, or after the game resized
        // or reset its graphics.
        if hook_ready.signalled() {
            source = None;
            match Source::open(gpu, unsafe { &*hook_info }, window) {
                Ok(s) => {
                    eprintln!("game hook: capturing {}×{} ({}) after {:.1?}", s.cx, s.cy, s.kind(), began.elapsed());
                    source = Some(s);
                }
                Err(e) if !unsafe { (*hook_info).force_shmem } => {
                    // Likely on another graphics card than ours: through the
                    // CPU instead. Stopping makes the hook start over.
                    eprintln!("game hook: {e:#}; trying shared memory");
                    unsafe { (*hook_info).force_shmem = true };
                    let _ = unsafe { SetEvent(hook_stop.0) };
                }
                Err(e) => break Err(e),
            }
        } else if source.is_none() {
            if Instant::now() > deadline {
                break Err(anyhow::anyhow!("the hook loaded but sent no picture (is the game drawing?)"));
            }
            if !nudged && began.elapsed() > Duration::from_secs(3) {
                nudged = true;
                let _ = unsafe { SetEvent(hook_stop.0) };
            }
        }
        // Minimized or tabbed out: the game draws nothing new, and the away
        // screen (or the last picture) is up instead.
        if let Some(s) = source.as_ref().filter(|_| !super::system::window_hidden(window)) {
            match s.copy(gpu, &quad, &target, latest, &mutexes) {
                Ok(Some(size)) => {
                    cursor.draw(gpu, &quad, latest, &target, window, pid, size);
                    unsafe { gpu.context.Flush() };
                    latest.mark_frame(size);
                }
                Ok(None) => {}
                Err(e) => eprintln!("game hook: {e:#}"),
            }
        }
        next += every;
        let now = Instant::now();
        if next > now {
            thread::sleep(next - now);
        } else {
            next = now;
        }
    };
    let _ = unsafe { SetEvent(hook_stop.0) };
    drop(keepalive);
    result
}

/// Start loading the hook into the game with OBS's helper, the way OBS does
/// by default ("anti-cheat compatibility hook": `SetWindowsHookEx` on the
/// window's thread rather than writing into the game's memory).
fn inject(dir: &Path, is64: bool, thread_id: u32) -> Result<std::process::Child> {
    let helper = dir.join(if is64 { "inject-helper64.exe" } else { "inject-helper32.exe" });
    let dll = dir.join(if is64 { "graphics-hook64.dll" } else { "graphics-hook32.dll" });
    super::system::hidden_command(&helper).arg(&dll).arg("1").arg(thread_id.to_string()).spawn().context("can't run the hook's helper")
}

/// Where the hook puts the game's frames, and how they get into `latest`.
struct Source {
    cx: u32,
    cy: u32,
    /// Upside down (OpenGL's are).
    flip: bool,
    /// The game's picture on our GPU: the texture shared with the game, or
    /// ours that shared memory frames are uploaded to.
    input: ID3D11Texture2D,
    /// To draw it, for pictures a plain copy can't take (another format,
    /// upside down, or bigger than the frame).
    view: Option<ID3D11ShaderResourceView>,
    /// Shared memory frames: two buffers the hook alternates between, rows
    /// `pitch` bytes apart.
    memory: Option<(View, u32)>,
}

impl Source {
    fn kind(&self) -> &'static str {
        match self.memory {
            None => "shared texture",
            Some(_) => "shared memory",
        }
    }

    fn open(gpu: &Gpu, info: &HookInfo, window: HWND) -> Result<Self> {
        if info.hook_ver_major > HOOK_VER_MAJOR {
            bail!("the hook is version {}.{}, newer than this app knows", info.hook_ver_major, info.hook_ver_minor);
        }
        let (cx, cy) = (info.cx, info.cy);
        if cx < 2 || cy < 2 {
            bail!("the game's picture is {cx}×{cy}");
        }
        // Named by the game's top-level window and a counter; the hook may
        // have picked another window of the game than ours.
        let root = unsafe { GetAncestor(window, GA_ROOT) };
        let names = [root.0 as usize as u64, info.window as u64].map(|w| format!("CaptureHook_Texture_{w}_{}", info.map_id));
        let view = names.iter().find_map(|n| View::open(n, info.map_size as usize)).context("can't open the hook's frame")?;
        let (input, memory) = if info.capture_type == 1 {
            let handle = unsafe { *view.ptr::<u32>() } as usize;
            let mut texture: Option<ID3D11Texture2D> = None;
            unsafe { gpu.device.OpenSharedResource(HANDLE(handle as *mut _), &mut texture) }.context("can't open the game's shared texture")?;
            (texture.context("no shared texture")?, None)
        } else {
            let format = DXGI_FORMAT(info.format as i32);
            let upload = gpu.texture(cx, cy, format, D3D11_BIND_SHADER_RESOURCE.0 as u32, D3D11_USAGE_DEFAULT, 0).with_context(|| format!("can't take frames as {format:?}"))?;
            (upload, Some((view, info.pitch)))
        };
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { input.GetDesc(&mut desc) };
        let copyable = matches!(desc.Format, DXGI_FORMAT_B8G8R8A8_UNORM | DXGI_FORMAT_B8G8R8A8_UNORM_SRGB | DXGI_FORMAT_B8G8R8A8_TYPELESS) && !info.flip;
        let view = if copyable { None } else { Some(super::quad::shader_view(gpu, &input)?) };
        Ok(Self { cx, cy, flip: info.flip, input, view, memory })
    }

    /// Copy the newest frame into `latest` (through `target`, its render
    /// target); its size there. None: no new one yet (shared memory, while
    /// the hook writes it).
    fn copy(&self, gpu: &Gpu, quad: &Quad, target: &ID3D11RenderTargetView, latest: &Latest, mutexes: &[Owned; 2]) -> Result<Option<(u32, u32)>> {
        if let Some((view, pitch)) = &self.memory {
            let data = view.ptr::<ShmemData>();
            let last = unsafe { std::ptr::read_volatile(&(*data).last_tex) };
            if !(0..=1).contains(&last) {
                return Ok(None);
            }
            // The hook writes one buffer while we read the other.
            let mutex = &mutexes[last as usize];
            let got = unsafe { WaitForSingleObject(mutex.0, 0) };
            if got != WAIT_OBJECT_0 && got != WAIT_ABANDONED {
                return Ok(None);
            }
            let offset = unsafe { if last == 0 { (*data).tex1_offset } else { (*data).tex2_offset } } as usize;
            unsafe {
                gpu.context.UpdateSubresource(&self.input, 0, None, view.ptr::<u8>().add(offset).cast(), *pitch, 0);
                let _ = ReleaseMutex(mutex.0);
            }
        }
        let (w, h) = fit_within(self.cx, self.cy, latest.width, latest.height);
        match &self.view {
            None if (w, h) == (self.cx, self.cy) => {
                let region = D3D11_BOX { left: 0, top: 0, front: 0, right: w, bottom: h, back: 1 };
                unsafe { gpu.context.CopySubresourceRegion(&latest.texture, 0, 0, 0, 0, &self.input, 0, Some(&region)) };
            }
            view => {
                let view = match view {
                    Some(v) => v.clone(),
                    None => super::quad::shader_view(gpu, &self.input)?,
                };
                let at = Placement {
                    dest: [0.0, 0.0, w as f32, h as f32],
                    uv: if self.flip { [0.0, 1.0, 1.0, 0.0] } else { [0.0, 0.0, 1.0, 1.0] },
                    clip: RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 },
                };
                quad.draw(gpu, target, (latest.width, latest.height), &view, &at, true);
            }
        }
        Ok(Some((w, h)))
    }
}

/// `w`×`h` shrunk (keeping its shape) to fit in `max_w`×`max_h`, if it's bigger.
fn fit_within(w: u32, h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    if w <= max_w && h <= max_h {
        return (w, h);
    }
    let scale = (max_w as f64 / w as f64).min(max_h as f64 / h as f64);
    (((w as f64 * scale) as u32).max(2) & !1, ((h as f64 * scale) as u32).max(2) & !1)
}

/// The hook's log, which it writes to a pipe named after the game's process:
/// passed on to ours (`game hook: …`).
struct LogPipe {
    name: Vec<u16>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl LogPipe {
    fn start(pid: u32) -> Option<Self> {
        let name = wide(&format!(r"\\.\pipe\CaptureHook_Pipe{pid}"));
        // Duplex: the hook opens it for reading and writing.
        let pipe = unsafe {
            CreateNamedPipeW(PCWSTR(name.as_ptr()), PIPE_ACCESS_DUPLEX, PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT, 1, 1024, 1024, 0, None)
        };
        if pipe.is_invalid() {
            return None;
        }
        let pipe = Owned(pipe);
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let thread = thread::Builder::new()
            .name("game hook log".into())
            .spawn(move || {
                let pipe = pipe;
                if let Err(e) = unsafe { ConnectNamedPipe(pipe.0, None) } {
                    // Already connected is fine.
                    if e.code() != ERROR_PIPE_CONNECTED.to_hresult() {
                        return;
                    }
                }
                let mut buf = vec![0u8; 1024];
                let mut message = Vec::new();
                while !stop2.load(Ordering::Relaxed) {
                    let mut read = 0u32;
                    let ok = unsafe { ReadFile(pipe.0, Some(&mut buf), Some(&mut read), None) };
                    message.extend_from_slice(&buf[..read as usize]);
                    match ok {
                        Ok(()) => {
                            if !stop2.load(Ordering::Relaxed) && !message.is_empty() {
                                let text = String::from_utf8_lossy(&message);
                                eprintln!("game hook: {}", text.trim_end_matches(['\0', '\n']));
                            }
                            message.clear();
                        }
                        // More of the same message to come.
                        Err(e) if e.code() == ERROR_MORE_DATA.to_hresult() => {}
                        Err(_) => break,
                    }
                }
            })
            .ok()?;
        Some(Self { name, stop, thread: Some(thread) })
    }
}

impl Drop for LogPipe {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // The thread may be waiting for the hook to connect: connect and hang
        // up to end the wait.
        let me = unsafe {
            CreateFileW(PCWSTR(self.name.as_ptr()), FILE_GENERIC_WRITE.0, FILE_SHARE_NONE, None, OPEN_EXISTING, FILE_FLAGS_AND_ATTRIBUTES(0), None)
        };
        if let Ok(h) = me {
            drop(Owned(h));
        }
        if let Some(t) = self.thread.take() {
            // A hook that stays connected and silent keeps it waiting: let it
            // go rather than hang (it ends when the game closes the pipe).
            let deadline = Instant::now() + Duration::from_millis(200);
            while !t.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
            if t.is_finished() {
                let _ = t.join();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offsets_from_the_tool() {
        let o = parse_offsets(
            "[d3d8]\npresent=0x0\n[d3d9]\npresent=0x61c40\npresent_ex=0x1c6f0\npresent_swap=0x80a90\nd3d9_clsoff=0x4030\nis_d3d9ex_clsoff=0x55a0\n[dxgi]\npresent=0x19000\npresent1=0x194a0\nresize=0x36c30\nrelease=0x335a0\n",
        );
        assert_eq!(o.d3d9_present, 0x61c40);
        assert_eq!(o.d3d9_is_d3d9ex_clsoff, 0x55a0);
        assert_eq!(o.dxgi_resize, 0x36c30);
        assert_eq!(o.dxgi2_release, 0x335a0);
        assert_eq!(o.d3d8_present, 0);
    }

    #[test]
    fn hook_info_layout() {
        assert_eq!(std::mem::offset_of!(HookInfo, flip), 48);
        assert_eq!(std::mem::offset_of!(HookInfo, frame_interval), 56);
        assert_eq!(std::mem::offset_of!(HookInfo, offsets), 68);
        assert_eq!(std::mem::size_of::<GraphicsOffsets>(), 76);
    }

    #[test]
    fn fits() {
        assert_eq!(fit_within(1920, 1080, 2560, 1440), (1920, 1080));
        assert_eq!(fit_within(3840, 2160, 1920, 1080), (1920, 1080));
    }
}
