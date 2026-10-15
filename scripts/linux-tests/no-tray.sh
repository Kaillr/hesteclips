cd /src
export XDG_RUNTIME_DIR=/tmp/rt HOME=/root; mkdir -p $XDG_RUNTIME_DIR; chmod 700 $XDG_RUNTIME_DIR
export DBUS_SESSION_BUS_ADDRESS=unix:path=$XDG_RUNTIME_DIR/bus; dbus-daemon --session --address=$DBUS_SESSION_BUS_ADDRESS --fork --nopidfile
cargo build -q -p hasteclip 2>&1 | grep -E "^error" -A6
Xorg :99 -config /xorg.conf -noreset -logfile /tmp/x.log >/dev/null 2>&1 & sleep 2
export DISPLAY=:99 XDG_SESSION_TYPE=x11 HASTECLIP_LOG=1; openbox >/dev/null 2>&1 & sleep 1
echo "== started from autostart (--background) with no tray: must show its window"
/target/debug/hasteclip --background > /tmp/app.txt 2>&1 & APP=$!; sleep 6
echo "window shown: $(wmctrl -l | grep -c HasteClip)"; grep -i tray /tmp/app.txt
echo "== closing the window with no tray: must quit"
wmctrl -c HasteClip; for i in $(seq 1 30); do kill -0 $APP 2>/dev/null || break; sleep 0.5; done
kill -0 $APP 2>/dev/null && echo "STILL RUNNING (bad)" || echo "quit (good)"
