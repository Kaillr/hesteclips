//! Windows: the app icon (Explorer, Start menu, the installer's shortcuts) and
//! version details, built into the .exe.
//!
//! `hw_decode`: the platform has an in-process video decoder
//! (`capture::decode`) for playback, scrubbing and the filmstrip: the
//! graphics card's on Windows and macOS, libavcodec's on Linux.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(hw_decode)");
    if matches!(std::env::var("CARGO_CFG_TARGET_OS").as_deref(), Ok("windows" | "macos" | "linux")) {
        println!("cargo::rustc-cfg=hw_decode");
    }
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=assets/icon.ico");
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico").set("ProductName", "HesteClips").set("FileDescription", "HesteClips");
        res.compile().expect("embedding the Windows icon");
    }
}
