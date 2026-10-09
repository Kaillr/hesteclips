#!/usr/bin/env bash
# Builds the Linux release into Releases/: HesteClips-linux-x64.tar.gz, the app
# with its desktop entry and icon, and its checksum (.sha256, for the app's
# updater). Unpack it anywhere and run ./hesteclips; on
# first launch it adds itself to the desktop's apps (linux_desktop.rs).
# ffmpeg is included, so there's nothing else to install.
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
ffmpeg_build="ffmpeg-n9.0.1-11-ge47273f4d9-linux64-gpl-shared-9.0"
ffmpeg_url="https://github.com/BtbN/FFmpeg-Builds/releases/download/autobuild-2026-08-31-13-27/$ffmpeg_build.tar.xz"
ffmpeg_sha256="30801e00d25f823c6da339378ea63c38e5265f237741909e1d6164b6b5bd3d33"

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
sed -i.bak -E "s/^version = \"[^\"]*\"/version = \"$version\"/" Cargo.toml && rm Cargo.toml.bak
cargo build --release -p hesteclips
restore
trap - EXIT

cp target/release/hesteclips "$stage/"
strip "$stage/hesteclips"

# Next to the app, where it looks first; the libraries in lib/ beside it.
tarball="$work/$ffmpeg_build.tar.xz"
if [ ! -f "$tarball" ] || ! echo "$ffmpeg_sha256  $tarball" | sha256sum -c --status; then
    curl -fsSL "$ffmpeg_url" -o "$tarball"
    echo "$ffmpeg_sha256  $tarball" | sha256sum -c --status || { echo "ffmpeg download has the wrong checksum" >&2; exit 1; }
fi
rm -rf "$work/$ffmpeg_build"
tar -C "$work" -xf "$tarball"
cp "$work/$ffmpeg_build/bin/ffmpeg" "$work/$ffmpeg_build/bin/ffprobe" "$stage/"
mkdir -p "$stage/lib"
cp -P "$work/$ffmpeg_build"/lib/*.so.* "$stage/lib/"
patchelf --set-rpath '$ORIGIN/lib' "$stage/ffmpeg" "$stage/ffprobe"
cp "$work/$ffmpeg_build/LICENSE.txt" "$stage/ffmpeg-LICENSE.txt"
"$stage/ffmpeg" -hide_banner -version >/dev/null
"$stage/ffprobe" -hide_banner -version >/dev/null

cp crates/app/assets/io.github.kaillr.HesteClips.desktop crates/app/assets/icon.svg "$stage/"
cat > "$stage/README.txt" <<EOF
HesteClips $version for Linux (x86-64)

Run ./hesteclips. The first time, it adds itself to your desktop's apps, so
after that you can start it from there (move this folder first if you want it
somewhere else, then run it once from its new place). It keeps itself up to
date, as long as this folder is yours to write to (in your home folder, not
/opt).

Needs a recent Linux (glibc 2.39+: Ubuntu 24.04, Fedora 40, Debian 13, Arch,
CachyOS or newer) with PipeWire, as desktops have them. ffmpeg is included
(ffmpeg, ffprobe and lib/, GPL: see ffmpeg-LICENSE.txt). If it doesn't start,
run it from a terminal: it says what's missing. Its log is in
~/.local/share/hesteclips/logs.

https://github.com/Kaillr/hesteclips
EOF

tar -C "$work" -czf "$out/$name.tar.gz" "$name"
# The app's updater checks the download against this before installing it.
(cd "$out" && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256")
echo "built $out/$name.tar.gz"
