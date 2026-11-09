//! The web page host: the app started again with [`ARG`], a process of its
//! own running Chromium's engine (CEF, `super::cef`). It draws each page
//! the app opens off screen and hands its frames back over the socket
//! (`super::web_wire`): on the graphics card as DMA-BUFs, else in shared
//! memory, each waited on until the app has copied it (the engine draws the
//! next one in the same buffer). CEF's own processes (renderers, the
//! graphics card's, utilities) are the app started again too, with
//! Chromium's `--type=…` ([`helper`] runs them).
//!
//! A process of its own, so a page that crashes or hangs Chromium takes the
//! host down, never a recording: the app starts it again (`super::web`).

use std::ffi::{CString, OsStr, c_char, c_int, c_uint, c_void};
use std::mem::{offset_of, size_of};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

use super::cef::*;
use super::gpu::{DRM_ABGR8888, DRM_ARGB8888};
use super::web_wire::{MAX_MESSAGE, ToApp, ToHost, Wire};

/// The argument the app starts its host with.
pub(crate) const ARG: &str = "--hasteclip-web-host";
/// The engine's folder (libcef.so and its files), for the host and CEF's
/// own processes. Set for the app too, it's the engine used instead of the
/// one fetched (`super::web_engine`).
pub(crate) const ENGINE_ENV: &str = "HASTECLIP_WEB_ENGINE_DIR";
/// Where pages keep their cookies and storage.
pub(crate) const DATA_ENV: &str = "HASTECLIP_WEB_DATA";
/// Set: without Chromium's sandbox (where the system can't make one).
pub(crate) const NO_SANDBOX_ENV: &str = "HASTECLIP_WEB_NO_SANDBOX";
/// The fd the socket to the app is handed over as.
pub(crate) const SOCKET_FD: c_int = 3;

/// The most frames a second a page is drawn at.
pub(crate) const MAX_FPS: u32 = 240;

/// How long the engine waits for the app to copy a frame before drawing
/// the next one anyway (the app's stuck: a torn frame beats a hung page).
const COPY_WAIT: Duration = Duration::from_millis(500);

/// Run as the web page host or one of CEF's processes, if that's what this
/// process was started as (returns true once it's done, and the process
/// should exit); else nothing (false). First thing at launch.
pub fn helper() -> bool {
    if std::env::args_os().nth(1).as_deref() == Some(OsStr::new(ARG)) {
        let code = match run() {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("web pages: {e:#}");
                1
            }
        };
        std::process::exit(code);
    }
    let Some(dir) = std::env::var_os(ENGINE_ENV) else { return false };
    if !std::env::args_os().any(|a| a.as_bytes().starts_with(b"--type=")) {
        return false;
    }
    // One of CEF's own processes: it runs in here till it's done.
    let code = match Lib::load(&Path::new(&dir).join("libcef.so")) {
        // SAFETY: CEF's entry point, with this process's arguments.
        Ok(lib) => unsafe { (lib.execute_process)(main_args(), null_mut(), null_mut()) },
        Err(e) => {
            eprintln!("web pages: can't load the engine: {e:#}");
            1
        }
    };
    std::process::exit(code.max(0));
}

/// This process's arguments as CEF takes them, kept for its life.
fn main_args() -> &'static cef_main_args_t {
    struct Args(cef_main_args_t);
    // SAFETY: the pointers are into strings leaked with it, never changed.
    unsafe impl Send for Args {}
    unsafe impl Sync for Args {}
    static ARGS: OnceLock<Args> = OnceLock::new();
    &ARGS
        .get_or_init(|| {
            let strings: Vec<CString> = std::env::args_os().filter_map(|a| CString::new(a.as_bytes()).ok()).collect();
            let mut ptrs: Vec<*mut c_char> = strings.iter().map(|s| s.as_ptr().cast_mut()).collect();
            let argc = ptrs.len() as c_int;
            ptrs.push(null_mut());
            let argv = Box::leak(ptrs.into_boxed_slice()).as_mut_ptr();
            std::mem::forget(strings);
            Args(cef_main_args_t { argc, argv })
        })
        .0
}

/// The host, once it runs.
struct Host {
    lib: Lib,
    wire: Wire,
    pages: Mutex<Vec<Arc<HostPage>>>,
    /// The app has gone: every page closes, then the host ends.
    quitting: AtomicBool,
}

static HOST: OnceLock<Host> = OnceLock::new();

fn host() -> &'static Host {
    HOST.get().expect("the web page host runs")
}

