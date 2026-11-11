//! CEF's C API, as much of it as web pages take (`super::web`): its
//! structures, laid out as in CEF 156's `include/capi` headers at API
//! version [`API_VERSION`], and its functions, looked up in libcef.so when
//! it's loaded ([`Lib::load`]). The app never links CEF: it's fetched the
//! first time a web page is added (`super::web::engine`).
//!
//! Structures CEF hands us (a browser, its host, a frame, a command line)
//! are only read through pointers, so they end at the last method used; the
//! methods before it that aren't used keep their places as [`Unused`].
//! Structures we make (an app, a client and its handlers, a task) are whole,
//! the methods we don't give left null, which CEF takes as "not handled".
//! Every field here was compared with bindgen's reading of those headers
//! (each one's offset, and the size of each structure we make): moving to
//! another API version means comparing them again.
//!
//! Reference counting, as CEF's own C++ wrapper does it: a pointer handed
//! across, either way (an argument, a return value), carries one reference,
//! which its receiver releases when it's done with it.

#![allow(non_camel_case_types, dead_code)]

use std::ffi::{CStr, c_char, c_int, c_uint, c_ulong, c_void};

use anyhow::{Result, anyhow};

/// The C API version asked for: the newest in CEF 156, which libcef.so of
/// later versions keeps taking as long as it lists it (`cef_api_versions.h`).
pub const API_VERSION: c_int = 15600;

/// A method in a structure that isn't called: only its place matters.
pub type Unused = usize;

#[repr(C)]
pub struct cef_string_t {
    pub str_: *mut u16,
    pub length: usize,
    pub dtor: Option<unsafe extern "C" fn(*mut u16)>,
}

impl cef_string_t {
    pub const EMPTY: Self = Self { str_: std::ptr::null_mut(), length: 0, dtor: None };

    /// The words, as UTF-8 (none for a null string).
    ///
    /// # Safety
    /// `s` is null, or a valid CEF string.
    pub unsafe fn read(s: *const cef_string_t) -> String {
        // SAFETY: as the caller says.
        match unsafe { s.as_ref() } {
            Some(s) if !s.str_.is_null() => String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(s.str_, s.length) }),
            _ => String::new(),
        }
    }
}

/// A string of ours for CEF to read (CEF copies what it keeps): UTF-16,
/// alive as long as this is.
pub struct Utf16 {
    words: Vec<u16>,
    raw: cef_string_t,
}

impl Utf16 {
    pub fn new(s: &str) -> Box<Self> {
        let words: Vec<u16> = s.encode_utf16().collect();
        let mut b = Box::new(Self { words, raw: cef_string_t::EMPTY });
        b.raw = cef_string_t { str_: b.words.as_mut_ptr(), length: b.words.len(), dtor: None };
        b
    }

    pub fn raw(&self) -> &cef_string_t {
        &self.raw
    }

    /// A copy of the string's header to put in a structure (`self` must
    /// outlive it).
    pub fn header(&self) -> cef_string_t {
        cef_string_t { str_: self.raw.str_, length: self.raw.length, dtor: None }
    }
}

#[repr(C)]
pub struct cef_base_ref_counted_t {
    pub size: usize,
    pub add_ref: Option<unsafe extern "C" fn(*mut cef_base_ref_counted_t)>,
    pub release: Option<unsafe extern "C" fn(*mut cef_base_ref_counted_t) -> c_int>,
    pub has_one_ref: Option<unsafe extern "C" fn(*mut cef_base_ref_counted_t) -> c_int>,
    pub has_at_least_one_ref: Option<unsafe extern "C" fn(*mut cef_base_ref_counted_t) -> c_int>,
}

/// Add a reference to a CEF object (before handing it across).
///
/// # Safety
/// `p` points at a live CEF object (its first field the base).
pub unsafe fn add_ref<T>(p: *mut T) {
    let base = p.cast::<cef_base_ref_counted_t>();
    // SAFETY: as the caller says.
    if let Some(f) = unsafe { (*base).add_ref } {
        unsafe { f(base) };
    }
}

