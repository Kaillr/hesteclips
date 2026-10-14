//! Start and stop a replay-buffer recording of the screen over and over,
//! printing this process's GPU memory after each stop: what a
//! stopped recording leaves behind. Windows only.
//! Env: ROUNDS=n (5), SECS=n per round (3), FPS=n (60), BLEND=n (looks per
//! frame, 3), HEIGHT=n (1080), SAVE=1 (save a clip each round).
#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use capture::{EncodeSettings, Mode, mixer::LiveAudio};
    let env = |k: &str| std::env::var(k).ok();
    let num = |k: &str, d: u32| env(k).and_then(|v| v.parse().ok()).unwrap_or(d);
    let out = std::env::temp_dir().join("hc");
    let settings = EncodeSettings {
        output_dir: out.clone(),
        container_ext: "mp4".into(),
        fps: num("FPS", 60),
        blend_samples: num("BLEND", 3),
        video_bitrate_kbps: 20000,
        target_height: Some(num("HEIGHT", 1080)),
        use_hardware: true,
        replay_seconds: 30,
        video: capture::VideoSource::Screen { id: String::new(), hook: capture::GameHook { auto: false, allowed: Vec::new() } },
        away_screen: None,
        webcam: None,
        sources: Vec::new(),
    };
    crash_report();
    println!("before: {}", memory());
    let live = LiveAudio::new();
    let mut rec = capture::default_recorder(live);
    for round in 1..=num("ROUNDS", 5) {
        rec.start(Mode::ReplayBuffer, &settings)?;
        std::thread::sleep(std::time::Duration::from_secs(num("SECS", 3) as u64));
        let running = memory();
        if env("SAVE").is_some() {
            println!("  saved: {:?}", rec.save_clip(&out).and_then(|p| p.finish()));
        }
        rec.stop(None)?;
        std::thread::sleep(std::time::Duration::from_millis(500));
        println!("round {round}: while recording {running}; after stop {}", memory());
    }
    Ok(())
}

/// This process's GPU memory on the default adapter (DXGI's own count of
/// what it has made resident).
#[cfg(windows)]
fn memory() -> String {
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, DXGI_QUERY_VIDEO_MEMORY_INFO, IDXGIAdapter3, IDXGIFactory1};
    use windows::core::Interface;
    let gpu = unsafe {
        CreateDXGIFactory1::<IDXGIFactory1>().and_then(|f| f.EnumAdapters1(0)).and_then(|a| a.cast::<IDXGIAdapter3>()).and_then(|a| {
            let mut info = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
            a.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut info).map(|()| info.CurrentUsage)
        })
    };
    format!("GPU {} MB", gpu.map_or(-1, |b| (b >> 20) as i64))
}

/// On an access violation: which module it was in, and the stack (build with
/// `CARGO_PROFILE_RELEASE_DEBUG=line-tables-only` for names).
#[cfg(windows)]
fn crash_report() {
    #[repr(C)]
    struct Record {
        code: u32,
        flags: u32,
        inner: *mut Record,
        address: *mut std::ffi::c_void,
        count: u32,
        info: [usize; 15],
    }
    #[repr(C)]
    struct Pointers {
        record: *mut Record,
        context: *mut std::ffi::c_void,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn AddVectoredExceptionHandler(first: u32, handler: unsafe extern "system" fn(*mut Pointers) -> i32) -> *mut std::ffi::c_void;
        fn GetModuleHandleExW(flags: u32, address: *const std::ffi::c_void, module: *mut *mut std::ffi::c_void) -> i32;
        fn GetModuleFileNameW(module: *mut std::ffi::c_void, name: *mut u16, size: u32) -> u32;
    }
    unsafe extern "system" fn handler(p: *mut Pointers) -> i32 {
        let r = unsafe { &*(*p).record };
        if r.code == 0xC000_0005 {
            let mut module = std::ptr::null_mut();
            let mut name = [0u16; 260];
            // FROM_ADDRESS | UNCHANGED_REFCOUNT
            let len = unsafe {
                if GetModuleHandleExW(4 | 2, r.address, &mut module) != 0 { GetModuleFileNameW(module, name.as_mut_ptr(), 260) } else { 0 }
            };
            eprintln!(
                "ACCESS VIOLATION at {:?} ({} + {:#x}), {} address {:#x}, thread {:?}\n{}",
                r.address,
                String::from_utf16_lossy(&name[..len as usize]),
                r.address as usize - module as usize,
                if r.info[0] == 0 { "reading" } else if r.info[0] == 1 { "writing" } else { "executing" },
                r.info[1],
                std::thread::current().name(),
                std::backtrace::Backtrace::force_capture()
            );
        }
        0
    }
    unsafe { AddVectoredExceptionHandler(1, handler) };
}

#[cfg(not(windows))]
fn main() {}