/// A page open in the host.
struct HostPage {
    id: u64,
    size: (u32, u32),
    /// The script adding the page's style, if it has one.
    style: Option<String>,
    /// Its browser once made, whose reference is ours till it closes. Read
    /// and written on CEF's UI thread only.
    browser: AtomicPtr<cef_browser_t>,
    /// The app closed it (before its browser was made, perhaps).
    closing: AtomicBool,
    /// How often its renderer has stopped (it's loaded again a few times).
    crashes: AtomicUsize,
    /// Whether the frame sent last has been copied, and the signal it has.
    copied: Mutex<bool>,
    copy_done: Condvar,
    /// Frames in memory go through this file, made as the first one comes.
    memory: Mutex<Option<OwnedFd>>,
}

impl HostPage {
    /// Close its browser (its frames stop; it's gone once CEF says so).
    /// On CEF's UI thread.
    fn close(&self) {
        self.closing.store(true, Ordering::Relaxed);
        let b = self.browser.load(Ordering::Relaxed);
        if b.is_null() {
            return;
        }
        // SAFETY: a browser we hold a reference to, on CEF's UI thread.
        unsafe {
            let h = ((*b).get_host.expect("get_host"))(b);
            if !h.is_null() {
                ((*h).close_browser.expect("close_browser"))(h, 1);
                release(h);
            }
        }
    }

    /// Its browser's host, for a call on it, and let go of after.
    fn with_host(&self, f: impl FnOnce(*mut cef_browser_host_t)) {
        let b = self.browser.load(Ordering::Relaxed);
        if b.is_null() {
            return;
        }
        // SAFETY: a browser we hold a reference to, on CEF's UI thread.
        unsafe {
            let h = ((*b).get_host.expect("get_host"))(b);
            if !h.is_null() {
                f(h);
                release(h);
            }
        }
    }
}

/// Run the host: the engine up, every page the app asks for drawn, till
/// the app goes.
fn run() -> Result<()> {
    // SAFETY: the app handed the socket over as this fd; checked to be one.
    let socket = unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        if libc::fstat(SOCKET_FD, &mut st) != 0 || st.st_mode & libc::S_IFMT != libc::S_IFSOCK {
            bail!("started without the app's socket");
        }
        libc::fcntl(SOCKET_FD, libc::F_SETFD, libc::FD_CLOEXEC);
        OwnedFd::from_raw_fd(SOCKET_FD)
    };
    let wire = Wire::from_fd(socket);
    let failed = |wire: &Wire, why: String| {
        let _ = wire.send(&ToApp::Failed { why: why.clone() }.encode(), None);
        anyhow!(why)
    };
    let dir = PathBuf::from(std::env::var_os(ENGINE_ENV).context("no engine folder")?);
    let lib = match Lib::load(&dir.join("libcef.so")) {
        Ok(l) => l,
        Err(e) => {
            let why = format!("{e:#}");
            // A library of the system's it links that isn't installed.
            let why = match why.split_once(": cannot open shared object file") {
                Some((lib, _)) => format!("it needs {lib}, which isn't installed (your distribution's packages have it)"),
                None => why,
            };
            return Err(failed(&wire, why));
        }
    };
    if HOST.set(Host { lib, wire, pages: Mutex::new(Vec::new()), quitting: AtomicBool::new(false) }).is_err() {
        bail!("the host started twice");
    }
    let host = host();
    let app = app();
    // SAFETY: CEF's entry point: in the browser process (no --type) it
    // returns -1 at once.
    let code = unsafe { (host.lib.execute_process)(main_args(), app, null_mut()) };
    if code >= 0 {
        std::process::exit(code);
    }
    let env = |k: &str| std::env::var(k).unwrap_or_default();
    let exe = std::env::current_exe().context("no path to the app")?;
    // Chromium's log goes where the host's errors do (the app gives it a
    // file of its own: `super::web`), not into a file of its own beside the
    // app, which it makes when it's given none.
    let strings = [exe.to_string_lossy().into_owned(), env(DATA_ENV), "/dev/null".to_owned(), dir.to_string_lossy().into_owned(), dir.join("locales").to_string_lossy().into_owned()];
    let [exe, data, log, resources, locales] = strings.map(|s| Utf16::new(&s));
    let locale = Utf16::new("en-US");
    // SAFETY: all zeros is CEF's default for every setting.
    let mut s: cef_settings_t = unsafe { std::mem::zeroed() };
    s.size = size_of::<cef_settings_t>();
    s.no_sandbox = c_int::from(std::env::var_os(NO_SANDBOX_ENV).is_some());
    s.browser_subprocess_path = exe.header();
    s.windowless_rendering_enabled = 1;
    s.root_cache_path = data.header();
    s.cache_path = data.header();
    s.persist_session_cookies = 1;
    s.locale = locale.header();
    s.log_file = log.header();
    s.log_severity = LOGSEVERITY_ERROR;
    s.resources_dir_path = resources.header();
    s.locales_dir_path = locales.header();
    // SAFETY: settings and strings alive for the call; CEF copies them.
    if unsafe { (host.lib.initialize)(main_args(), &s, app, null_mut()) } != 1 {
        return Err(failed(&host.wire, "its log is web.log, with the app's".into()));
    }
    host.wire.send(&ToApp::Ready.encode(), None).context("the app has gone")?;
    thread::Builder::new().name("web host reader".into()).spawn(read)?;
    // SAFETY: on the thread CEF was started on, till a page task quits it.
    unsafe { (host.lib.run_message_loop)() };
    // SAFETY: every browser is closed by now (or the watchdog ends us).
    unsafe { (host.lib.shutdown)() };
    Ok(())
}