/// Let go of a reference to a CEF object (one handed to us, done with).
///
/// # Safety
/// `p` is null, or points at a live CEF object we hold a reference to.
pub unsafe fn release<T>(p: *mut T) {
    if p.is_null() {
        return;
    }
    let base = p.cast::<cef_base_ref_counted_t>();
    // SAFETY: as the caller says.
    if let Some(f) = unsafe { (*base).release } {
        unsafe { f(base) };
    }
}

#[repr(C)]
pub struct cef_main_args_t {
    pub argc: c_int,
    pub argv: *mut *mut c_char,
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct cef_rect_t {
    pub x: c_int,
    pub y: c_int,
    pub width: c_int,
    pub height: c_int,
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct cef_size_t {
    pub width: c_int,
    pub height: c_int,
}

pub type cef_color_t = u32;
pub type cef_state_t = c_uint;
pub const STATE_DEFAULT: cef_state_t = 0;
pub const STATE_ENABLED: cef_state_t = 1;
pub const STATE_DISABLED: cef_state_t = 2;

pub type cef_log_severity_t = c_uint;
pub const LOGSEVERITY_DEFAULT: cef_log_severity_t = 0;
pub const LOGSEVERITY_WARNING: cef_log_severity_t = 3;
pub const LOGSEVERITY_ERROR: cef_log_severity_t = 4;
pub const LOGSEVERITY_FATAL: cef_log_severity_t = 5;
pub const LOGSEVERITY_DISABLE: cef_log_severity_t = 99;

pub type cef_thread_id_t = c_uint;
pub const TID_UI: cef_thread_id_t = 0;

pub type cef_paint_element_type_t = c_uint;
pub const PET_VIEW: cef_paint_element_type_t = 0;

pub type cef_color_type_t = c_uint;
pub const CEF_COLOR_TYPE_RGBA_8888: cef_color_type_t = 0;
pub const CEF_COLOR_TYPE_BGRA_8888: cef_color_type_t = 1;

pub type cef_runtime_style_t = c_uint;
pub const CEF_RUNTIME_STYLE_ALLOY: cef_runtime_style_t = 2;

pub type cef_termination_status_t = c_uint;

#[repr(C)]
pub struct cef_settings_t {
    pub size: usize,
    pub no_sandbox: c_int,
    pub browser_subprocess_path: cef_string_t,
    pub framework_dir_path: cef_string_t,
    pub main_bundle_path: cef_string_t,
    pub multi_threaded_message_loop: c_int,
    pub external_message_pump: c_int,
    pub windowless_rendering_enabled: c_int,
    pub command_line_args_disabled: c_int,
    pub cache_path: cef_string_t,
    pub root_cache_path: cef_string_t,
    pub persist_session_cookies: c_int,
    pub user_agent: cef_string_t,
    pub user_agent_product: cef_string_t,
    pub locale: cef_string_t,
    pub log_file: cef_string_t,
    pub log_severity: cef_log_severity_t,
    pub log_items: c_uint,
    pub javascript_flags: cef_string_t,
    pub resources_dir_path: cef_string_t,
    pub locales_dir_path: cef_string_t,
    pub remote_debugging_port: c_int,
    pub uncaught_exception_stack_size: c_int,
    pub background_color: cef_color_t,
    pub accept_language_list: cef_string_t,
    pub cookieable_schemes_list: cef_string_t,
    pub cookieable_schemes_exclude_defaults: c_int,
    pub chrome_policy_id: cef_string_t,
    pub chrome_app_icon_id: c_int,
    pub disable_signal_handlers: c_int,
    pub use_views_default_popup: c_int,
    pub keychain_service_name: cef_string_t,
    pub keychain_account_name: cef_string_t,
}

#[repr(C)]
pub struct cef_window_info_t {
    pub size: usize,
    pub window_name: cef_string_t,
    pub bounds: cef_rect_t,
    pub parent_window: c_ulong,
    pub windowless_rendering_enabled: c_int,
    pub shared_texture_enabled: c_int,
    pub external_begin_frame_enabled: c_int,
    pub window: c_ulong,
    pub runtime_style: cef_runtime_style_t,
}

#[repr(C)]
pub struct cef_browser_settings_t {
    pub size: usize,
    pub windowless_frame_rate: c_int,
    pub standard_font_family: cef_string_t,
    pub fixed_font_family: cef_string_t,
    pub serif_font_family: cef_string_t,
    pub sans_serif_font_family: cef_string_t,
    pub cursive_font_family: cef_string_t,
    pub fantasy_font_family: cef_string_t,
    pub default_font_size: c_int,
    pub default_fixed_font_size: c_int,
    pub minimum_font_size: c_int,
    pub minimum_logical_font_size: c_int,
    pub default_encoding: cef_string_t,
    pub remote_fonts: cef_state_t,
    pub javascript: cef_state_t,
    pub javascript_close_windows: cef_state_t,
    pub javascript_access_clipboard: cef_state_t,
    pub javascript_dom_paste: cef_state_t,
    pub image_loading: cef_state_t,
    pub image_shrink_standalone_to_fit: cef_state_t,
    pub text_area_resize: cef_state_t,
    pub tab_to_links: cef_state_t,
    pub local_storage: cef_state_t,
    pub databases_deprecated: cef_state_t,
    pub webgl: cef_state_t,
    pub background_color: cef_color_t,
    pub chrome_status_bubble: cef_state_t,
    pub chrome_zoom_bubble: cef_state_t,
}

#[repr(C)]
pub struct cef_screen_info_t {
    pub size: usize,
    pub device_scale_factor: f32,
    pub depth: c_int,
    pub depth_per_component: c_int,
    pub is_monochrome: c_int,
    pub rect: cef_rect_t,
    pub available_rect: cef_rect_t,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct cef_accelerated_paint_native_pixmap_plane_t {
    pub stride: u32,
    pub offset: u64,
    pub size: u64,
    pub fd: c_int,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct cef_accelerated_paint_info_common_t {
    pub size: usize,
    pub timestamp: u64,
    pub coded_size: cef_size_t,
    pub visible_rect: cef_rect_t,
    pub content_rect: cef_rect_t,
    pub source_size: cef_size_t,
    pub capture_update_rect: cef_rect_t,
    pub region_capture_rect: cef_rect_t,
    pub capture_counter: u64,
    pub has_capture_update_rect: u8,
    pub has_region_capture_rect: u8,
    pub has_source_size: u8,
    pub has_capture_counter: u8,
}

/// A frame on the graphics card (Linux: DMA-BUF planes), valid during the
/// call it's handed to only.
#[repr(C)]
pub struct cef_accelerated_paint_info_t {
    pub size: usize,
    pub planes: [cef_accelerated_paint_native_pixmap_plane_t; 4],
    pub plane_count: c_int,
    pub modifier: u64,
    pub format: cef_color_type_t,
    pub extra: cef_accelerated_paint_info_common_t,
}

// ---------------------------------------------------------------------------
// Structures we make
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct cef_app_t {
    pub base: cef_base_ref_counted_t,
    pub on_before_command_line_processing: Option<unsafe extern "C" fn(*mut cef_app_t, *const cef_string_t, *mut cef_command_line_t)>,
    pub on_register_custom_schemes: Unused,
    pub get_resource_bundle_handler: Unused,
    pub get_browser_process_handler: Unused,
    pub get_render_process_handler: Unused,
}

#[repr(C)]
pub struct cef_client_t {
    pub base: cef_base_ref_counted_t,
    pub get_audio_handler: Unused,
    pub get_command_handler: Unused,
    pub get_context_menu_handler: Unused,
    pub get_dialog_handler: Unused,
    pub get_display_handler: Option<unsafe extern "C" fn(*mut cef_client_t) -> *mut cef_display_handler_t>,
    pub get_download_handler: Unused,
    pub get_drag_handler: Unused,
    pub get_find_handler: Unused,
    pub get_focus_handler: Unused,
    pub get_frame_handler: Unused,
    pub get_permission_handler: Unused,
    pub get_jsdialog_handler: Unused,
    pub get_keyboard_handler: Unused,
    pub get_life_span_handler: Option<unsafe extern "C" fn(*mut cef_client_t) -> *mut cef_life_span_handler_t>,
    pub get_load_handler: Option<unsafe extern "C" fn(*mut cef_client_t) -> *mut cef_load_handler_t>,
    pub get_print_handler: Unused,
    pub get_render_handler: Option<unsafe extern "C" fn(*mut cef_client_t) -> *mut cef_render_handler_t>,
    pub get_request_handler: Option<unsafe extern "C" fn(*mut cef_client_t) -> *mut cef_request_handler_t>,
    pub on_process_message_received: Unused,
}

#[repr(C)]
pub struct cef_render_handler_t {
    pub base: cef_base_ref_counted_t,
    pub get_accessibility_handler: Unused,
    pub get_root_screen_rect: Unused,
    pub get_view_rect: Option<unsafe extern "C" fn(*mut cef_render_handler_t, *mut cef_browser_t, *mut cef_rect_t)>,
    pub get_screen_point: Unused,
    pub get_screen_info: Option<unsafe extern "C" fn(*mut cef_render_handler_t, *mut cef_browser_t, *mut cef_screen_info_t) -> c_int>,
    pub on_popup_show: Unused,
    pub on_popup_size: Unused,
    #[allow(clippy::type_complexity)]
    pub on_paint: Option<unsafe extern "C" fn(*mut cef_render_handler_t, *mut cef_browser_t, cef_paint_element_type_t, usize, *const cef_rect_t, *const c_void, c_int, c_int)>,
    pub on_accelerated_paint: Option<unsafe extern "C" fn(*mut cef_render_handler_t, *mut cef_browser_t, cef_paint_element_type_t, usize, *const cef_rect_t, *const cef_accelerated_paint_info_t)>,
    pub get_touch_handle_size: Unused,
    pub on_touch_handle_state_changed: Unused,
    pub start_dragging: Unused,
    pub update_drag_cursor: Unused,
    pub on_scroll_offset_changed: Unused,
    pub on_ime_composition_range_changed: Unused,
    pub on_text_selection_changed: Unused,
    pub on_virtual_keyboard_requested: Unused,
}

/// `on_before_popup`'s arguments after the frame, which we don't read.
pub type PopupRest = (c_int, *const cef_string_t, *const cef_string_t, c_uint, c_int, *const c_void, *mut c_void, *mut c_void, *mut c_void, *mut c_void, *mut c_int);

#[repr(C)]
pub struct cef_life_span_handler_t {
    pub base: cef_base_ref_counted_t,
    #[allow(clippy::type_complexity)]
    pub on_before_popup: Option<
        unsafe extern "C" fn(
            *mut cef_life_span_handler_t,
            *mut cef_browser_t,
            *mut cef_frame_t,
            c_int,
            *const cef_string_t,
            *const cef_string_t,
            c_uint,
            c_int,
            *const c_void,
            *mut c_void,
            *mut c_void,
            *mut c_void,
            *mut c_void,
            *mut c_int,
        ) -> c_int,
    >,
    pub on_before_popup_aborted: Unused,
    pub on_before_dev_tools_popup: Unused,
    pub on_after_created: Option<unsafe extern "C" fn(*mut cef_life_span_handler_t, *mut cef_browser_t)>,
    pub do_close: Option<unsafe extern "C" fn(*mut cef_life_span_handler_t, *mut cef_browser_t) -> c_int>,
    pub on_before_close: Option<unsafe extern "C" fn(*mut cef_life_span_handler_t, *mut cef_browser_t)>,
}

#[repr(C)]
pub struct cef_load_handler_t {
    pub base: cef_base_ref_counted_t,
    pub on_loading_state_change: Unused,
    pub on_load_start: Option<unsafe extern "C" fn(*mut cef_load_handler_t, *mut cef_browser_t, *mut cef_frame_t, c_uint)>,
    pub on_load_end: Option<unsafe extern "C" fn(*mut cef_load_handler_t, *mut cef_browser_t, *mut cef_frame_t, c_int)>,
    pub on_load_error: Option<unsafe extern "C" fn(*mut cef_load_handler_t, *mut cef_browser_t, *mut cef_frame_t, c_int, *const cef_string_t, *const cef_string_t)>,
}

#[repr(C)]
pub struct cef_request_handler_t {
    pub base: cef_base_ref_counted_t,
    pub on_before_browse: Unused,
    pub on_open_urlfrom_tab: Unused,
    pub get_resource_request_handler: Unused,
    pub get_auth_credentials: Unused,
    pub on_certificate_error: Unused,
    pub on_select_client_certificate: Unused,
    pub on_render_view_ready: Unused,
    pub on_render_process_unresponsive: Unused,
    pub on_render_process_responsive: Unused,
    pub on_render_process_terminated: Option<unsafe extern "C" fn(*mut cef_request_handler_t, *mut cef_browser_t, cef_termination_status_t, c_int, *const cef_string_t)>,
    pub on_document_available_in_main_frame: Unused,
}

#[repr(C)]
pub struct cef_display_handler_t {
    pub base: cef_base_ref_counted_t,
    pub on_address_change: Unused,
    pub on_title_change: Unused,
    pub on_favicon_urlchange: Unused,
    pub on_fullscreen_mode_change: Unused,
    pub on_tooltip: Unused,
    pub on_status_message: Unused,
    pub on_console_message: Option<unsafe extern "C" fn(*mut cef_display_handler_t, *mut cef_browser_t, cef_log_severity_t, *const cef_string_t, *const cef_string_t, c_int) -> c_int>,
    pub on_auto_resize: Unused,
    pub on_loading_progress_change: Unused,
    pub on_cursor_change: Unused,
    pub on_media_access_change: Unused,
    pub on_contents_bounds_change: Unused,
    pub get_root_window_screen_rect: Unused,
}

#[repr(C)]
pub struct cef_task_t {
    pub base: cef_base_ref_counted_t,
    pub execute: Option<unsafe extern "C" fn(*mut cef_task_t)>,
}

// ---------------------------------------------------------------------------
// Structures CEF hands us: read through pointers only, up to the last
// method used.
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct cef_browser_t {
    pub base: cef_base_ref_counted_t,
    pub is_valid: Unused,
    pub get_host: Option<unsafe extern "C" fn(*mut cef_browser_t) -> *mut cef_browser_host_t>,
    pub can_go_back: Unused,
    pub go_back: Unused,
    pub can_go_forward: Unused,
    pub go_forward: Unused,
    pub is_loading: Unused,
    pub reload: Option<unsafe extern "C" fn(*mut cef_browser_t)>,
    pub reload_ignore_cache: Unused,
    pub stop_load: Unused,
    pub get_identifier: Option<unsafe extern "C" fn(*mut cef_browser_t) -> c_int>,
    pub is_same: Unused,
    pub is_popup: Unused,
    pub has_document: Unused,
    pub get_main_frame: Option<unsafe extern "C" fn(*mut cef_browser_t) -> *mut cef_frame_t>,
}

#[repr(C)]
pub struct cef_browser_host_t {
    pub base: cef_base_ref_counted_t,
    pub get_browser: Unused,
    pub close_browser: Option<unsafe extern "C" fn(*mut cef_browser_host_t, c_int)>,
    /// try_close_browser … add_word_to_dictionary.
    pub _a: [Unused; 30],
    pub is_window_rendering_disabled: Unused,
    pub was_resized: Option<unsafe extern "C" fn(*mut cef_browser_host_t)>,
    pub was_hidden: Option<unsafe extern "C" fn(*mut cef_browser_host_t, c_int)>,
    pub notify_screen_info_changed: Unused,
    pub invalidate: Option<unsafe extern "C" fn(*mut cef_browser_host_t, cef_paint_element_type_t)>,
    /// send_external_begin_frame … notify_move_or_resize_started.
    pub _b: [Unused; 8],
    pub get_windowless_frame_rate: Unused,
    pub set_windowless_frame_rate: Option<unsafe extern "C" fn(*mut cef_browser_host_t, c_int)>,
    /// ime_set_composition … set_auto_resize_enabled.
    pub _c: [Unused; 13],
    pub set_audio_muted: Option<unsafe extern "C" fn(*mut cef_browser_host_t, c_int)>,
}

#[repr(C)]
pub struct cef_frame_t {
    pub base: cef_base_ref_counted_t,
    /// is_valid … load_url.
    pub _a: [Unused; 14],
    pub execute_java_script: Option<unsafe extern "C" fn(*mut cef_frame_t, *const cef_string_t, *const cef_string_t, c_int)>,
    pub is_main: Option<unsafe extern "C" fn(*mut cef_frame_t) -> c_int>,
}

#[repr(C)]
pub struct cef_command_line_t {
    pub base: cef_base_ref_counted_t,
    /// is_valid … get_switches.
    pub _a: [Unused; 14],
    pub append_switch: Option<unsafe extern "C" fn(*mut cef_command_line_t, *const cef_string_t)>,
    pub append_switch_with_value: Option<unsafe extern "C" fn(*mut cef_command_line_t, *const cef_string_t, *const cef_string_t)>,
}

// Opaque: only passed along as null.
pub enum cef_dictionary_value_t {}
pub enum cef_request_context_t {}

// ---------------------------------------------------------------------------
// The library
// ---------------------------------------------------------------------------

// The functions of libcef.so we call (`cef_api_hash.h`, `cef_app_capi.h`,
// `cef_browser_capi.h`, `cef_task_capi.h`).
type ApiHash = unsafe extern "C" fn(c_int, c_int) -> *const c_char;
type ExecuteProcess = unsafe extern "C" fn(*const cef_main_args_t, *mut cef_app_t, *mut c_void) -> c_int;
type Initialize = unsafe extern "C" fn(*const cef_main_args_t, *const cef_settings_t, *mut cef_app_t, *mut c_void) -> c_int;
type Call = unsafe extern "C" fn();
type CreateBrowser = unsafe extern "C" fn(*const cef_window_info_t, *mut cef_client_t, *const cef_string_t, *const cef_browser_settings_t, *mut cef_dictionary_value_t, *mut cef_request_context_t) -> c_int;
type PostTask = unsafe extern "C" fn(cef_thread_id_t, *mut cef_task_t) -> c_int;

/// libcef.so, loaded, and the functions of it we call.
pub struct Lib {
    pub api_hash: ApiHash,
    pub execute_process: ExecuteProcess,
    pub initialize: Initialize,
    pub shutdown: Call,
    pub run_message_loop: Call,
    pub quit_message_loop: Call,
    pub browser_host_create_browser: CreateBrowser,
    pub post_task: PostTask,
}

impl Lib {
    /// Load `path` (libcef.so) and set the API version we speak. The libraries
    /// it needs that aren't there (most desktops have them all) are named in
    /// the error.
    pub fn load(path: &std::path::Path) -> Result<Self> {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes())?;
        // SAFETY: a path; libcef.so's initializers only set things up.
        let lib = unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
        if lib.is_null() {
            // SAFETY: dlerror's message, right after the failure.
            let why = unsafe { libc::dlerror() };
            let why = if why.is_null() { "unknown".into() } else { unsafe { CStr::from_ptr(why) }.to_string_lossy().into_owned() };
            return Err(anyhow!("{why}"));
        }
        let sym = |name: &CStr| -> Result<*mut c_void> {
            // SAFETY: a library just opened, a symbol name.
            let p = unsafe { libc::dlsym(lib, name.as_ptr()) };
            if p.is_null() { Err(anyhow!("libcef.so has no {}", name.to_string_lossy())) } else { Ok(p) }
        };
        // SAFETY: each symbol is CEF's function of that name, of this type
        // (cef_*_capi.h, cef_api_hash.h).
        let me = unsafe {
            Self {
                api_hash: std::mem::transmute::<*mut c_void, ApiHash>(sym(c"cef_api_hash")?),
                execute_process: std::mem::transmute::<*mut c_void, ExecuteProcess>(sym(c"cef_execute_process")?),
                initialize: std::mem::transmute::<*mut c_void, Initialize>(sym(c"cef_initialize")?),
                shutdown: std::mem::transmute::<*mut c_void, Call>(sym(c"cef_shutdown")?),
                run_message_loop: std::mem::transmute::<*mut c_void, Call>(sym(c"cef_run_message_loop")?),
                quit_message_loop: std::mem::transmute::<*mut c_void, Call>(sym(c"cef_quit_message_loop")?),
                browser_host_create_browser: std::mem::transmute::<*mut c_void, CreateBrowser>(sym(c"cef_browser_host_create_browser")?),
                post_task: std::mem::transmute::<*mut c_void, PostTask>(sym(c"cef_post_task")?),
            }
        };
        // Before anything else: the API version our structures are laid out
        // for. A libcef.so that no longer takes it says so here.
        // SAFETY: CEF's function; the hash it returns is its own.
        let hash = unsafe { (me.api_hash)(API_VERSION, 0) };
        if hash.is_null() {
            return Err(anyhow!("this CEF doesn't speak API version {API_VERSION}"));
        }
        Ok(me)
    }
}
