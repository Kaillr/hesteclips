#!/usr/bin/env bash
# Builds the Linux release into Releases/: HesteClips-linux-x64.tar.gz, the app
# with its desktop entry and icon, and its checksum (.sha256, for the app's
# updater). Unpack it anywhere and run ./hesteclips; on
# first launch it adds itself to the desktop's apps (linux_desktop.rs).
# ffmpeg is included, so there's nothing else to install.
# Beside it, the web page engine (HesteClips-web-engine-linux-x64-<cef>.tar.xz
# and its .sha256), which the app fetches the first time a web page is shown.
#
# The release workflow runs this for each new version. It also runs locally:
#   scripts/release-linux.sh 0.0.1-local
# Needs Rust and the build packages listed in the README.
set -euo pipefail

version="${1:?usage: release-linux.sh <version>}"
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

# Encoding, the editor, thumbnails and probing run ffmpeg. BtbN's GPL build,
# with x264, NVENC and VA-API; the shared-library one is ~40 MB smaller packed
# than the static one. Its RPATH is set to $ORIGIN/lib, so it finds its
# libraries in lib/ beside it (needs patchelf). It loads libva, libdrm and CUDA from the system when there
# (dlopen), so the GPU encoders use the distro's own drivers. Pinned by checksum.
# The app decodes video for playback with the same libavcodec (opened from
# lib/ at run time), so it's built against this build's headers.
# 8.1, not 9.0: BtbN builds 9.0 against the newest NVENC headers, whose
# encoder needs NVIDIA's driver 610 or newer, and GTX 10-series cards never
# get past 580. 8.1's headers (SDK 13.0) need 570.
ffmpeg_build="ffmpeg-n8.1.3-27-g8dca1586f4-linux64-gpl-shared-8.1"
ffmpeg_url="https://github.com/BtbN/FFmpeg-Builds/releases/download/autobuild-2026-10-30-13-01/$ffmpeg_build.tar.xz"
ffmpeg_sha256="bf4a4655c74faca6c6890ee68d2d61af54b1145efdb46d617cb26046f73eabf5"

name="HesteClips-linux-x64"
work="target/release-package"
stage="$work/$name"
out="Releases"
rm -rf "$stage" "$out"
mkdir -p "$stage" "$out"

# The version goes into the binary; the repo itself isn't changed (Cargo.lock
# follows the version, so it's put back too).
cp Cargo.toml "$work/Cargo.toml.orig"
cp Cargo.lock "$work/Cargo.lock.orig"
restore() {
    cp "$work/Cargo.toml.orig" Cargo.toml
    cp "$work/Cargo.lock.orig" Cargo.lock
}
trap restore EXIT
# ffmpeg first: the app is built against its headers. It goes next to the
# app, where it looks first; the libraries in lib/ beside it.
tarball="$work/$ffmpeg_build.tar.xz"
if [ ! -f "$tarball" ] || ! echo "$ffmpeg_sha256  $tarball" | sha256sum -c --status; then
    curl -fsSL "$ffmpeg_url" -o "$tarball"
    echo "$ffmpeg_sha256  $tarball" | sha256sum -c --status || { echo "ffmpeg download has the wrong checksum" >&2; exit 1; }
fi
rm -rf "$work/$ffmpeg_build"
tar -C "$work" -xf "$tarball"
# The NVIDIA driver its NVENC needs (the string after this log message in
# libavcodec): no newer than GTX 10-series cards' last, 580.
nvenc_driver="$(tr '\0' '\n' < "$work/$ffmpeg_build"/lib/libavcodec.so.[0-9][0-9] \
    | grep -a -A2 -x 'Loaded Nvenc version %d.%d' | grep -aE '^[0-9]+\.[0-9]+$' | head -1)"
[ -n "$nvenc_driver" ] || { echo "couldn't find the NVIDIA driver this ffmpeg's NVENC needs" >&2; exit 1; }
if [ "${nvenc_driver%%.*}" -gt 580 ]; then
    echo "this ffmpeg's NVENC needs NVIDIA's driver $nvenc_driver, newer than GTX 10-series cards get (580)" >&2
    exit 1
fi

sed -i.bak -E "s/^version = \"[^\"]*\"/version = \"$version\"/" Cargo.toml && rm Cargo.toml.bak
HESTECLIPS_FFMPEG_INCLUDE="$root/$work/$ffmpeg_build/include" cargo build --release -p hesteclips
restore
trap - EXIT

cp target/release/hesteclips "$stage/"
strip "$stage/hesteclips"

