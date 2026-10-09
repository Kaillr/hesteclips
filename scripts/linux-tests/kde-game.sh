cd /src
export XDG_RUNTIME_DIR=/tmp/rt HOME=/root; mkdir -p $XDG_RUNTIME_DIR; chmod 700 $XDG_RUNTIME_DIR
export DBUS_SESSION_BUS_ADDRESS=unix:path=$XDG_RUNTIME_DIR/bus; dbus-daemon --session --address=$DBUS_SESSION_BUS_ADDRESS --fork --nopidfile
cargo build -q -p capture --example focus 2>&1 | grep -E "^error" -A6
FOCUS=$(readlink -f /target/debug/examples/focus)
mkdir -p ~/.local/share/applications
printf '[Desktop Entry]\nType=Application\nName=hc-focus\nExec=%s\nX-KDE-Wayland-Interfaces=zkde_screencast_unstable_v1,org_kde_plasma_window_management\n' "$FOCUS" > ~/.local/share/applications/hc-focus.desktop
kbuildsycoca6 >/dev/null 2>&1
S=/root/.local/share/Steam/steamapps; mkdir -p "$S/common/Hades"; cp /usr/bin/foot "$S/common/Hades/Hades"
export XDG_CURRENT_DESKTOP=KDE QT_QPA_PLATFORM=offscreen KWIN_COMPOSE=Q
kwin_wayland --virtual --width 1280 --height 720 --socket wayland-hc >/tmp/kwin.log 2>&1 &
for i in $(seq 1 100); do [ -S $XDG_RUNTIME_DIR/wayland-hc ] && break; sleep 0.1; done
export WAYLAND_DISPLAY=wayland-hc; unset DISPLAY; sleep 2
"$S/common/Hades/Hades" sleep 60 >/dev/null 2>&1 & sleep 3
echo "== native Wayland Steam game in front (KDE):"; SECS=1 $FOCUS | tail -1
foot --app-id notes sleep 60 >/dev/null 2>&1 & sleep 3
echo "== normal app in front (KDE):"; SECS=1 $FOCUS | tail -1
