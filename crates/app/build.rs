//! Windows: the app icon (Explorer, Start menu, the installer's shortcuts) and
//! version details, built into the .exe.
//!
//! `hw_decode`: the platform has an in-process hardware video decoder
//! (`capture::decode`) for playback, scrub frames and the filmstrip.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(hw_decode)");
    if matches!(std::env::var("CARGO_CFG_TARGET_OS").as_deref(), Ok("windows" | "macos")) {
        println!("cargo::rustc-cfg=hw_decode");
    }
    #[cfg(windows)]
    {
        // sherpa-onnx (voice.rs) ships static libraries built against the
        // static C runtime; Rust uses the DLL one. Each side frees what it
        // allocates, so one runtime for the linker to pick is enough.
        if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
            println!("cargo:rustc-link-arg-bins=/NODEFAULTLIB:LIBCMT");
        }
        println!("cargo:rerun-if-changed=assets/icon.ico");
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico").set("ProductName", "HesteClips").set("FileDescription", "HesteClips");
        res.compile().expect("embedding the Windows icon");
    }
}
