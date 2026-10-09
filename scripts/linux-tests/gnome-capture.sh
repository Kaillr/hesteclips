cd /src
export XDG_RUNTIME_DIR=/tmp/rt HOME=/root; mkdir -p $XDG_RUNTIME_DIR; chmod 700 $XDG_RUNTIME_DIR
export DBUS_SESSION_BUS_ADDRESS=unix:path=$XDG_RUNTIME_DIR/bus
dbus-daemon --session --address=$DBUS_SESSION_BUS_ADDRESS --fork --nopidfile
pipewire >/tmp/pw.log 2>&1 &
sleep 0.5; wireplumber >/tmp/wp.log 2>&1 &
cargo build -q -p capture --example devices --example record 2>&1 | grep -E "^error" -A5
export XDG_CURRENT_DESKTOP=GNOME XDG_SESSION_TYPE=wayland
mutter --wayland --headless --no-x11 --wayland-display=wayland-hc --virtual-monitor 1280x720 --virtual-monitor 800x600 >/tmp/mutter.log 2>&1 &
for i in $(seq 1 100); do [ -S $XDG_RUNTIME_DIR/wayland-hc ] && break; sleep 0.1; done
export WAYLAND_DISPLAY=wayland-hc
sleep 2
/target/debug/examples/devices 2>&1 | grep -A3 screens
for m in $(/target/debug/examples/devices 2>/dev/null | grep -o "\[gnome:[^]]*\]" | tr -d '[]'); do
  SECS=3 HEIGHT=0 FPS=30 SOFTWARE=1 SCREEN=$m /target/debug/examples/record 2>&1 | grep -viE "^MIX|pw\.|conf\.c" | tail -3
  f=$(ls -t /tmp/hc/*.mp4 2>/dev/null | head -1); [ -n "$f" ] && ffprobe -v error -show_entries stream=width,height,nb_frames -of compact "$f" && ffmpeg -v error -y -ss 1.5 -i "$f" -frames:v 1 -vf scale=320:-1 /out/gnome-${m#gnome:}.png
done
echo "--- mutter:"; grep -iE "error|screencast|fail" /tmp/mutter.log | head -5