/// Read what the app says, till it goes: frames copied are told to the page
/// waiting on them, everything else is done on CEF's UI thread.
fn read() {
    let host = host();
    let mut buf = vec![0u8; MAX_MESSAGE];
    loop {
        match host.wire.recv(&mut buf) {
            Ok(Some((n, _))) => match ToHost::decode(&buf[..n]) {
                Ok(ToHost::Done { page }) => {
                    if let Some(p) = host.pages.lock().unwrap().iter().find(|p| p.id == page) {
                        *p.copied.lock().unwrap() = true;
                        p.copy_done.notify_all();
                    }
                }
                Ok(m) => post(move || command(m)),
                Err(e) => eprintln!("web pages: {e:#}"),
            },
            Ok(None) | Err(_) => {
                post(quit);
                // Whatever doesn't close in time, the process ends anyway.
                thread::sleep(Duration::from_secs(5));
                // SAFETY: ending the process; nothing more of ours runs.
                unsafe { libc::_exit(0) };
            }
        }
    }
}

/// Do what the app asked. On CEF's UI thread.
fn command(m: ToHost) {
    let host = host();
    match m {
        ToHost::Open { page, url, width, height, css, fps, card } => open(page, &url, (width, height), &css, fps, card),
        ToHost::Rate { page, fps } => {
            // Found, then let go of the list (CEF's calls back can take it).
            let p = host.pages.lock().unwrap().iter().find(|p| p.id == page).cloned();
            if let Some(p) = p {
                // SAFETY: a browser host, on CEF's UI thread.
                p.with_host(|h| unsafe { ((*h).set_windowless_frame_rate.expect("set_windowless_frame_rate"))(h, fps.clamp(1, MAX_FPS) as c_int) });
            }
        }
        ToHost::Close { page } => {
            let p = host.pages.lock().unwrap().iter().find(|p| p.id == page).cloned();
            if let Some(p) = p {
                p.close();
            }
        }
        ToHost::Done { .. } => {}
    }
}

/// Close every page, then end: the app has gone. On CEF's UI thread.
fn quit() {
    let host = host();
    if host.quitting.swap(true, Ordering::AcqRel) {
        return;
    }
    let pages = host.pages.lock().unwrap().clone();
    if pages.is_empty() {
        // SAFETY: on CEF's UI thread.
        unsafe { (host.lib.quit_message_loop)() };
    }
    for p in pages {
        p.close();
    }
}

/// Open page `id`: its browser, off screen, transparent where the page is.
/// On CEF's UI thread.
fn open(id: u64, url: &str, size: (u32, u32), css: &str, fps: u32, card: bool) {
    let host = host();
    let size = (size.0.clamp(16, 7680), size.1.clamp(16, 4320));
    let page = Arc::new(HostPage {
        id,
        size,
        style: crate::layers::style_script(css),
        browser: AtomicPtr::new(null_mut()),
        closing: AtomicBool::new(false),
        crashes: AtomicUsize::new(0),
        copied: Mutex::new(true),
        copy_done: Condvar::new(),
        memory: Mutex::new(None),
    });
    host.pages.lock().unwrap().push(page.clone());
    let client = PageClient::make(page.clone());
    // SAFETY: all zeros is CEF's default for every setting.
    let mut window: cef_window_info_t = unsafe { std::mem::zeroed() };
    window.size = size_of::<cef_window_info_t>();
    window.windowless_rendering_enabled = 1;
    window.shared_texture_enabled = c_int::from(card);
    window.runtime_style = CEF_RUNTIME_STYLE_ALLOY;
    // SAFETY: as above.
    let mut settings: cef_browser_settings_t = unsafe { std::mem::zeroed() };
    settings.size = size_of::<cef_browser_settings_t>();
    settings.windowless_frame_rate = fps.clamp(1, MAX_FPS) as c_int;
    // Transparent where the page is: blended over what's under it.
    settings.background_color = 0;
    // A page can't close itself (its layer would go blank).
    settings.javascript_close_windows = STATE_DISABLED;
    let url = Utf16::new(url);
    // SAFETY: CEF's function, with structures and strings alive for the call;
    // the client goes over with a reference of its own, ours let go of after.
    let made = unsafe {
        add_ref(client);
        let made = (host.lib.browser_host_create_browser)(&window, client.cast(), url.raw(), &settings, null_mut(), null_mut());
        release(client);
        made
    };
    if made != 1 {
        host.pages.lock().unwrap().retain(|p| p.id != id);
        let _ = host.wire.send(&ToApp::Loaded { page: id, error: Some("Chromium couldn't open it".into()) }.encode(), None);
    }
}

