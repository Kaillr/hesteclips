#!/usr/bin/env bash
# Builds the macOS release into Releases/: HesteClips-mac-arm64.dmg, holding
# HesteClips.app with ffmpeg inside, so there's nothing else to install. Drag
# it to Applications and open it.
#
# Not signed with a Developer ID (there isn't one), only ad hoc, which Apple
# Silicon needs to run it at all; so the first launch has to be allowed in
# System Settings → Privacy & Security (see the README).
#
# The release workflow runs this for each new version. It also runs locally:
#   scripts/release-mac.sh 0.0.1-local
set -euo pipefail

version="${1:?usage: release-mac.sh <version>}"
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

# The editor, thumbnails and probing run ffmpeg. A static arm64 build that
# only links system frameworks (VideoToolbox, AudioToolbox), pinned by checksum.
ffmpeg_url="https://ffmpeg.martin-riedl.de/download/macos/arm64/1789931890_9.0.2"
ffmpeg_sha256="c8ed4c4e6978a03c485edbfe4e0a5dc2380f8a30bba5150531b31b094492d924"
ffprobe_sha256="fcbe839537485eaee7a7a8bc5cbc0f90d53617e80943e8a5b2e31cb851197ea6"

name="HesteClips-mac-arm64"
work="target/release-package"
app="$work/dmg/HesteClips.app"
out="Releases"
rm -rf "$work/dmg" "$out"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources" "$out"

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
MACOSX_DEPLOYMENT_TARGET=13.0 cargo build --release -p hesteclips --target aarch64-apple-darwin
restore
trap - EXIT

cp target/aarch64-apple-darwin/release/hesteclips "$app/Contents/MacOS/"
strip -x "$app/Contents/MacOS/hesteclips"
cp crates/app/assets/hesteclips.icns "$app/Contents/Resources/"

# Next to the app's binary, where media::tool looks first.
fetch() { # name sha256
    local zip="$work/$1-9.0.2.zip"
    if [ ! -f "$zip" ] || ! echo "$2  $zip" | shasum -a 256 -c --status; then
        curl -fsSL "$ffmpeg_url/$1.zip" -o "$zip"
        echo "$2  $zip" | shasum -a 256 -c --status || { echo "$1 download has the wrong checksum" >&2; exit 1; }
    fi
    unzip -oq "$zip" -d "$app/Contents/MacOS"
}
fetch ffmpeg "$ffmpeg_sha256"
fetch ffprobe "$ffprobe_sha256"
"$app/Contents/MacOS/ffmpeg" -hide_banner -version >/dev/null
"$app/Contents/MacOS/ffprobe" -hide_banner -version >/dev/null
curl -fsSL "https://raw.githubusercontent.com/FFmpeg/FFmpeg/n9.0.2/COPYING.GPLv3" -o "$app/Contents/Resources/ffmpeg-LICENSE.txt"

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>HesteClips</string>
    <key>CFBundleDisplayName</key><string>HesteClips</string>
    <key>CFBundleIdentifier</key><string>io.github.kaillr.HesteClips</string>
    <key>CFBundleExecutable</key><string>hesteclips</string>
    <key>CFBundleIconFile</key><string>hesteclips</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$version</string>
    <key>CFBundleVersion</key><string>$version</string>
    <key>LSMinimumSystemVersion</key><string>13.0</string>
    <key>LSApplicationCategoryType</key><string>public.app-category.video</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSMicrophoneUsageDescription</key><string>HesteClips records your microphone into your clips when you add it as a source.</string>
    <key>NSCameraUsageDescription</key><string>HesteClips puts your webcam into your clips when you turn it on.</string>
</dict>
</plist>
PLIST

# Ad hoc: ffmpeg keeps its own signature; the app is sealed around it.
codesign --force --sign - "$app/Contents/MacOS/hesteclips"
codesign --force --sign - "$app"
codesign --verify --strict "$app"

ln -s /Applications "$work/dmg/Applications"
hdiutil create -volname HesteClips -srcfolder "$work/dmg" -fs HFS+ -format UDZO -ov "$out/$name.dmg" >/dev/null
echo "built $out/$name.dmg"