cp "$work/$ffmpeg_build/bin/ffmpeg" "$work/$ffmpeg_build/bin/ffprobe" "$stage/"
mkdir -p "$stage/lib"
cp -P "$work/$ffmpeg_build"/lib/*.so.* "$stage/lib/"
patchelf --set-rpath '$ORIGIN/lib' "$stage/ffmpeg" "$stage/ffprobe"
cp "$work/$ffmpeg_build/LICENSE.txt" "$stage/ffmpeg-LICENSE.txt"
"$stage/ffmpeg" -hide_banner -version >/dev/null
"$stage/ffprobe" -hide_banner -version >/dev/null

cp crates/app/assets/io.github.kaillr.HesteClips.desktop "$stage/"
cp crates/app/assets/glyph-icon/256.png "$stage/io.github.kaillr.HesteClips.png"
cat > "$stage/README.txt" <<EOF
HesteClips $version for Linux (x86-64)

Run ./hesteclips. The first time, it adds itself to your desktop's apps, so
after that you can start it from there (move this folder first if you want it
somewhere else, then run it once from its new place). It keeps itself up to
date, as long as this folder is yours to write to (in your home folder, not
/opt).

Needs a recent Linux (glibc 2.39+: Ubuntu 24.04, Fedora 40, Debian 13, Arch,
CachyOS or newer) with PipeWire, as desktops have them. ffmpeg is included
(ffmpeg, ffprobe and lib/, GPL: see ffmpeg-LICENSE.txt). Web pages in the
video are drawn by Chromium's engine (CEF), downloaded once the first time
you add one (about 107 MB, kept in ~/.local/share/hesteclips). If it doesn't start,
run it from a terminal: it says what's missing. Its log is in
~/.local/share/hesteclips/logs.

https://github.com/Kaillr/hesteclips
EOF

tar -C "$work" -czf "$out/$name.tar.gz" "$name"
# The app's updater checks the download against this before installing it.
(cd "$out" && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256")
echo "built $out/$name.tar.gz"

# The web page engine: Chromium, through CEF, which the app fetches from the
# release the first time a web page is shown (crates/capture/src/linux/web_engine.rs),
# so it's never part of the app's own download or its updates. CEF's official
# build, pinned by checksum, as much of it as off-screen pages take, stripped
# (its libcef.so is 1.5 GB with symbols, 278 MB without) and English only:
# about 107 MB packed. The version is the one the app speaks (CEF_VERSION,
# whose C API crates/capture/src/linux/cef.rs is laid out for). A version of
# the app from before web pages on Linux (a tag built again) has none.
if [ ! -f crates/capture/src/linux/web_engine.rs ]; then
    exit 0
fi
cef_build="cef_binary_156.0.26+g8ea0c73+chromium-156.0.8078.88_linux64_minimal"
cef_sha1="cacfbb7d7261fea682fb67c108de7e4347519ff7"
cef_version="$(sed -nE 's/^pub\(crate\) const CEF_VERSION: &str = "([^"]+)";/\1/p' crates/capture/src/linux/web_engine.rs)"
case "$cef_build" in
    "cef_binary_$cef_version+"*) ;;
    *) echo "the app speaks CEF $cef_version, but this script packs $cef_build" >&2; exit 1 ;;
esac
cef_tarball="$work/$cef_build.tar.bz2"
if [ ! -f "$cef_tarball" ] || ! echo "$cef_sha1  $cef_tarball" | sha1sum -c --status; then
    curl -fsSL "https://cef-builds.spotifycdn.com/${cef_build//+/%2B}.tar.bz2" -o "$cef_tarball"
    echo "$cef_sha1  $cef_tarball" | sha1sum -c --status || { echo "CEF download has the wrong checksum" >&2; exit 1; }
fi
rm -rf "$work/$cef_build" "$work/web-engine"
tar -C "$work" -xjf "$cef_tarball"
engine="$work/web-engine"
mkdir -p "$engine/locales"
cef="$work/$cef_build"
cp "$cef"/Release/{libcef.so,libvk_swiftshader.so,libvulkan.so.1,vk_swiftshader_icd.json,v8_context_snapshot.bin} "$engine/"
cp "$cef"/Resources/{icudtl.dat,resources.pak,chrome_100_percent.pak,chrome_200_percent.pak} "$engine/"
cp "$cef/Resources/locales/en-US.pak" "$engine/locales/"
cp "$cef/LICENSE.txt" "$engine/CEF-LICENSE.txt"
cp "$cef/CREDITS.html" "$engine/CREDITS.html"
strip --strip-all "$engine/libcef.so"
rm -rf "$cef"
engine_name="HesteClips-web-engine-linux-x64-$cef_version.tar.xz"
tar -C "$engine" -cf - . | xz -9 -T0 > "$out/$engine_name"
(cd "$out" && sha256sum "$engine_name" > "$engine_name.sha256")
echo "built $out/$engine_name"