/// Send a frame of `page`'s, and wait till the app has copied it.
fn send_frame(page: &HostPage, msg: &ToApp, fd: BorrowedFd<'_>) {
    let host = host();
    if host.quitting.load(Ordering::Relaxed) {
        return;
    }
    *page.copied.lock().unwrap() = false;
    if host.wire.send(&msg.encode(), Some(fd)).is_err() {
        return;
    }
    let copied = page.copied.lock().unwrap();
    let _ = page.copy_done.wait_timeout_while(copied, COPY_WAIT, |c| !*c);
}

// ---------------------------------------------------------------------------
// CEF's objects of ours
// ---------------------------------------------------------------------------

/// The app (CEF's `cef_app_t`): one, never let go of.
fn app() -> *mut cef_app_t {
    struct App(cef_app_t);
    // SAFETY: CEF only reads it; it's never changed after it's made.
    unsafe impl Send for App {}
    unsafe impl Sync for App {}
    static APP: OnceLock<App> = OnceLock::new();
    let app = APP.get_or_init(|| {
        // SAFETY: all zeros: every method not handled.
        let mut app: cef_app_t = unsafe { std::mem::zeroed() };
        app.base = static_base(size_of::<cef_app_t>());
        app.on_before_command_line_processing = Some(command_line);
        App(app)
    });
    (&raw const app.0).cast_mut()
}

unsafe extern "C" fn static_add_ref(_: *mut cef_base_ref_counted_t) {}
unsafe extern "C" fn static_release(_: *mut cef_base_ref_counted_t) -> c_int {
    0
}
unsafe extern "C" fn static_has_ref(_: *mut cef_base_ref_counted_t) -> c_int {
    1
}

/// The base of an object that lives as long as the process.
fn static_base(size: usize) -> cef_base_ref_counted_t {
    cef_base_ref_counted_t { size, add_ref: Some(static_add_ref), release: Some(static_release), has_one_ref: Some(static_has_ref), has_at_least_one_ref: Some(static_has_ref) }
}

/// Chromium's switches for the browser process: pages keep drawing and
/// running their timers off screen, play videos and sounds without a click
/// (alerts), never ask the desktop's keyring for a password, and never
/// download Chromium's own components or take up the system's browser
/// extensions (KDE's Plasma integration installs one for every Chromium).
unsafe extern "C" fn command_line(_: *mut cef_app_t, process_type: *const cef_string_t, line: *mut cef_command_line_t) {
    // SAFETY: CEF's string and command line, valid for the call.
    unsafe {
        if cef_string_t::read(process_type).is_empty() {
            let set = |name: &str, value: Option<&str>| {
                let name = Utf16::new(name);
                match value {
                    Some(v) => ((*line).append_switch_with_value.expect("append_switch_with_value"))(line, name.raw(), Utf16::new(v).raw()),
                    None => ((*line).append_switch.expect("append_switch"))(line, name.raw()),
                }
            };
            set("autoplay-policy", Some("no-user-gesture-required"));
            set("disable-background-timer-throttling", None);
            set("disable-renderer-backgrounding", None);
            set("disable-backgrounding-occluded-windows", None);
            set("password-store", Some("basic"));
            set("disable-component-update", None);
            set("disable-extensions", None);
        }
        release(line);
    }
}

