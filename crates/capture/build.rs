//! Linux: the libavcodec shim for in-process video decoding
//! (`src/linux/avdec.c`), against libavcodec's headers:
//! `HESTECLIPS_FFMPEG_INCLUDE` (the release points it at the ffmpeg it ships),
//! else pkg-config's, else the system's. Without them it's built to say so,
//! and playback uses ffmpeg processes instead.
//!
//! And the NVENC shim for recording (`src/linux/nvenc.c`) and the CUDA one
//! for decoding on NVIDIA's card (`src/linux/cuda.c`), against NVIDIA's
//! headers in `src/linux/ffnvcodec/`: nothing to install.

use std::path::{Path, PathBuf};

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        return;
    }
    println!("cargo:rerun-if-changed=src/linux/nvenc.c");
    println!("cargo:rerun-if-changed=src/linux/cuda.c");
    println!("cargo:rerun-if-changed=src/linux/ffnvcodec");
    cc::Build::new().file("src/linux/nvenc.c").compile("hcnvenc");
    cc::Build::new().file("src/linux/cuda.c").compile("hccuda");

    println!("cargo:rerun-if-changed=src/linux/avdec.c");
    println!("cargo:rerun-if-env-changed=HESTECLIPS_FFMPEG_INCLUDE");
    let has_headers = |dir: &Path| dir.join("libavcodec/avcodec.h").exists() && dir.join("libswresample/version_major.h").exists();
    // Asked for (a release): those headers or nothing, never built without.
    if let Some(dir) = std::env::var_os("HESTECLIPS_FFMPEG_INCLUDE").map(PathBuf::from) {
        assert!(has_headers(&dir), "HESTECLIPS_FFMPEG_INCLUDE={} has no libavcodec headers", dir.display());
    }
    let include = std::env::var_os("HESTECLIPS_FFMPEG_INCLUDE")
        .map(PathBuf::from)
        .or_else(pkg_config_include)
        .or_else(|| Some(PathBuf::from("/usr/include")))
        .filter(|dir| has_headers(dir));
    let mut build = cc::Build::new();
    build.file("src/linux/avdec.c");
    match include {
        Some(dir) => {
            // Built again when they change (a system update to another
            // libavcodec): else the app asks for a library that's gone.
            println!("cargo:rerun-if-changed={}", dir.join("libavcodec/version_major.h").display());
            build.include(dir);
        }
        None => {
            println!(
                "cargo:warning=libavcodec's headers aren't installed: playback will use ffmpeg processes \
                 (install libavcodec-dev and libswresample-dev, or ffmpeg-devel)"
            );
            build.define("HCAV_NO_LIBAV", None);
        }
    }
    build.compile("hcavdec");
}

/// Where pkg-config says libavcodec's headers are (nothing for the system's
/// own include directory).
fn pkg_config_include() -> Option<PathBuf> {
    let out = std::process::Command::new("pkg-config").args(["--cflags-only-I", "libavcodec"]).output().ok()?;
    let text = String::from_utf8(out.stdout).ok()?;
    text.split_whitespace().filter_map(|f| f.strip_prefix("-I")).map(Path::new).find(|d| d.join("libavcodec/avcodec.h").exists()).map(Path::to_path_buf)
}
