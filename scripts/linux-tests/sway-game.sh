cd /src
cargo build -q -p capture --example focus 2>&1 | grep -E "^error" -A6
S=/root/.local/share/Steam/steamapps; mkdir -p "$S/common/Counter-Strike Global Offensive/game/bin/linuxsteamrt64"
printf '"AppState"\n{\n\t"appid"\t\t"730"\n\t"name"\t\t"Counter-Strike 2"\n\t"installdir"\t\t"Counter-Strike Global Offensive"\n}\n' > $S/appmanifest_730.acf
# A native Wayland "game": foot (a Wayland terminal) copied as cs2, with app id cs2.
cp /usr/bin/foot "$S/common/Counter-Strike Global Offensive/game/bin/linuxsteamrt64/cs2"
export XDG_RUNTIME_DIR=/tmp/rt HOME=/root; mkdir -p $XDG_RUNTIME_DIR; chmod 700 $XDG_RUNTIME_DIR
export WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 WLR_RENDERER=pixman WLR_HEADLESS_OUTPUTS=1
printf 'output HEADLESS-1 mode 1280x720\n' > /tmp/sway.conf
sway -c /tmp/sway.conf >/tmp/sway.log 2>&1 &
for i in $(seq 1 50); do ls $XDG_RUNTIME_DIR/wayland-* >/dev/null 2>&1 && break; sleep 0.1; done
export WAYLAND_DISPLAY=$(basename $(ls $XDG_RUNTIME_DIR/wayland-* | grep -v lock | head -1)); unset DISPLAY
export SWAYSOCK=$(ls $XDG_RUNTIME_DIR/sway-ipc.* | head -1)
sleep 1
"$S/common/Counter-Strike Global Offensive/game/bin/linuxsteamrt64/cs2" --app-id cs2 sleep 60 >/dev/null 2>&1 &
sleep 1.5; swaymsg '[app_id="cs2"] focus' >/dev/null
echo "== native Wayland Steam game focused:"; SECS=1 /target/debug/examples/focus | tail -1
foot --app-id notes sleep 60 >/dev/null 2>&1 &
sleep 1.5; swaymsg '[app_id="notes"] focus' >/dev/null
echo "== normal Wayland app focused:"; SECS=1 /target/debug/examples/focus | tail -1
