cd /src
cargo build -q -p capture --example focus 2>&1 | grep -E "^error" -A6
S=/root/.local/share/Steam/steamapps; mkdir -p "$S/common/Counter-Strike Global Offensive/game/bin/linuxsteamrt64" "$S/common/osu!" /tmp/fakewine
printf '"AppState"\n{\n\t"appid"\t\t"730"\n\t"name"\t\t"Counter-Strike 2"\n\t"installdir"\t\t"Counter-Strike Global Offensive"\n}\n' > $S/appmanifest_730.acf
# Stand-in games: xterm (it takes focus, like a game) copied under each game's name.
cp /usr/bin/xterm "$S/common/Counter-Strike Global Offensive/game/bin/linuxsteamrt64/cs2"
touch "$S/common/osu!/osu!.exe"; cp /usr/bin/xterm /tmp/fakewine/wine64-preloader
Xorg :99 -config /xorg.conf -noreset -logfile /tmp/x.log >/dev/null 2>&1 & sleep 2
export DISPLAY=:99 HOME=/root XDG_SESSION_TYPE=x11; openbox >/dev/null 2>&1 & sleep 2
focus() { for i in $(seq 1 20); do W=$(wmctrl -l | awk -v t="$1" 'index($0,t){print $1; exit}'); [ -n "$W" ] && break; sleep 0.3; done; wmctrl -i -a $W; sleep 0.7; }
"$S/common/Counter-Strike Global Offensive/game/bin/linuxsteamrt64/cs2" -T cs2-window -e sleep 60 &
focus cs2-window; echo "== native Steam game focused:"; SECS=1 /target/debug/examples/focus | tail -1
(exec -a 'Z:\root\.local\share\Steam\steamapps\common\osu!\osu!.exe' /tmp/fakewine/wine64-preloader -T osu-window -e sleep 60) &
focus osu-window; echo "== Proton game focused:"; SECS=1 /target/debug/examples/focus | tail -1
xterm -T normal-app -e sleep 60 &
focus normal-app; echo "== normal app focused:"; SECS=1 /target/debug/examples/focus | tail -1
