//! Windows: the app icon (Explorer, Start menu, the installer's shortcuts) and
//! version details, built into the .exe.

fn main() {
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=assets/icon.ico");
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico").set("ProductName", "HesteClips").set("FileDescription", "HesteClips");
        res.compile().expect("embedding the Windows icon");
    }
}
