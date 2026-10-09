cd /src
export XDG_RUNTIME_DIR=/tmp/rt HOME=/root; mkdir -p $XDG_RUNTIME_DIR; chmod 700 $XDG_RUNTIME_DIR
export DBUS_SESSION_BUS_ADDRESS=unix:path=$XDG_RUNTIME_DIR/bus; dbus-daemon --session --address=$DBUS_SESSION_BUS_ADDRESS --fork --nopidfile
cargo build -q -p hesteclips 2>&1 | grep -E "^error" -A6
export XDG_CURRENT_DESKTOP=KDE QT_QPA_PLATFORM=offscreen KWIN_COMPOSE=Q HESTECLIPS_LOG=1
kwin_wayland --virtual --width 1280 --height 720 --socket wayland-hc >/tmp/kwin.log 2>&1 &
for i in $(seq 1 100); do [ -S $XDG_RUNTIME_DIR/wayland-hc ] && break; sleep 0.1; done
export WAYLAND_DISPLAY=wayland-hc; unset DISPLAY; sleep 1
W() { busctl --user "$@" 2>&1; }
if [ "$TRAY" = 1 ]; then
  kded6 >/tmp/kded.log 2>&1 & sleep 3; busctl --user call org.kde.kded6 /kded org.kde.kded6 loadModule s statusnotifierwatcher >/dev/null; sleep 1
  echo "watcher on the bus: $(busctl --user list | grep -c StatusNotifierWatcher)"
fi
/target/debug/hesteclips > /tmp/app.txt 2>&1 &
APP=$!; sleep 6
grep -iE "tray" /tmp/app.txt
if [ "$TRAY" = 1 ]; then
  ITEMS=$(busctl --user get-property org.kde.StatusNotifierWatcher /StatusNotifierWatcher org.kde.StatusNotifierWatcher RegisteredStatusNotifierItems); echo "registered: $ITEMS"
  SVC=org.kde.StatusNotifierItem-$(echo "$ITEMS" | grep -oE 'StatusNotifierItem-[0-9-]+' | head -1 | sed 's/StatusNotifierItem-//')
  echo "title: $(busctl --user get-property $SVC /StatusNotifierItem org.kde.StatusNotifierItem Title)"
  echo "icon: $(busctl --user get-property $SVC /StatusNotifierItem org.kde.StatusNotifierItem IconPixmap | head -c 30)..."
  LAYOUT=$(busctl --user -- call $SVC /MenuBar com.canonical.dbusmenu GetLayout iias 0 -1 1 label)
  echo "menu: $(echo "$LAYOUT" | grep -oE '"label" s "[^"]*"' | sed 's/"label" s //' | tr '\n' ' ')"
  # Start the replay buffer from the menu: its label should change.
  BID=$(echo "$LAYOUT" | grep -oE '[0-9]+ 1 "label" s "Start replay buffer"' | awk '{print $1}')
  busctl --user -- call $SVC /MenuBar com.canonical.dbusmenu Event isvu $BID clicked s "" 0 >/dev/null; sleep 3
  grep -iE "buffer|capture|error|failed" /tmp/app.txt | grep -v "pw\.|PipeWire" | tail -4; echo "after Start: $(busctl --user -- call $SVC /MenuBar com.canonical.dbusmenu GetLayout iias 0 -1 1 label | grep -oE '"(Start|Stop) replay buffer"')"
  QID=$(echo "$LAYOUT" | grep -oE '[0-9]+ 1 "label" s "Quit HesteClips"' | awk '{print $1}')
  busctl --user -- call $SVC /MenuBar com.canonical.dbusmenu Event isvu $QID clicked s "" 0 >/dev/null
  for i in $(seq 1 30); do kill -0 $APP 2>/dev/null || break; sleep 0.5; done
  kill -0 $APP 2>/dev/null && echo "app STILL RUNNING after Quit" || echo "app quit from the tray menu"
fi
