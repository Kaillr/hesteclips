<p align="center">
  <img src="crates/app/assets/icon-1024.png" width="128" alt="HesteClips icon">
</p>

<h1 align="center">HesteClips</h1>

<p align="center">
  A fast, no-nonsense clip recorder and editor for macOS, Windows and Linux.<br>
  It keeps the last few minutes of your game ready. Press a key when something happens, trim it, share it.<br>
  Free and open source. No ads, no account, no watermark.
</p>

<p align="center">
  <a href="https://github.com/Kaillr/hesteclips/releases/latest"><b>Download</b></a> ·
  <a href="#features">Features</a> ·
  <a href="#building-from-source">Build from source</a>
</p>

<p align="center">
  <img src="docs/images/library.webp" alt="The HesteClips library: clips grouped by day, with games and collections in the sidebar">
</p>

## Contents

- [Download](#download)
- [Getting started](#getting-started)
- [Features](#features)
- [Platform support](#platform-support)
- [Building from source](#building-from-source)
- [License](#license)

## Download

Download yours from the [latest release](https://github.com/Kaillr/hesteclips/releases/latest). Every package includes ffmpeg.

| System | File | Requirements | Updates |
|---|---|---|---|
| macOS | `HesteClips-mac-arm64.dmg` | macOS 13 or later, Apple Silicon | Automatic |
| Windows | `HesteClips-win-Setup.exe` | Windows 10 (2004) or 11 | Automatic |
| Linux | `HesteClips-linux-x64.tar.gz` | x86-64, Wayland or X11, PipeWire | Automatic |

Automatic updates download in the background and install when you quit, never mid-game. You can turn them off in *Settings → App*.

<details>
<summary><b>macOS</b></summary>

Open the `.dmg` and drag HesteClips to Applications.

The app is signed with HesteClips' own certificate, not by Apple, so macOS blocks the first launch. Open it once, then go to *System Settings → Privacy & Security* and click *Open Anyway*. You can also run `xattr -dr com.apple.quarantine /Applications/HesteClips.app` instead. Updates don't ask again.

On first launch, HesteClips shows what it needs macOS to allow: **Screen & System Audio Recording**, and **Input Monitoring** so your shortcuts work in games that take over the whole screen. Allow them, then restart HesteClips. macOS keeps them through updates. You can see and change them later in *Settings → Permissions*.
</details>

<details>
<summary><b>Windows</b></summary>

Run `HesteClips-win-Setup.exe`. It installs for your user only, so there's no admin prompt.

Windows asks for nothing, except microphone access if *Settings → Privacy & security → Microphone* is off for desktop apps. Some GPU overlays use the same shortcut: NVIDIA's Instant Replay, for example, uses <kbd>Alt</kbd> + <kbd>F10</kbd>. HesteClips' settings tell you if a shortcut couldn't be registered.
</details>

<details>
<summary><b>Linux</b></summary>

Unpack the tarball into a folder that's yours to write to, so updates can replace it, and run `./hesteclips`. On first launch it adds itself to your apps (`~/.local/share/applications`).

The first time you record or open **Sources**, your desktop asks which screen to share. HesteClips remembers your choice, and *Sources → Choose another…* asks again. On Wayland, global shortcuts go through your desktop: it may ask once to allow them, and you change their keys in its keyboard settings.
</details>

## Getting started

1. **Open HesteClips.** Clipping starts right away. It keeps the last minute in memory, and nothing is written to your drive until you save.
2. **Play.** When something happens, press <kbd>Alt</kbd> + <kbd>F10</kbd> (<kbd>Option</kbd> + <kbd>F10</kbd> on macOS). You can also say **“hashtag HesteClip that”** if a microphone is set up.
3. **Find it in your library.** The clip lands under *Today*, in its game's folder. Click it to watch, then **Edit** to trim it or **Share** to send it.

Then make it yours:

- **Sources** is what goes into your clips: the screen or a game's window, your microphone, your desktop sound, single apps, and a webcam.
- **Settings** has the clip length (5 seconds to an hour), resolution, frame rate, quality, file format, the clips folder, and more shortcuts. Start or stop clipping, start or stop recording, and take a screenshot are off until you pick a key for them.

Clips are saved in a `hesteclips` folder in your Videos folder (Movies on macOS), with one folder per game. You can change this in *Settings → Clips*.

## Features

### Clip and record

- **Replay buffer.** It keeps the last 5 seconds to 1 hour in memory. Save it with a shortcut, or by voice: “hashtag HesteClip that” is recognised on your computer and never sent anywhere.
- **Clips named after the play.** “3 kills on Mirage”, “Pentakill as Jinx”, “412pp FC”: clips of Counter-Strike 2, Dota 2, League of Legends and osu! are named after what happened. Other games' clips are named by the time.
- **Recording.** Record straight to disk for as long as you like. A crash or power loss still leaves a playable file.
- **Go live.** Stream to Twitch, YouTube, Kick or a server of your own, to several at once, at a size, frame rate and bitrate of the stream's own, frame blending and all. Clipping and recording go on beside it, all from one capture.
- **Native, hardware encoding.** H.264 on your graphics card's video engine, at native resolution or scaled down, up to 540 fps at 1080p. HesteClips is written in Rust and draws on the GPU, with no browser running behind your game.
- **Any shape.** Record the whole screen or a game's window, in its own aspect ratio or another, like 16:9 from an ultrawide screen.
- **Sound as sources, not tracks.** Add your microphone, desktop sound, or single apps like the game, Discord or music. For each one, choose whether it's in the clip's sound, has its own track for editing later, or both. Each source has a live meter and a fader. There's noise reduction and mono for microphones, and a limiter on the mix.
- **Webcam.** Show your camera in a corner, placed and cropped by dragging it on the preview.

### Edit

<p align="center">
  <img src="docs/images/editor.webp" alt="The HesteClips editor: the clip's picture above a timeline with a video lane and one lane per audio track">
</p>

- **Frame-accurate trimming.** Drag the ends, or press <kbd>I</kbd> and <kbd>O</kbd>.
- **Cut out the slow parts.** Press <kbd>S</kbd> to split at the playhead, then click a part and press <kbd>Delete</kbd>, or drag a split open. Playing skips what's cut out, and it stays on the timeline, dimmed, to drag back.
- **Fast saving.** Only the frames around each cut are re-encoded, and the rest is copied as it is. A trim saves in seconds and looks exactly like the original.
- **Every track on its own lane.** Turn your mic down after the fact, and draw volume changes along the timeline.
- **Crop** to 16:9, 4:3, 1:1, 9:16 for phones, or any shape.
- **Export smaller copies** at a lower resolution or frame rate, or to fit a size, like under 25 MB.
- **Non-destructive.** Your original recording is kept, so you can revert at any time, or free up the space once you're happy.

### Watch, organize and share

<p align="center">
  <img src="docs/images/player.webp" alt="The HesteClips player: a clip playing, with its moments marked along the timeline">
</p>

- **A library that sorts itself.** Clips are grouped by day and by game automatically. Collections hold the ones worth keeping. Search, sort, and rename (<kbd>F2</kbd>) or delete a clip with one key.
- **A player made for clips.** Step frame by frame, jump between moments such as kills and your own markers (<kbd>B</kbd>), and change the speed. Press <kbd>?</kbd> for every shortcut.
- **Upload to HesteFiles.** Connect your HesteFiles account in *Settings → App* and upload a clip into a folder of your storage (a folder per game if you like), with a public link to share if you want one.
- **Or share the file.** Copy a clip and paste it anywhere, or drag one (or ten) out of the library into Discord, a chat or a folder. You can also use your system's share sheet.
- **Export and import.** Move clips to another computer with their edits, markers and collections in one `.hesteclips` file.

## Platform support

| Feature | macOS | Windows | Linux |
|---|:-:|:-:|:-:|
| Replay buffer, recording, editor, library | ✅ | ✅ | ✅ |
| HesteFiles uploads, copy, export and import | ✅ | ✅ | ✅ |
| Voice saving, named clips, per-app sound, webcam | ✅ | ✅ | ✅ |
| Several webcams, other displays, windows, images, text and videos in the video | ✅ | ✅ | ✅ |
| A web page in the video (alerts, chat) | ✅ | ✅ | ✅ (see below) |
| Record a single game or app window | ✅ | ✅ | ✅ (see below) |
| Drag clips out, system share sheet | ✅ | ✅ | Drag out on Wayland (no share sheet) |
| Frame blending | ✅ | ✅ | ✅ |
| Go live (streaming) | ✅ | ✅ | ✅ |
| Automatic updates | ✅ | ✅ | ✅ |
| Screenshots | ✅ | ✅ | ✅ |
| Game capture for fullscreen games | — | ✅ | — |

**Games and apps on Linux:** every window on KDE Plasma, on X11 desktops, and on Sway and other desktops with the ext window capture protocols (Sway 1.11 and later). On GNOME, the windows of X11 apps, which includes games run through Proton or Wine; GNOME doesn't let apps follow its native Wayland windows, so record the whole screen for those.

**Web pages on Linux** are drawn by Chromium's engine ([CEF](https://bitbucket.org/chromiumembedded/cef), the one OBS uses), on your graphics card where it can. It isn't part of the download: the first time you add a web page, HesteClips downloads it once from its releases (about 107 MB) and keeps it in `~/.local/share/hesteclips`. A build of your own can use an engine unpacked elsewhere with `HESTECLIPS_WEB_ENGINE_DIR=<folder>`.

**Encoders:** VideoToolbox on macOS. NVENC, AMF or Quick Sync through Media Foundation on Windows. NVENC or VA-API through ffmpeg on Linux, with x264 when there's no GPU encoder. A stream gets an encoder of its own (on Linux, always ffmpeg's).

**Frame blending** captures several times faster than the clip's frame rate and blends those pictures into each frame, for smooth motion.

**Game capture** handles fullscreen games that Windows' own capture records as a frozen picture, or without their cursor (such as fullscreen OpenGL games like osu!). It uses [OBS Studio](https://obsproject.com)'s game capture hook. It is never used on games with anti-cheat unless you add the game in *Settings → Video → Game capture*. Those games are found from their anti-cheat's files, [AreWeAntiCheatYet](https://areweanticheatyet.com)'s list and a few known games.

**Named clips** read what the game reports itself. Counter-Strike 2 and Dota 2 use Valve's Game State Integration; Dota 2 also needs `-gamestateintegration` in its launch options. League of Legends uses its Live Client Data API. osu! needs [tosu](https://github.com/tosuapp/tosu) or gosumemory running.

## Building from source

You need:

- [Rust](https://rustup.rs) (stable).
- [ffmpeg](https://ffmpeg.org) on your `PATH`, or next to the app. The editor, thumbnails and playback use it, and so does encoding on Linux. On macOS, run `brew install ffmpeg`; on Windows, `winget install Gyan.FFmpeg`. On Linux, your distribution's ffmpeg must encode and decode H.264. Debian, Ubuntu and Arch's packages do. Fedora's own `ffmpeg-free` doesn't, so use RPM Fusion's: `sudo dnf swap ffmpeg-free ffmpeg --allowerasing`.
- On Linux: a C compiler, clang, and the PipeWire, D-Bus, ALSA and FFmpeg (libavcodec) headers. On Fedora, run `sudo dnf install gcc clang-devel pipewire-devel dbus-devel alsa-lib-devel ffmpeg-free-devel`. On Debian or Ubuntu, run `sudo apt install build-essential libclang-dev libpipewire-0.3-dev libdbus-1-dev libasound2-dev libavcodec-dev libswresample-dev`. You also need an xdg-desktop-portal for your desktop (GNOME, KDE Plasma, wlroots, Hyprland). Without the FFmpeg headers it still builds, but playback decodes through ffmpeg processes: no instant play or full-quality scrubbing.

Then build and run:

```sh
git clone https://github.com/Kaillr/hesteclips.git
cd hesteclips
cargo run --release
```

On macOS, a build run from a terminal asks for permissions on the terminal app's behalf. On Windows, `pwsh scripts/fetch-game-hook.ps1` puts OBS Studio's game capture hook in `target/game-hook`, where a development build finds it. These are signed GPLv2 files that run as separate programs. Without them, games are recorded with Windows' own capture only.

### Project structure

| Crate | Purpose |
|---|---|
| `crates/app` | The desktop app (egui): library, player, editor, sources, settings |
| `crates/capture` | Screen and audio capture, mixing, encoding and writing files |
| `crates/media` | Probing, preview decoding and rendering edits |
| `crates/hestefiles` | The HesteFiles client: account, folders and uploads |

### Releasing

Releases are cut by hand: run *Actions → Release → Run workflow* on `main`. [semantic-release](https://semantic-release.gitbook.io) picks the version from the [Conventional Commits](https://www.conventionalcommits.org) since the last tag:

- `fix:` makes a patch release.
- `feat:` makes a minor release.
- `feat!:`, or a `BREAKING CHANGE:` footer, makes a major release.
- `chore:`, `docs:`, `refactor:`, `ci:` and the like don't make a release on their own.

The release scripts build the app and bundle ffmpeg:

- `scripts/release.ps1` (Windows) also bundles OBS Studio's game capture hook. It packs everything with [Velopack](https://velopack.io) into the installer and the delta update packages.
- `scripts/release-mac.sh` builds the `.dmg`.
- `scripts/release-linux.sh` builds the tarball.

## License

HesteClips is free software. You can use, change and share it under the [GNU General Public License, version 3 or later](LICENSE). Copies and changed versions must stay under the same license, with their source available and this project's copyright notices kept.

The releases bundle [ffmpeg](https://ffmpeg.org) (GPL), the [Hanken Grotesk](https://fonts.google.com/specimen/Hanken+Grotesk) font (SIL Open Font License), and, on Windows, [OBS Studio](https://obsproject.com)'s game capture hook (GPL-2.0-or-later).
