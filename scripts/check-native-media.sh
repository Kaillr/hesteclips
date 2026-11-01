#!/usr/bin/env bash
# Checks the exports made in the app against ffmpeg's, on this machine's
# graphics card: what docs/native-media.md asks of every platform.
#
#   scripts/check-native-media.sh [dir]
#
# Makes its own clips in `dir` (target/native-media-check; kept for the next
# run), then, with release builds:
#   1. the unit tests of `capture` and `media`
#   2. `card_exports_match_ffmpegs`: cuts at 720p, a crop at 30 fps, 540 fps
#      blended even and soft, each against ffmpeg's export
#   3. a 144 fps clip saved at 60: the frames ffmpeg's export shows
#      (`frame-numbers.py`)
#   4. a BT.601 clip: its colours as ffmpeg's export shows them
#   5. `export_times`, in the app and by ffmpeg, for the note's tables
# and says what passed. The log lines starting "export:" say which way each
# export went (decoder, encoder, or why ffmpeg made it). Needs ffmpeg,
# ffprobe and Python 3 on PATH. Works in Git Bash on Windows too.

set -u
cd "$(dirname "$0")/.."
dir=${1:-target/native-media-check}
mkdir -p "$dir"
py=$(command -v python3 || command -v python)
log="$dir/check.log"
: > "$log"
results=()
pass() { results+=("PASS  $1"); }
fail() { results+=("FAIL  $1"); }
say() { echo; echo "== $*" | tee -a "$log"; }

# The clips: NVIDIA's encoder where ffmpeg has a working one (quick), else x264.
if ffmpeg -v error -f lavfi -i testsrc2=s=256x144:d=0.2 -c:v h264_nvenc -f null - 2>/dev/null; then
  enc=(-c:v h264_nvenc -preset p4 -bf 0)
else
  enc=(-c:v libx264 -preset veryfast -bf 0)
fi
tags=(-color_range tv -colorspace bt709 -color_primaries bt709 -color_trc bt709)
clip() {
  local out=$1; shift
  [ -s "$dir/$out" ] && return
  say "making $out"
  ffmpeg -hide_banner -loglevel error -y "$@" "$dir/$out" || { rm -f "$dir/$out"; echo "couldn't make $out"; exit 1; }
}
clip src60.mp4 -f lavfi -i testsrc2=s=2560x1440:r=60:d=60 -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=60" \
  "${enc[@]}" -b:v 20M -g 120 -pix_fmt yuv420p "${tags[@]}" -c:a aac -ac 2 -b:a 160k
clip fast540.mp4 -f lavfi -i testsrc2=s=2560x1440:r=540:d=12 -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=12" \
  "${enc[@]}" -b:v 50M -g 1080 -pix_fmt yuv420p "${tags[@]}" -c:a aac -ac 2 -b:a 160k
# BT.601, said only in the H.264 stream (as x264 writes it).
clip bt601.mp4 -f lavfi -i testsrc2=s=1280x720:r=30:d=6 -f lavfi -i sine=d=6 \
  -c:v libx264 -preset fast -crf 16 -bf 0 -pix_fmt yuv420p -color_range tv -colorspace smpte170m -color_primaries smpte170m -color_trc smpte170m -c:a aac
if [ ! -s "$dir/numbers144.mp4" ]; then
  say "making numbers144.mp4"
  "$py" scripts/frame-numbers.py make "$dir/numbers144.mp4" --size 1920x1080 --fps 144 --seconds 12 --gop 288 || exit 1
fi

say "building"
cargo build --release -q -p media --example render_output 2>&1 | tee -a "$log" | grep -E "^(error|warning)" -A5
render=target/release/examples/render_output
[ -x "$render" ] || [ -x "$render.exe" ] || { echo "the build failed: see $log"; exit 1; }

say "1. unit tests"
for crate in capture media; do
  if cargo test --release -q -p "$crate" >> "$log" 2>&1; then pass "unit tests: $crate"; else fail "unit tests: $crate (see $log)"; fi