/// A task for CEF's UI thread.
#[repr(C)]
struct Task {
    cef: cef_task_t,
    refs: AtomicUsize,
    run: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

unsafe extern "C" fn task_add_ref(b: *mut cef_base_ref_counted_t) {
    // SAFETY: the base is a Task's first field.
    unsafe { (*b.cast::<Task>()).refs.fetch_add(1, Ordering::Relaxed) };
}
unsafe extern "C" fn task_release(b: *mut cef_base_ref_counted_t) -> c_int {
    let t = b.cast::<Task>();
    // SAFETY: the base is a Task's first field; made by Box, freed once.
    unsafe {
        if (*t).refs.fetch_sub(1, Ordering::AcqRel) == 1 {
            drop(Box::from_raw(t));
            return 1;
        }
    }
    0
}
unsafe extern "C" fn task_has_one(b: *mut cef_base_ref_counted_t) -> c_int {
    // SAFETY: the base is a Task's first field.
    c_int::from(unsafe { (*b.cast::<Task>()).refs.load(Ordering::Acquire) } == 1)
}
unsafe extern "C" fn task_has_any(b: *mut cef_base_ref_counted_t) -> c_int {
    // SAFETY: as above.
    c_int::from(unsafe { (*b.cast::<Task>()).refs.load(Ordering::Acquire) } >= 1)
}
unsafe extern "C" fn task_execute(t: *mut cef_task_t) {
    // SAFETY: the task is a Task's first field.
    let run = unsafe { (*t.cast::<Task>()).run.lock().unwrap().take() };
    if let Some(run) = run {
        run();
    }
}

/// Run `f` on CEF's UI thread.
fn post(f: impl FnOnce() + Send + 'static) {
    let task = Box::into_raw(Box::new(Task {
        cef: cef_task_t {
            base: cef_base_ref_counted_t {
                size: size_of::<cef_task_t>(),
                add_ref: Some(task_add_ref),
                release: Some(task_release),
                has_one_ref: Some(task_has_one),
                has_at_least_one_ref: Some(task_has_any),
            },
            execute: Some(task_execute),
        },
        refs: AtomicUsize::new(1),
        run: Mutex::new(Some(Box::new(f))),
    }));
    // SAFETY: CEF's function; the task goes over with its one reference.
    unsafe { (host().lib.post_task)(TID_UI, task.cast()) };
}

/// A page's client and handlers, in one allocation with one count of
/// references (each handler's base counts the whole).
#[repr(C)]
struct PageClient {
    client: cef_client_t,
    render: cef_render_handler_t,
    life: cef_life_span_handler_t,
    load: cef_load_handler_t,
    request: cef_request_handler_t,
    refs: AtomicUsize,
    page: Arc<HostPage>,
}

/// The page client a field at offset `at` (a handler's base) is in.
///
/// # Safety
/// `p` points at the field at `at` of a live `PageClient`.
unsafe fn client_at<'a, T>(p: *mut T, at: usize) -> &'a PageClient {
    // SAFETY: as the caller says.
    unsafe { &*p.byte_sub(at).cast::<PageClient>() }
}

unsafe extern "C" fn add_ref_at<const AT: usize>(b: *mut cef_base_ref_counted_t) {
    // SAFETY: CEF calls a handler's base of ours.
    unsafe { client_at(b, AT) }.refs.fetch_add(1, Ordering::Relaxed);
}
unsafe extern "C" fn release_at<const AT: usize>(b: *mut cef_base_ref_counted_t) -> c_int {
    // SAFETY: CEF calls a handler's base of ours; made by Box, freed once.
    unsafe {
        let me = b.byte_sub(AT).cast::<PageClient>();
        if (*me).refs.fetch_sub(1, Ordering::AcqRel) == 1 {
            drop(Box::from_raw(me));
            return 1;
        }
    }
    0
}
unsafe extern "C" fn has_one_at<const AT: usize>(b: *mut cef_base_ref_counted_t) -> c_int {
    // SAFETY: CEF calls a handler's base of ours.
    c_int::from(unsafe { client_at(b, AT) }.refs.load(Ordering::Acquire) == 1)
}
unsafe extern "C" fn has_any_at<const AT: usize>(b: *mut cef_base_ref_counted_t) -> c_int {
    // SAFETY: as above.
    c_int::from(unsafe { client_at(b, AT) }.refs.load(Ordering::Acquire) >= 1)
}

fn base_at<const AT: usize>(size: usize) -> cef_base_ref_counted_t {
    cef_base_ref_counted_t { size, add_ref: Some(add_ref_at::<AT>), release: Some(release_at::<AT>), has_one_ref: Some(has_one_at::<AT>), has_at_least_one_ref: Some(has_any_at::<AT>) }
}

const CLIENT: usize = offset_of!(PageClient, client);
const RENDER: usize = offset_of!(PageClient, render);
const LIFE: usize = offset_of!(PageClient, life);
const LOAD: usize = offset_of!(PageClient, load);
const REQUEST: usize = offset_of!(PageClient, request);

impl PageClient {
    /// A new one, with its one reference (the caller's).
    fn make(page: Arc<HostPage>) -> *mut PageClient {
        // SAFETY: all zeros: every method not handled.
        let (mut client, mut render, mut life, mut load, mut request): (cef_client_t, cef_render_handler_t, cef_life_span_handler_t, cef_load_handler_t, cef_request_handler_t) = unsafe { std::mem::zeroed() };
        client.base = base_at::<CLIENT>(size_of::<cef_client_t>());
        client.get_render_handler = Some(get_render_handler);
        client.get_life_span_handler = Some(get_life_span_handler);
        client.get_load_handler = Some(get_load_handler);
        client.get_request_handler = Some(get_request_handler);
        render.base = base_at::<RENDER>(size_of::<cef_render_handler_t>());
        render.get_view_rect = Some(get_view_rect);
        render.get_screen_info = Some(get_screen_info);
        render.on_paint = Some(on_paint);
        render.on_accelerated_paint = Some(on_accelerated_paint);
        life.base = base_at::<LIFE>(size_of::<cef_life_span_handler_t>());
        life.on_before_popup = Some(on_before_popup);
        life.on_after_created = Some(on_after_created);
        life.do_close = Some(do_close);
        life.on_before_close = Some(on_before_close);
        load.base = base_at::<LOAD>(size_of::<cef_load_handler_t>());
        load.on_load_start = Some(on_load_start);
        load.on_load_end = Some(on_load_end);
        load.on_load_error = Some(on_load_error);
        request.base = base_at::<REQUEST>(size_of::<cef_request_handler_t>());
        request.on_render_process_terminated = Some(on_render_process_terminated);
        Box::into_raw(Box::new(PageClient { client, render, life, load, request, refs: AtomicUsize::new(1), page }))
    }
}

/// The page a handler of ours is for.
///
/// # Safety
/// `p` is the handler at `at` in a live `PageClient`.
unsafe fn page_of<'a, T>(p: *mut T, at: usize) -> &'a HostPage {
    // SAFETY: as the caller says.
    &unsafe { client_at(p, at) }.page
}

/// A handler of a client to hand over: with a reference of its own.
///
/// # Safety
/// `c` is a live `PageClient`'s client.
unsafe fn handler<T>(c: *mut cef_client_t, at: usize) -> *mut T {
    // SAFETY: as the caller says; the count is the whole client's.
    unsafe {
        add_ref(c);
        c.byte_add(at - CLIENT).cast()
    }
}

unsafe extern "C" fn get_render_handler(c: *mut cef_client_t) -> *mut cef_render_handler_t {
    // SAFETY: CEF calls a client of ours.
    unsafe { handler(c, RENDER) }
}
unsafe extern "C" fn get_life_span_handler(c: *mut cef_client_t) -> *mut cef_life_span_handler_t {
    // SAFETY: as above.
    unsafe { handler(c, LIFE) }
}
unsafe extern "C" fn get_load_handler(c: *mut cef_client_t) -> *mut cef_load_handler_t {
    // SAFETY: as above.
    unsafe { handler(c, LOAD) }
}
unsafe extern "C" fn get_request_handler(c: *mut cef_client_t) -> *mut cef_request_handler_t {
    // SAFETY: as above.
    unsafe { handler(c, REQUEST) }
}

/// The page's size, in its own pixels (one a pixel: [`get_screen_info`]).
unsafe extern "C" fn get_view_rect(h: *mut cef_render_handler_t, browser: *mut cef_browser_t, rect: *mut cef_rect_t) {
    // SAFETY: CEF calls a handler of ours, with a browser and a rect to fill.
    unsafe {
        let (w, hh) = page_of(h, RENDER).size;
        *rect = cef_rect_t { x: 0, y: 0, width: w as c_int, height: hh as c_int };
        release(browser);
    }
}

unsafe extern "C" fn get_screen_info(h: *mut cef_render_handler_t, browser: *mut cef_browser_t, info: *mut cef_screen_info_t) -> c_int {
    // SAFETY: as above.
    unsafe {
        let (w, hh) = page_of(h, RENDER).size;
        let all = cef_rect_t { x: 0, y: 0, width: w as c_int, height: hh as c_int };
        (*info).device_scale_factor = 1.0;
        (*info).depth = 24;
        (*info).depth_per_component = 8;
        (*info).rect = all;
        (*info).available_rect = all;
        release(browser);
    }
    1
}

/// A frame drawn in memory: into the page's memory file, for the app.
#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn on_paint(h: *mut cef_render_handler_t, browser: *mut cef_browser_t, kind: cef_paint_element_type_t, _rects: usize, _rect: *const cef_rect_t, buffer: *const c_void, width: c_int, height: c_int) {
    // SAFETY: CEF calls a handler of ours, with a browser.
    let page = unsafe {
        release(browser);
        page_of(h, RENDER)
    };
    if kind != PET_VIEW || buffer.is_null() || width <= 0 || height <= 0 {
        return;
    }
    let len = width as usize * height as usize * 4;
    // SAFETY: CEF's pixels, `width`×`height`×4 bytes, valid for the call.
    let pixels = unsafe { std::slice::from_raw_parts(buffer.cast::<u8>(), len) };
    let mut memory = page.memory.lock().unwrap();
    if memory.is_none() {
        // SAFETY: a name; a new file of ours.
        let fd = unsafe { libc::memfd_create(c"hasteclip-page".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return;
        }
        // SAFETY: just made, ours.
        *memory = Some(unsafe { OwnedFd::from_raw_fd(fd) });
    }
    let Some(file) = memory.as_ref() else { return };
    let mut at = 0;
    while at < len {
        // SAFETY: our file, a slice of ours.
        let n = unsafe { libc::pwrite(file.as_raw_fd(), pixels[at..].as_ptr().cast(), len - at, at as libc::off_t) };
        if n <= 0 {
            return;
        }
        at += n as usize;
    }
    send_frame(page, &ToApp::Memory { page: page.id, width: width as u32, height: height as u32 }, file.as_fd());
}

/// A frame drawn on the graphics card: its DMA-BUF to the app, which
/// copies it before the engine draws in it again.
unsafe extern "C" fn on_accelerated_paint(h: *mut cef_render_handler_t, browser: *mut cef_browser_t, kind: cef_paint_element_type_t, _rects: usize, _rect: *const cef_rect_t, info: *const cef_accelerated_paint_info_t) {
    // SAFETY: CEF calls a handler of ours, with a browser.
    let page = unsafe {
        release(browser);
        page_of(h, RENDER)
    };
    if kind != PET_VIEW || info.is_null() {
        return;
    }
    // SAFETY: CEF's frame, valid for the call.
    let info = unsafe { &*info };
    let fourcc = match info.format {
        CEF_COLOR_TYPE_BGRA_8888 => DRM_ARGB8888,
        CEF_COLOR_TYPE_RGBA_8888 => DRM_ABGR8888,
        _ => return,
    };
    if info.plane_count < 1 || info.planes[0].fd < 0 {
        return;
    }
    let plane = info.planes[0];
    let (mut w, mut hh) = page.size;
    let mut visible = [0, 0, w, hh];
    if info.extra.size > 0 && info.extra.coded_size.width > 0 && info.extra.coded_size.height > 0 {
        (w, hh) = (info.extra.coded_size.width as u32, info.extra.coded_size.height as u32);
        let v = info.extra.visible_rect;
        visible = if v.width > 0 && v.height > 0 { [v.x.max(0) as u32, v.y.max(0) as u32, v.width as u32, v.height as u32] } else { [0, 0, w, hh] };
    }
    // SAFETY: CEF's fd, open for the call (the app gets its own copy).
    let fd = unsafe { BorrowedFd::borrow_raw(plane.fd) };
    let msg = ToApp::Card { page: page.id, width: w, height: hh, visible, fourcc, modifier: info.modifier, offset: plane.offset, stride: plane.stride };
    send_frame(page, &msg, fd);
}

/// Pages can't open windows of their own.
#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn on_before_popup(
    _: *mut cef_life_span_handler_t,
    browser: *mut cef_browser_t,
    frame: *mut cef_frame_t,
    _: c_int,
    _: *const cef_string_t,
    _: *const cef_string_t,
    _: c_uint,
    _: c_int,
    _: *const c_void,
    _: *mut c_void,
    _: *mut c_void,
    _: *mut c_void,
    _: *mut c_void,
    _: *mut c_int,
) -> c_int {
    // SAFETY: CEF's browser and frame, handed over with a reference each.
    unsafe {
        release(frame);
        release(browser);
    }
    1
}

