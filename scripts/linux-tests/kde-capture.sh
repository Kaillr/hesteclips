cd /src
export XDG_RUNTIME_DIR=/tmp/rt HOME=/root; mkdir -p $XDG_RUNTIME_DIR; chmod 700 $XDG_RUNTIME_DIR
export DBUS_SESSION_BUS_ADDRESS=unix:path=$XDG_RUNTIME_DIR/bus; dbus-daemon --session --address=$DBUS_SESSION_BUS_ADDRESS --fork --nopidfile
pipewire >/tmp/pw.log 2>&1 &
sleep 0.5; wireplumber >/tmp/wp.log 2>&1 &
cargo build -q -p capture --example devices --example record 2>&1 | grep -E "^error" -A5
EXE=$(readlink -f /target/debug/examples/record)
DEV=$(readlink -f /target/debug/examples/devices)
mkdir -p ~/.local/share/applications
if [ "$GRANT" = 1 ]; then
  for x in record:$EXE devices:$DEV; do n=${x%%:*}; e=${x#*:}
  printf '[Desktop Entry]\nType=Application\nName=hc-%s\nExec=%s\nX-KDE-Wayland-Interfaces=zkde_screencast_unstable_v1\n' $n "$e" > ~/.local/share/applications/hc-$n.desktop; done
fi
export QT_LOGGING_RULES="kwin_core.debug=true;KWIN_UTILS.debug=true;kwin_screencast.debug=true" QT_FORCE_STDERR_LOGGING=1; XDG_MENU_PREFIX=plasma- kbuildsycoca6 >/dev/null 2>&1
export XDG_CURRENT_DESKTOP=KDE QT_QPA_PLATFORM=offscreen KWIN_COMPOSE=O2 LIBGL_ALWAYS_SOFTWARE=1
kwin_wayland --virtual --width 1280 --height 720 --output-count 2 --socket wayland-hc >/tmp/kwin.log 2>&1 &
for i in $(seq 1 100); do [ -S $XDG_RUNTIME_DIR/wayland-hc ] && break; sleep 0.1; done
export WAYLAND_DISPLAY=wayland-hc
sleep 2; echo "--- kwin log"; tail -5 /tmp/kwin.log
/target/debug/examples/devices 2>&1 | grep -A3 screens
for m in Virtual-0 Virtual-1; do
  SECS=3 HEIGHT=0 FPS=30 SOFTWARE=1 SCREEN=kde:$m /target/debug/examples/record 2>&1 | grep -viE "^MIX|pw\." | tail -3
  f=$(ls -t /tmp/hc/*.mp4 2>/dev/null | head -1); [ -n "$f" ] && ffprobe -v error -show_entries stream=width,height,nb_frames -of compact "$f"
done
grep -iE "screencast|X-KDE|not in|authoriz|desktop file|Interfaces found" /tmp/kwin.log | head -12; ls -la /tmp/rt
