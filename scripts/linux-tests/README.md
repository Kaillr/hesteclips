# Linux tests

Real Linux desktops in containers, so the Linux app can be tested from any
machine with Docker (a Mac too):

```
scripts/linux-tests/run.sh                         # all of them
scripts/linux-tests/run.sh sound-sync x11-capture  # just these
```

Each test prints what it found; read the output. Pictures from the capture
tests go to `target/linux-tests/`.

| Test | Checks |
|---|---|
| `x11-capture` | Records each of two X11 monitors (no sharing dialog) |
| `sway-capture` | Records each of two Sway monitors |
| `gnome-capture` | Records each of two GNOME monitors |
| `kde-capture` | KDE lets the app stream the screen (the stream itself needs a GPU) |
| `sound-sync` | Desktop sound and a mic on their own tracks; a beep lands on the same frame as a screen flash |
| `replay-sync` | The same in clips saved from the replay buffer |
| `x11-game`, `sway-game`, `gnome-game`, `kde-game` | The game in front is known: a native Steam game, a Proton `.exe`, a normal app |
| `kde-tray` | The tray icon, its menu, quitting from it |
| `no-tray` | Without a tray, the window opens from autostart and closing quits |
| `update` | Updating 1.0.0 → 1.1.0 from a local "GitHub", on quit and after a crash |

The machines: `ubuntu.Dockerfile` (X11 with a dummy driver for several
monitors, Sway, PipeWire), `gnome.Dockerfile` and `kde.Dockerfile` (Fedora).

**What can't be tested here:** anything needing a GPU — KDE's screen stream,
NVENC and VA-API encoding, decoding on the GPU. Those need a real Linux PC.

The release package itself (`scripts/release-linux.sh`) was also run on fresh
Ubuntu 24.04, Fedora 42, Debian 13 and Arch (x86-64 under emulation): it needs
glibc 2.39+ plus the libraries every desktop has (D-Bus, PipeWire, ALSA,
libXcursor/libXrandr/libXi and libxkbcommon-x11 on X11, Vulkan or OpenGL).
