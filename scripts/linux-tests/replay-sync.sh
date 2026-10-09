cd /src
export XDG_RUNTIME_DIR=/tmp/rt HOME=/root; mkdir -p $XDG_RUNTIME_DIR; chmod 700 $XDG_RUNTIME_DIR
export DBUS_SESSION_BUS_ADDRESS=unix:path=$XDG_RUNTIME_DIR/bus; dbus-daemon --session --address=$DBUS_SESSION_BUS_ADDRESS --fork --nopidfile
pipewire >/tmp/pw.log 2>&1 & sleep 0.5
wireplumber >/tmp/wp.log 2>&1 & sleep 0.5
pipewire-pulse >/tmp/pp.log 2>&1 & sleep 1.5
# A speaker (what apps play to) and a microphone, both virtual.
pactl load-module module-null-sink sink_name=speakers sink_properties=device.description=Speakers >/dev/null
# The mic: a sink to play the "voice" into, looped back out as a microphone.
pactl load-module module-null-sink sink_name=voicein sink_properties=device.description=VoiceIn >/dev/null
pactl load-module module-remap-source master=voicein.monitor source_name=micsrc source_properties=device.description=Microphone >/dev/null
pactl set-default-sink speakers; pactl set-default-source micsrc
sleep 1
echo "--- PipeWire devices:"; pactl list short sinks; pactl list short sources | grep -v monitor
cargo build -q -p capture --example record --example audio_devices 2>&1 | grep -E "^error" -A6
LIST=1 /target/debug/examples/record 2>&1 | grep -v "pw\.\|conf\.c" | head -12
# Beeps: a 440 Hz "game" tone and an 880 Hz "voice", 0.3 s each.
ffmpeg -v error -y -f lavfi -i "sine=frequency=440:duration=0.3" -ar 48000 -ac 2 /tmp/game.wav
ffmpeg -v error -y -f lavfi -i "sine=frequency=880:duration=0.3" -ar 48000 -ac 2 /tmp/voice.wav
Xorg :99 -config /xorg.conf -noreset -logfile /tmp/x.log >/dev/null 2>&1 & sleep 2
export DISPLAY=:99 XDG_SESSION_TYPE=x11
rm -rf /tmp/hc
REPLAY=1 SECS=6 HEIGHT=0 FPS=30 SOFTWARE=1 /target/debug/examples/record desktop mic:micsrc > /tmp/rec.txt 2>&1 &
REC=$!
# Wait for capture to start (first meter line), then play at known times.
for i in $(seq 1 50); do grep -q "dB" /tmp/rec.txt && break; sleep 0.1; done
T0=$(date +%s.%N)
# Flash the screen white and beep at the same instant, twice: picture and sound
# must land on the same moment in the file.
# One player, started before: silence until 2.0 s, a beep, silence until 4.0 s,
# a beep. Its playback is already running, so it doesn't add a start-up delay.
ffmpeg -v error -y -f lavfi -i "sine=frequency=440:duration=0.3" -f lavfi -i "anullsrc=r=48000:cl=stereo" -filter_complex "[1]atrim=0:1.7[s1];[0]aformat=channel_layouts=stereo,asplit[b1][b2];[1]atrim=0:1.7[s2];[s1][b1][s2][b2]concat=n=4:v=0:a=1" -ar 48000 -ac 2 /tmp/timed.wav
pw-play --target speakers /tmp/timed.wav &
sleep 1.7; xsetroot -solid white; sleep 0.3; xsetroot -solid black
sleep 1.7; xsetroot -solid white; sleep 0.3; xsetroot -solid black
wait $REC
echo "--- meters:"; grep -E "dB" /tmp/rec.txt | tail -6
grep -E "save [12]" /tmp/rec.txt; f=$(ls -t /tmp/hc/*.mp4 | grep -v recording | tail -1); echo "file: $f"
ffprobe -v error -show_entries stream=index,codec_type,codec_name,duration:stream_tags=title -of compact "$f"
# When does the picture turn white, and when does the game track get loud?
flashes=$(ffmpeg -v info -i "$f" -vf "signalstats,metadata=print:key=lavfi.signalstats.YAVG" -an -f null - 2>&1 | awk '/pts_time/{split($0,a,"pts_time:");t=a[2]+0} /YAVG/{split($0,b,"=");y=b[2]+0; if(y>128&&!on){printf "%.3fs ",t;on=1} if(y<64)on=0}')
beeps=$(ffmpeg -v info -i "$f" -map 0:a:1 -af silencedetect=noise=-35dB:d=0.1 -f null - 2>&1 | grep -oE "silence_end: [0-9.]+" | awk '{if($2<5.9)printf "%.3fs ", $2}')
echo "screen flashes at: $flashes"
echo "game beeps at:     $beeps"