/// The browser's made: kept (its reference is ours) till it closes.
unsafe extern "C" fn on_after_created(h: *mut cef_life_span_handler_t, browser: *mut cef_browser_t) {
    // SAFETY: CEF calls a handler of ours.
    let page = unsafe { page_of(h, LIFE) };
    page.browser.store(browser, Ordering::Relaxed);
    // Closed before it was made: closed now.
    if page.closing.load(Ordering::Relaxed) {
        page.close();
    }
}

unsafe extern "C" fn do_close(_: *mut cef_life_span_handler_t, browser: *mut cef_browser_t) -> c_int {
    // SAFETY: CEF's browser, handed over with a reference.
    unsafe { release(browser) };
    0
}

/// The browser's closed: the page is gone (and once the last is, so is the
/// host, if the app has gone).
unsafe extern "C" fn on_before_close(h: *mut cef_life_span_handler_t, browser: *mut cef_browser_t) {
    // SAFETY: CEF calls a handler of ours, with a browser; the kept one's
    // reference is ours.
    let page = unsafe {
        release(browser);
        page_of(h, LIFE)
    };
    // SAFETY: the reference kept since it was made.
    unsafe { release(page.browser.swap(null_mut(), Ordering::Relaxed)) };
    let host = host();
    let empty = {
        let mut pages = host.pages.lock().unwrap();
        pages.retain(|p| p.id != page.id);
        pages.is_empty()
    };
    if empty && host.quitting.load(Ordering::Acquire) {
        // SAFETY: on CEF's UI thread.
        unsafe { (host.lib.quit_message_loop)() };
    }
}