done
if [ "$(uname -s)" = Linux ]; then
  say "1b. Linux: frames decoded on the card are the CPU's"
  if HC_CLIP="$dir/src60.mp4" cargo test --release -q -p capture -- --ignored card_frames small_card card_through_a_clip >> "$log" 2>&1; then
    pass "Linux decoding on the card"
  else
    fail "Linux decoding on the card (see $log)"
  fi
fi

say "2. exports in the app against ffmpeg's"
if HC_SRC="$dir/src60.mp4" HC_FAST="$dir/fast540.mp4" cargo test --release -q -p media card_exports_match_ffmpegs -- --ignored --nocapture --test-threads=1 2>&1 \
  | tee -a "$log" | grep -E "^(cuts|crop|blend)|^export:|panicked"; [ "${PIPESTATUS[0]}" -eq 0 ]; then
  pass "card_exports_match_ffmpegs"
else
  fail "card_exports_match_ffmpegs"
fi

say "3. 144 fps saved at 60: ffmpeg's frames"
START=1 END=9 "$render" "$dir/numbers144.mp4" "$dir/n144-app.mp4" - 60 2>&1 | tee -a "$log" | grep "^export:"
START=1 END=9 HASTECLIP_EXPORT_FFMPEG=1 "$render" "$dir/numbers144.mp4" "$dir/n144-ffmpeg.mp4" - 60 >> "$log" 2>&1
app=$("$py" scripts/frame-numbers.py read "$dir/n144-app.mp4")
theirs=$("$py" scripts/frame-numbers.py read "$dir/n144-ffmpeg.mp4")
echo "app:    ${app:0:120}…"
echo "ffmpeg: ${theirs:0:120}…"
if [ -n "$app" ] && [ "$app" = "$theirs" ]; then pass "144 to 60: the same frames"; else fail "144 to 60: other frames"; fi

say "4. a BT.601 clip's colours"
"$render" "$dir/bt601.mp4" "$dir/bt601-app.mp4" 360 - 4000 2>&1 | tee -a "$log" | grep "^export:"
HASTECLIP_EXPORT_FFMPEG=1 "$render" "$dir/bt601.mp4" "$dir/bt601-ffmpeg.mp4" 360 - 4000 >> "$log" 2>&1
# Each decoded by what it says it is: right, ~40 dB apart; a BT.601 picture
# taken for BT.709 (its magenta pink), ~24.
for side in app ffmpeg; do ffmpeg -v error -y -i "$dir/bt601-$side.mp4" -f rawvideo -pix_fmt rgb24 "$dir/bt601-$side.rgb"; done
raw=(-f rawvideo -pix_fmt rgb24 -s 640x360)
db=$(ffmpeg -hide_banner "${raw[@]}" -i "$dir/bt601-app.rgb" "${raw[@]}" -i "$dir/bt601-ffmpeg.rgb" -lavfi psnr -f null - 2>&1 | grep -o "average:[0-9.inf]*" | cut -d: -f2)
rm -f "$dir"/bt601-*.rgb
echo "RGB against ffmpeg's export: ${db:-?} dB"
if [ -n "$db" ] && "$py" -c "import sys; sys.exit(0 if float('$db'.replace('inf', '99')) >= 33 else 1)"; then pass "BT.601 colours ($db dB)"; else fail "BT.601 colours (${db:-?} dB, under 33)"; fi

say "5. export times (in the app, then by ffmpeg)"
for way in app ffmpeg; do
  echo "-- $way"
  if [ $way = ffmpeg ]; then export HASTECLIP_EXPORT_FFMPEG=1; else unset HASTECLIP_EXPORT_FFMPEG; fi
  HC_SRC="$dir/src60.mp4" HC_FAST="$dir/fast540.mp4" cargo test --release -q -p media export_times -- --ignored --nocapture --test-threads=1 2>&1 \
    | tee -a "$log" | grep -E "^(trim|1080p|discord|blend)"
done
unset HASTECLIP_EXPORT_FFMPEG

echo
echo "== results ($(uname -s), log in $log)"
printf '%s\n' "${results[@]}"
printf '%s\n' "${results[@]}" | grep -q "^FAIL" && exit 1
exit 0
