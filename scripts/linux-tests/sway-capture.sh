set -e
cd /src
export XDG_RUNTIME_DIR=/tmp/rt; mkdir -p $XDG_RUNTIME_DIR; chmod 700 $XDG_RUNTIME_DIR
export WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 WLR_RENDERER=pixman WLR_HEADLESS_OUTPUTS=2
cat > /tmp/sway.conf <<'C'
output HEADLESS-1 mode 1280x720 position 0 0 bg #3366cc solid_color
output HEADLESS-2 mode 800x600 position 1280 0 bg #cc3333 solid_color
C
sway -c /tmp/sway.conf >/tmp/sway.log 2>&1 &
for i in $(seq 1 50); do ls $XDG_RUNTIME_DIR/wayland-* >/dev/null 2>&1 && break; sleep 0.1; done
export WAYLAND_DISPLAY=$(basename $(ls $XDG_RUNTIME_DIR/wayland-* | grep -v lock | head -1))
unset DISPLAY
sleep 1
SWAYSOCK=$(ls $XDG_RUNTIME_DIR/sway-ipc.* | head -1) swaymsg -t get_outputs | grep -E "name|current_mode" -A0 | head; 
cargo run -q -p capture --example devices 2>/dev/null | head -4
for m in HEADLESS-1 HEADLESS-2; do
  SECS=3 HEIGHT=0 FPS=30 SOFTWARE=1 SCREEN=wl:$m cargo run -q -p capture --example record 2>&1 | grep -v "pw\.\|MIX" | tail -2
  f=$(ls -t /tmp/hc/*.mp4 | head -1); ffprobe -v error -show_entries stream=width,height,nb_frames -of compact "$f"
  ffmpeg -v error -y -ss 1.5 -i "$f" -frames:v 1 -vf scale=320:-1 /out/sway-$m.png
done
