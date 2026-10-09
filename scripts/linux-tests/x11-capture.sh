set -e
cd /src
Xorg :99 -config /xorg.conf -noreset -logfile /tmp/x.log >/dev/null 2>&1 &
sleep 2
export DISPLAY=:99 XDG_SESSION_TYPE=x11
xrandr --fb 1920x720
xrandr --delmonitor DUMMY0 >/dev/null 2>&1 || true
xrandr --setmonitor LEFT 1280/340x720/190+0+0 none
xrandr --setmonitor RIGHT 640/170x480/130+1280+0 none
xrandr --listmonitors
xclock -update 1 -geometry 400x400+100+100 &
xeyes -geometry 300x300+1400+50 &
sleep 1
cargo run -q -p capture --example devices 2>/dev/null | head -4
for m in LEFT RIGHT; do
  SECS=3 HEIGHT=0 FPS=30 SOFTWARE=1 SCREEN=x11:$m cargo run -q -p capture --example record 2>/dev/null | tail -1
  f=$(ls -t /tmp/hc/*.mp4 | head -1); ffprobe -v error -show_entries stream=width,height,nb_frames -of compact "$f"
  ffmpeg -v error -y -ss 1.5 -i "$f" -frames:v 1 /out/x11-$m.png
done