/// Whether `frame` is its page's main frame.
///
/// # Safety
/// `frame` is null or a live CEF frame.
unsafe fn is_main(frame: *mut cef_frame_t) -> bool {
    // SAFETY: as the caller says.
    !frame.is_null() && unsafe { ((*frame).is_main.expect("is_main"))(frame) } != 0
}

/// Add the page's style to `frame`, if it has one.
///
/// # Safety
/// `frame` is a live CEF frame.
unsafe fn add_style(page: &HostPage, frame: *mut cef_frame_t) {
    if let Some(script) = &page.style {
        let (code, url) = (Utf16::new(script), Utf16::new(""));
        // SAFETY: as the caller says; strings alive for the call.
        unsafe { ((*frame).execute_java_script.expect("execute_java_script"))(frame, code.raw(), url.raw(), 0) };
    }
}

/// A page starts loading: its style goes in as soon as its document's there.
unsafe extern "C" fn on_load_start(h: *mut cef_load_handler_t, browser: *mut cef_browser_t, frame: *mut cef_frame_t, _: c_uint) {
    // SAFETY: CEF calls a handler of ours, with a browser and frame.
    unsafe {
        if is_main(frame) {
            add_style(page_of(h, LOAD), frame);
        }
        release(frame);
        release(browser);
    }
}

