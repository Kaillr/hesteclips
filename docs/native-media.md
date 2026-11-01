# Native media: where it stands

The branch `linux-native-nvenc` (not merged, not released) does in the app,
natively, what the bundled ffmpeg did, wherever that's faster. Performance
is the reason; dropping the bundled ffmpeg (most of the Linux download) is a
goal once nothing needs it, not a reason on its own.

Linux is done and measured. macOS and Windows already decoded playback on
the GPU in the app (VideoToolbox; D3D11) and recorded with their own
encoders, so most of Linux's gains they had. What they don't have yet:
exports made in the app (ffmpeg still makes the picture there), and the
shared changes on this branch haven't run on them. This note is what's known
so far; how to do it there is open.

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

- Linux's export: `crates/capture/src/linux/export.rs`, reached through
  `capture::export::render_video` (which returns `Ok(false)` elsewhere, so
  `media` has ffmpeg make the picture). The sound is added by `media`
  (`mux_audio`), on every platform.
- Which source frames make up an export (`Timeline`, in the Linux export)
  has nothing Linux in it: worth sharing rather than copying.
- Linux's decoding on the card and its check: `crates/capture/src/linux/decode.rs`
  (and `dmabuf.rs`); its encoder check: `linux/vkenc.rs` (`check`).
- What macOS has that may help: `capture::mac::decode` (VideoToolbox),
  `mac::gpu` (the Metal compositor), `mac::blend`, the recording's
  VideoToolbox encoder (`sck.rs`), `avwriter.rs`. Windows: `capture::win::decode`
  and `direct` (D3D11 decoding), `win::d3d` (the video processor),
  `win::h264` (Media Foundation's encoders), the blend pacer in `win/mod.rs`,
  and the MP4 writer Linux uses too (`mp4file`).

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

## Working on macOS and Windows at once

One branch each, off `linux-native-nvenc`, merged back when done. Most of the
work is in each platform's own modules; the places both may want to change
are the shared export entry (`crates/capture/src/export.rs`), the `media`
export, and this note: keep changes there small and in their own commits,
and settle who moves shared code (like `Timeline`) before both do. Nothing
goes into main or out as a release until the maintainer says so.

## Not tried yet

- AMD's and Intel's cards on Linux (VA-API decoding, Vulkan Video encoding):
  both check themselves before use and fall back, but have only run on
  NVIDIA's.
- Sound decoded in the app (waveforms and playback still start ffmpeg once a
  track: `media::pcm`), and other apps' videos (MKV, WebM, HEVC, AV1). Shared
  work, for whichever machine gets to it.
