# Native media: where it stands

The branch `linux-native-nvenc` (not merged, not released) does in the app,
natively, what the bundled ffmpeg did, wherever that's faster. Performance
is the reason; dropping the bundled ffmpeg (most of the Linux download) is a
goal once nothing needs it, not a reason on its own.

Linux is done and measured. macOS's exports are made in the app too, on
`mac-native-media` (off this branch, not merged), measured below. Windows
already decoded playback on the GPU in the app (D3D11) and recorded with its
own encoders, so most of Linux's gains it had; what it doesn't have yet is
exports made in the app (ffmpeg still makes the picture there), and the
shared changes on this branch haven't run on it. This note is what's known
so far.

## The bar: what Linux gained

Measured on a desktop with an i7-8700K and an RTX 4060 Ti, mostly 1440p
clips. CPU in % of one core.

| | Before | Now |
|---|---|---|
| Recording 1440p at 540 fps | ffmpeg: 221% CPU, ~234 fps | 19% CPU, ~425 fps (the encoder's own limit) |
| Playing a 1440p 540 fps clip | ~50 fps shown, 710% CPU | 540 fps, ~53% CPU |
| CPU per decoded 1440p frame | ~10 ms | ~0.5 ms (decoded on the card) |
| Fast scrubbing (20–28×) | 138–158 new frames/s, 4.7 GB | 247–291 frames/s, 1.0 GB |
| Export: 23.75 s cut from 1440p60, saved at 720p | ffmpeg: 8.5 s | 3.5 s |
| Export: 10 s of 540 fps blended to 60 | ffmpeg: 41 s (soft: 69 s) | 11 s (12 s) |
| Reading a clip's length, size, tracks | ffprobe: 46–73 ms | 0.02–0.09 ms |
| A clip's thumbnail | 105–175 ms | 28–43 ms |

The last two are shared code and run on every platform; the rest is Linux's.

## What macOS gained

Measured on a MacBook Pro with an M4 Pro (14 cores), 1440p clips made by ffmpeg
(`testsrc2`), release builds, against the ffmpeg the app ships (9.0.2).
CPU in % of one core.

| | ffmpeg | Now |
|---|---|---|
| Export: 23.75 s cut from 1440p60, saved at 720p | 3.5 s | 3.6 s |
| Export: a minute of 1440p60 saved at 1080p | 16.6 s, 197% CPU | 16.9 s, 42% |
| Export: a minute made to fit Discord (10 MB) | 3.9 s, 606% | 3.9 s, 66% |
| Export: 14 s of 540 fps blended to 60 | 14.8 s, 980% | 6.6 s, 53% |
| Export: 10 s of 540 fps blended to 60, soft | 15.0 s | 4.8 s |
| Reading a clip's length, size, tracks | ffprobe: 17–35 ms | 0.03–0.2 ms |
| A clip's thumbnail | 40 ms | 10 ms (the first of a run, 80) |

On the Mac ffmpeg already encoded on the media engine (`h264_videotoolbox`),
and that encoder is the limit: 440 to 500 megapixels a second here (136 fps
at 1440p, 236 at 1080p, 476 at 720p), however it's set (real time or not,
speed over quality, keyframes) and however many sessions share it (the M4
Pro has one encode engine). So an export that only scales, crops or drops
frames takes as long as ffmpeg's, within 3%, with a fifth to a tenth of the
CPU; blending, which ffmpeg did on the CPU, takes a third to a half of the
time. At the same bitrate the picture's a little better (35.5 dB against
35.2 at 4 Mbit/s): ffmpeg's encoder put a keyframe every 12 frames.

## What has to hold, however it's done

- **Exports are ffmpeg's, frame for frame.** The same source frame in each
  output frame, as many frames, every stream as long as the parts kept,
  colours tagged as the file says (BT.709, studio range). Speed is the point,
  not a different result.
- **Something that fails falls back.** A hardware path that won't start, or
  doesn't do what it should, gives way to ffmpeg or the CPU, and the export
  or recording still comes out right. Linux tries its newer hardware paths
  once before trusting them (a short clip decoded on the card, a few frames
  through Vulkan Video), because a driver can be wrong without saying so.
