#!/usr/bin/env bash
# Builds the macOS release into Releases/: HasteClip-mac-arm64.dmg, holding
# HasteClip.app with ffmpeg inside, so there's nothing else to install. Drag
# it to Applications and open it.
#
# Signed with HasteClip's own certificate, not an Apple Developer ID (there
# isn't one), so the first launch has to be allowed in System Settings →
# Privacy & Security (see the README).
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

name="HasteClip-mac-arm64"
work="target/release-package"
app="$work/dmg/HasteClip.app"
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
MACOSX_DEPLOYMENT_TARGET=13.0 cargo build --release -p hasteclip --target aarch64-apple-darwin
restore
trap - EXIT

cp target/aarch64-apple-darwin/release/hasteclip "$app/Contents/MacOS/"
strip -x "$app/Contents/MacOS/hasteclip"
cp crates/app/assets/hasteclip.icns "$app/Contents/Resources/"

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
    <key>CFBundleName</key><string>HasteClip</string>
    <key>CFBundleDisplayName</key><string>HasteClip</string>
    <key>CFBundleIdentifier</key><string>io.github.kaillr.HasteClip</string>
    <key>CFBundleExecutable</key><string>hasteclip</string>
    <key>CFBundleIconFile</key><string>hasteclip</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$version</string>
    <key>CFBundleVersion</key><string>$version</string>
    <key>LSMinimumSystemVersion</key><string>13.0</string>
    <key>LSApplicationCategoryType</key><string>public.app-category.video</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSMicrophoneUsageDescription</key><string>HasteClip records your microphone into your clips when you add it as a source.</string>
    <key>NSCameraUsageDescription</key><string>HasteClip puts your webcam into your clips when you turn it on.</string>
</dict>
</plist>
PLIST

# Signed with HasteClip's own certificate (MAC_SIGNING_P12, base64, and
# MAC_SIGNING_PASSWORD), the same for every release: macOS ties what it's
# allowed (Screen Recording, Input Monitoring) to the app's identifier and
# that certificate, so a new version keeps them. Ad hoc, each build is only
# itself and loses them. It isn't Apple's, so Gatekeeper still asks the first
# time. Without the certificate (a local build), ad hoc. ffmpeg keeps its own
# signature; the app is sealed around it.
sign=-
if [ -n "${MAC_SIGNING_P12:-}" ]; then
    echo "signing: making a keychain with the certificate"
    keychain="$PWD/$work/signing.keychain-db"
    kc_pass="$(openssl rand -hex 16)"
    rm -f "$keychain"
    security create-keychain -p "$kc_pass" "$keychain"
    security set-keychain-settings "$keychain"
    security unlock-keychain -p "$kc_pass" "$keychain"
    echo "$MAC_SIGNING_P12" | base64 --decode > "$work/signing.p12"
    security import "$work/signing.p12" -k "$keychain" -P "$MAC_SIGNING_PASSWORD" -T /usr/bin/codesign >/dev/null
    rm "$work/signing.p12"
    security set-key-partition-list -S apple-tool:,apple: -s -k "$kc_pass" "$keychain" >/dev/null
    # On the search list too, ahead of the others: codesign on GitHub's
    # macOS 15 runners didn't find the key with --keychain alone ("The
    # specified item could not be found in the keychain"). The list is put
    # back as it was afterwards.
    searched=()
    while IFS= read -r k; do searched+=("$k"); done < <(security list-keychains -d user | sed -E 's/^[[:space:]]*"(.*)"$/\1/')
    security list-keychains -d user -s "$keychain" ${searched[@]+"${searched[@]}"}
    trap 'security list-keychains -d user -s ${searched[@]+"${searched[@]}"}; security delete-keychain "$keychain" 2>/dev/null || true' EXIT
    # Not trusted (it isn't Apple's), so it's found by its hash.
    sign="$(security find-identity -p codesigning "$keychain" | awk '/"HasteClip"/ {print $2; exit}')"
    [ -n "$sign" ] || { echo "the signing certificate wasn't found" >&2; exit 1; }
    echo "signing: with $sign"
fi
codesign --force --sign "$sign" ${keychain:+--keychain "$keychain"} "$app/Contents/MacOS/hasteclip"
codesign --force --sign "$sign" ${keychain:+--keychain "$keychain"} "$app"
codesign --verify --strict "$app"
codesign -dr - "$app" 2>&1 | grep designated

# What the installed app updates itself from (update_mac.rs): the app,
# zipped with ditto (which keeps its signature), and its checksum.
ditto -c -k --keepParent "$app" "$out/$name.zip"
(cd "$out" && shasum -a 256 "$name.zip" > "$name.zip.sha256")

ln -s /Applications "$work/dmg/Applications"
hdiutil create -volname HasteClip -srcfolder "$work/dmg" -fs HFS+ -format UDZO -ov "$out/$name.dmg" >/dev/null
echo "built $out/$name.dmg and $out/$name.zip"
