cd /src
cargo build -q -p capture --example focus 2>&1 | grep -E "^error" -A6
export XDG_RUNTIME_DIR=/tmp/rt HOME=/root; mkdir -p $XDG_RUNTIME_DIR; chmod 700 $XDG_RUNTIME_DIR
export DBUS_SESSION_BUS_ADDRESS=unix:path=$XDG_RUNTIME_DIR/bus; dbus-daemon --session --address=$DBUS_SESSION_BUS_ADDRESS --fork --nopidfile
S=/root/.local/share/Steam/steamapps; mkdir -p "$S/common/Counter-Strike Global Offensive/game/bin/linuxsteamrt64" "$S/common/Hades"
printf '"AppState"\n{\n\t"appid"\t\t"730"\n\t"name"\t\t"Counter-Strike 2"\n\t"installdir"\t\t"Counter-Strike Global Offensive"\n}\n' > $S/appmanifest_730.acf
cp /usr/bin/xterm "$S/common/Counter-Strike Global Offensive/game/bin/linuxsteamrt64/cs2"
cp /usr/bin/foot "$S/common/Hades/Hades"
mkdir -p /tmp/.X11-unix; chmod 1777 /tmp/.X11-unix
export XDG_CURRENT_DESKTOP=GNOME XDG_SESSION_TYPE=wayland
mutter --wayland --headless --wayland-display=wayland-hc --virtual-monitor 1280x720 >/tmp/mutter.log 2>&1 &
for i in $(seq 1 100); do [ -S $XDG_RUNTIME_DIR/wayland-hc ] && break; sleep 0.1; done
export WAYLAND_DISPLAY=wayland-hc; sleep 2
for i in $(seq 1 30); do ls /tmp/.X11-unix/X* >/dev/null 2>&1 && break; sleep 0.2; done; export DISPLAY=:$(ls /tmp/.X11-unix/ | sed -n "s/^X//p" | head -1); export XAUTHORITY=$(ls $XDG_RUNTIME_DIR/.mutter-Xwaylandauth.* | head -1); echo "XWayland display: $DISPLAY"
echo "== XWayland Steam game in front (GNOME):"
"$S/common/Counter-Strike Global Offensive/game/bin/linuxsteamrt64/cs2" -e sleep 60 & sleep 3
SECS=1 /target/debug/examples/focus | tail -1
echo "== native Wayland game in front (GNOME, the fallback):"
pkill -f "linuxsteamrt64/cs2"; sleep 1
"$S/common/Hades/Hades" sleep 60 >/dev/null 2>&1 & sleep 3
SECS=1 /target/debug/examples/focus | tail -1