- **Faster, measured.** Before and after, time and CPU (`export_times`).
- **The UI thread stays free.** A hardware decoder's first start can take
  hundreds of milliseconds.

## Where things are

- Linux's export: `crates/capture/src/linux/export.rs`; macOS's:
  `crates/capture/src/mac/export.rs` (VideoToolbox decoding and encoding,
  Metal scaling, the recording's blending, our MP4 writer). Both reached
  through `capture::export::render_video` (which returns `Ok(false)` on
  Windows, so `media` has ffmpeg make the picture). The sound is added by
  `media` (`mux_audio`), on every platform.
- Shared, in `crates/capture/src/export.rs`: which source frames make up an
  export (`Timeline`) and the Lanczos weights exports scale by (`lanczos`).
  The colour matrix a clip states, for the decoders and `media`'s probe:
  its MP4's `colr` box, else its H.264 stream's own (`capture::h264`).
- Linux's decoding on the card and its check: `crates/capture/src/linux/decode.rs`
  (and `dmabuf.rs`); its encoder check: `linux/vkenc.rs` (`check`).
- What Windows has that may help: `capture::win::decode` and `direct` (D3D11
  decoding), `win::d3d` (the video processor), `win::h264` (Media
  Foundation's encoders), the blend pacer in `win/mod.rs`, and the MP4 writer
  Linux uses too (`mp4file`).

## Checking it

Make clips rather than using recordings of anyone's screen:
`ffmpeg -f lavfi -i testsrc2=s=2560x1440:r=60:d=60 -f lavfi -i sine=d=60 …`
for picture and sound, and `scripts/frame-numbers.py make clip.mp4` for one
whose frames carry their numbers (`scripts/frame-numbers.py read
export.mp4` says which frames an export kept).

| Test | What it checks |
|---|---|
| `media` `card_exports_match_ffmpegs` | Exports made in the app against ffmpeg's: frames, each frame, stream lengths (`HC_SRC`: a 60 fps clip with sound, `HC_FAST`: 540 fps; `--ignored`) |
| `media` `export_times` | Time and CPU of exports (same clips) |
| `media` `cut_out_of_the_middle`, `probing_mp4_matches_ffprobe` | Cuts, muted tracks; the in-app probe against ffprobe |
| `capture` `card_frames_match_the_cpus` and the other `card_` tests | Linux: decoding on the card is the CPU's, byte for byte |
| `capture` `frames_shown_are_ffmpegs`, `looks_are_ffmpegs`, `a_join_at_a_frames_middle` | Which source frames an export shows: ffmpeg's, as read from its exports |
| `capture` `scaled_as_linux_scales`, `frames_not_waited_for_are_each_their_own` | macOS: Metal's scaling is Linux's, sample for sample |

On macOS, compare with the ffmpeg the app ships: Homebrew's has no `zscale`,
so its blends mix pixel values, not light. A release build leaves it in
`target/release-package/dmg/HasteClip.app/Contents/MacOS`; put that first on
`PATH`. And time release builds: in a debug build VideoToolbox's first
decoder takes ~0.6 s to start (60 ms in release, 3 ms after).

Switches for comparing paths: `HASTECLIP_EXPORT_FFMPEG=1` (exports by
ffmpeg), `HASTECLIP_ENCODER`, `HASTECLIP_DECODE`, `HASTECLIP_CPU_DECODE=1`,
`HASTECLIP_FFPROBE=1`, `HASTECLIP_SCRUB_ENGINE=0`.

## Lessons from Linux

- Ending a file at its shortest stream (`-shortest`) cut the picture's last
  frames whenever a sound came out a few milliseconds short. Every stream is
  now made exactly as long as the parts.
- With B-frames, frames come out in another order than they go in: an
  encoder's output belongs to the frame it says, not the one just handed in,
  and the MP4 needs composition offsets and an edit list (`mp4mux::Spec::reorder`).
- Encoders work in whole macroblocks (1080 is coded as 1088): scale to the
  frame's size and crop, never stretch to the coded size.
- Scaling in RGB clipped colours the screen can't show and lost ~3 dB on
  detailed pictures against ffmpeg; Linux scales in YUV, as ffmpeg does.
- ffmpeg's comparisons (ssim, psnr) quietly convert when one file is tagged
  BT.709 and the other isn't: tag both, or compare raw planes.
- Decoders number frames differently (Media Foundation shifts every
  timestamp by the reordering delay); the app numbers them on its own
  frame-rate grid (`index_of` in the decoders).
- A card has one video decoder: a filmstrip or export decoding there starved
  playback of a 540 fps clip, so background decoding steps aside while
  playback is short of frames (`capture::playback_behind`).
- A driver can report work done before it is (NVIDIA's VA-API driver did);
  the checks above catch that kind of thing.
- Clips at fractional rates (59.94) are still exported by ffmpeg.

## Lessons from macOS

- ffmpeg's `fps` filter rounds each source frame's time to the nearest new
  frame's and keeps the last of each: a new frame shows the last source
  frame to start before its middle, not the one at its start. The two agree
  only at half the rate (60 to 30), which the checks had used; 144 to 60,
  120 to 30 and 540 to 60 showed other frames until the shared `Timeline`
  took the middle. That changed Linux's export too, which hasn't run since.
- A GPU waited on frame by frame sits idle: each frame took 1.5–2 ms for a
  fraction of that in work. The frames go in without waiting, and the
  encoder takes each a few behind (`AHEAD`), once it's made.
- ffmpeg's soft blend gets its very last frame wrong: the looks past the
  clip's end should see its last frame (as `blend_graph` says), but `mix`
  holds each branch's frame from a whole frame before. The app does as the
  comment says, so that one frame differs.
- The media engine decodes and encodes: decoding there may cost the
  encoder a little (exports only scaled were up to 3% slower than ffmpeg's,
  which decodes on the CPU; its first decoder of a run takes 60 ms too).
- Clips state their colours in their H.264 stream more often than in the
  MP4 (x264 and ffmpeg write them only there). The decoders read only the
  `colr` box, so a BT.601 clip from ffmpeg played as BT.709 on every
  platform, and its export came out labelled BT.709, its colours shifted;
  `media`'s probe (and ffprobe) read the stream. Now the decoders do too.
- BT.601 made BT.709 is a 3×3 on Y′CbCr (luma takes its block's colour,
  the colour's re-mixed), done as the frame's scaled down, rounded once. It
  came out within 0.5 of a level in RGB of the same picture recorded
  BT.709, and of ffmpeg's export (which keeps BT.601).
- ffmpeg's blending stamped every clip BT.709 before reading it into
  light: a BT.601 clip came out 8.5 levels off in RGB. It's read by its own
  matrix now.

## Working on macOS and Windows at once

One branch each, off `linux-native-nvenc`, merged back when done. Most of the
work is in each platform's own modules; the places both may want to change
are the shared export entry (`crates/capture/src/export.rs`), the `media`
export, and this note: keep changes there small and in their own commits,
and settle who moves shared code before both do. `Timeline` and `lanczos`
were moved there on `mac-native-media`: Windows starts from that, or takes
those changes. Nothing goes into main or out as a release until the
maintainer says so.

## Not tried yet

- AMD's and Intel's cards on Linux (VA-API decoding, Vulkan Video encoding):
  both check themselves before use and fall back, but have only run on
  NVIDIA's.
- Sound decoded in the app (waveforms and playback still start ffmpeg once a
  track: `media::pcm`), and other apps' videos (MKV, WebM, HEVC, AV1). Shared
  work, for whichever machine gets to it.
- Linux since `Timeline` and `lanczos` moved and `Timeline` took ffmpeg's
  frames: not built or run there yet (`card_exports_match_ffmpegs`, and
  `frame-numbers.py` on a 144 fps export at 60).
- On macOS, anything not H.264 in an MP4 is still made by ffmpeg. The
  thumbnail check (`first_frame_is_ffmpegs`) is 2.27 off ffmpeg's there
  against a bar of 2.0: VideoToolbox interpolates the colours, ffmpeg's
  default scaling doesn't (against its `full_chroma_int+accurate_rnd`, 1.44).
- A clip that doesn't say its colours: `media`'s probe guesses BT.601 below
  720 lines (as players do), the decoders take BT.709, and so do the exports
  made in the app (labelled so); ffmpeg's left them unlabelled. One rule for
  all would be better.