/// A page has loaded: its style in, if it isn't yet (a script run as it
/// started can be lost), and the app told.
unsafe extern "C" fn on_load_end(h: *mut cef_load_handler_t, browser: *mut cef_browser_t, frame: *mut cef_frame_t, status: c_int) {
    // SAFETY: as above.
    unsafe {
        if is_main(frame) {
            let page = page_of(h, LOAD);
            add_style(page, frame);
            page.crashes.store(0, Ordering::Relaxed);
            let error = (status >= 400).then(|| format!("the site answered {status}"));
            let _ = host().wire.send(&ToApp::Loaded { page: page.id, error }.encode(), None);
        }
        release(frame);
        release(browser);
    }
}

unsafe extern "C" fn on_load_error(h: *mut cef_load_handler_t, browser: *mut cef_browser_t, frame: *mut cef_frame_t, code: c_int, text: *const cef_string_t, _url: *const cef_string_t) {
    // SAFETY: as above, and CEF's strings.
    unsafe {
        // ERR_ABORTED: another load took its place.
        if is_main(frame) && code != -3 {
            let page = page_of(h, LOAD);
            let text = cef_string_t::read(text);
            let error = if text.is_empty() { format!("error {code}") } else { text };
            let _ = host().wire.send(&ToApp::Loaded { page: page.id, error: Some(error) }.encode(), None);
        }
        release(frame);
        release(browser);
    }
}

/// A page's renderer stopped (crashed, killed): loaded again, a few times.
unsafe extern "C" fn on_render_process_terminated(h: *mut cef_request_handler_t, browser: *mut cef_browser_t, status: cef_termination_status_t, code: c_int, why: *const cef_string_t) {
    // SAFETY: CEF calls a handler of ours, with a browser and a string.
    unsafe {
        let page = page_of(h, REQUEST);
        let n = page.crashes.fetch_add(1, Ordering::Relaxed) + 1;
        eprintln!("web page {}: its renderer stopped ({status}, {code}: {})", page.id, cef_string_t::read(why));
        if n <= 3 && !page.closing.load(Ordering::Relaxed) {
            ((*browser).reload.expect("reload"))(browser);
        }
        release(browser);
    }
}
