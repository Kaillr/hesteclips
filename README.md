<p align="center">
  <img src="crates/app/assets/icon-1024.png" width="128" alt="HesteClips icon">
</p>

<h1 align="center">HesteClips</h1>

<p align="center">
  A fast, no-nonsense clip recorder and trimmer.<br>
  Keep a replay buffer running, hit a hotkey when something happens, and share the clip — no watermarks, no account.
</p>

---

## Features

- **Replay buffer** — always keeps the last 15 seconds to 5 minutes in memory. Press a hotkey to save it as a clip; nothing is written to disk until you do.
- **Regular recording** — record straight to disk. Files are written so that even a crash or power loss leaves a playable recording.
- **Audio sources, not tracks** — add your microphone, your desktop sound, or individual apps (a game, Discord, music). For each one, choose whether it's part of the clip and whether it gets its own track for editing later.
- **Live levels** — every source has a meter and a volume fader, plus a meter for the final mix and an optional limiter, so you can get levels right before you play.
- **Non-destructive editor** — trim frame-accurately, rebalance each audio track, and draw volume changes on the timeline. Your original recording is never modified, and you can revert any time.
- **Fast saving** — edits only re-encode the few frames around each cut; the rest is copied as-is, so saving a trim takes seconds.
- **Clip library** — your clips grouped by day, with thumbnails, quick playback, rename, drag-and-drop into any app, and sharing (copy, AirDrop, Messages, Mail).
- **High quality** — hardware H.264 encoding at native resolution or downscaled, up to 240 fps, with multi-track AAC audio in MP4 or MOV.

## Platform support

| Platform | Status |
|---|---|
| macOS 13+ | ✅ Recording, editing and library all work |
| Windows | 🚧 Planned next (Windows Graphics Capture) |
| Linux | 📋 Planned (PipeWire) |

## Requirements

- macOS 13 (Ventura) or later
- [ffmpeg](https://ffmpeg.org) on your `PATH` — used by the editor, thumbnails and playback (`brew install ffmpeg`)
- [Rust](https://rustup.rs) (stable) to build from source

## Getting started

```sh
git clone https://github.com/Kaillr/hesteclips.git
cd hesteclips
cargo run --release
```

On first launch macOS asks for **Screen & System Audio Recording** permission (and **Microphone** if you record one). Grant them in System Settings → Privacy & Security, then restart the app. When running from a terminal, the permission belongs to the terminal app.

## Usage

| Hotkey | Action |
|---|---|
| <kbd>Option</kbd> + <kbd>F8</kbd> | Start / stop the replay buffer |
| <kbd>Option</kbd> + <kbd>F9</kbd> | Start / stop recording |
| <kbd>Option</kbd> + <kbd>F10</kbd> | Save the replay buffer as a clip |

Hotkeys work while other apps (and games) are focused. On Windows and Linux use <kbd>Alt</kbd> instead of <kbd>Option</kbd>.

- **Sources** — set up your mic, desktop sound and apps, and adjust their levels.
- **Settings** — replay length, resolution, frame rate, quality, file format and where clips are saved (`~/Movies/hesteclips` by default).
- **Library** — click a clip to play it, hover for **Edit** and **Share**, right-click for more.

## Project structure

| Crate | Purpose |
|---|---|
| `crates/app` | The desktop app (egui): library, editor, sources, settings |
| `crates/capture` | Screen and audio capture, mixing, encoding and file writing |
| `crates/media` | Probing, preview decoding and rendering edits |
| `crates/hestefiles` | Optional cloud sharing to HesteFiles |
