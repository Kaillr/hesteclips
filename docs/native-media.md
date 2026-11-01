# Native media: where it stands, what's left

Work on the branch `linux-native-nvenc` (not merged, not released). The goal:
do in the app, natively, what the bundled ffmpeg did, wherever that's faster.
Performance is the reason, not removing ffmpeg for its own sake; dropping
the bundled ffmpeg (most of the Linux download) is a goal once nothing needs
it. The Linux side is done and measured; macOS and Windows are next, on
those machines.

## What changed, by platform

Everything below that isn't marked Linux runs on all three, and has only
been run on Linux so far.

| Commit | Change | Platforms |
|---|---|---|
| 1314159, c997519, 88d4073 | Clips probed in the app from the MP4's index (`media::mp4`), not by ffprobe; lengths as ffprobe has them (edit lists); broken, crash-saved (fragmented: ffprobe then) and turned files handled | all |
| 8bee2b8, 7243a4d | A clip's thumbnail decoded in the app by the decoder playback uses, on low-priority threads | all |
| 851bb84, 2e387d7 | Exports that make the picture again (size, rate, crop, blending) made on the graphics card | Linux; elsewhere ffmpeg, as before |
| 743784f | An export with every track muted keeps its last frames (each sound padded or trimmed to the parts; no `-shortest`) | all |
| 9860c7f | The scrub engine opens off the UI thread (opening the first clip held the UI ~290 ms on Linux) | all |
| 643bd4c, 33bf2ed, 96f1fdf | Recording: NVIDIA's encoder driven directly; AMD's and Intel's through Vulkan Video, checked once a run before use | Linux |
| 64183b0, 7a34184, ef6fa6e | Webcams read in the app (V4L2, libjpeg-turbo), drawn into their box on the card | Linux |
| e50afd9, a10f8d8, b32f95c | Playback, scrubbing, the filmstrip and exports decoded on the card: NVIDIA's (CUDA), AMD's and Intel's (VA-API), checked once a run | Linux |

## macOS

1. Build and run the tests: `cargo test -p media`, `cargo test -p capture`,
   `cargo test -p hasteclip`. Then the ones that need clips or ffmpeg's
   tools, below.
2. Check the changes that run there:
   - `cargo test -p media probing_mp4_matches_ffprobe`: the in-app probe
     against ffprobe, on generated files. Then on real recordings and other
     apps' videos: the app's clip list against `ffprobe` (length, size, rate,
     tracks).
   - Thumbnails come from VideoToolbox (`capture::mac::decode`), on threads
     at the utility QoS class (`video::lower_priority`).
   - `cargo test -p media cut_out_of_the_middle`: exports with cuts, every
     track muted too, as long as their parts.
   - Scrubbing a clip right after opening it (the engine joins a moment
     later; until then the playback decoder answers).
3. Then exports on the GPU, as Linux has them: `capture::export::render_video`
   returns `Ok(false)` on macOS, so ffmpeg makes the picture. The Linux one
   (`crates/capture/src/linux/export.rs`) is the model: frames decoded where
   playback decodes them (VideoToolbox, frames in IOSurfaces), cropped and
   scaled in YUV on the GPU (Lanczos, `Nv12Scaler`), blended by the
   recording's frame code, encoded by the recording's encoder, written by our
   MP4 writer (`writer::Writer::record`). `media` adds the sound (ffmpeg,
   for now). It has to match ffmpeg's exports frame for frame:
   `card_exports_match_ffmpegs` (below) and `scripts/frame-numbers.py`.

## Windows

The same steps, with Windows' parts: thumbnails from the D3D11 decoder
(`capture::win::decode`), below-normal thread priority; exports through
Media Foundation's encoders (`capture::win::h264`: NVENC, AMF or Quick Sync
behind their MFTs) from D3D11 textures. `cargo check -p hasteclip --target
x86_64-pc-windows-gnu` type-checks it on Linux; building and running needs
Windows.

## Testing

Clips: make them, don't use personal recordings. `ffmpeg -f lavfi -i
testsrc2=s=2560x1440:r=60:d=60 -f lavfi -i sine=d=60 …` for picture and
sound; `scripts/frame-numbers.py make clip.mp4` for one whose frames carry
their numbers, and `scripts/frame-numbers.py read export.mp4` to see which
frames an export kept.

| Test | What it checks | Needs |
|---|---|---|
| `media` `card_exports_match_ffmpegs` | Exports on the card against ffmpeg's: frames, each frame, stream lengths | `HC_SRC` (60 fps clip with sound), `HC_FAST` (540 fps), `--ignored --nocapture` |
| `media` `export_times` | How long exports take, CPU used | the same |
| `capture` `card_frames_match_the_cpus`, `small_card_pictures_match_the_cpus`, `card_through_a_clip` | Decoding on the card is the CPU's, byte for byte | Linux, a card that decodes |
| `capture` `vulkan_video_encodes_what_it_is_given`, `encodes_what_it_is_given` (NVENC) | The encoders | Linux |
| `hasteclip` `card_frame_matches_ffmpeg` | A card frame drawn by the real filter matches ffmpeg | Linux |

Switches for trying one path against another: `HASTECLIP_EXPORT_FFMPEG=1`
(exports by ffmpeg), `HASTECLIP_ENCODER=nvenc|vulkan|ffmpeg`,
`HASTECLIP_DECODE=nvidia|vaapi`, `HASTECLIP_CPU_DECODE=1`,
`HASTECLIP_FFPROBE=1`, `HASTECLIP_SCRUB_ENGINE=0`.

Vulkan validation (Linux): unpack the distribution's
`vulkan-validation-layers` package somewhere, point the manifest's
`library_path` at its `.so`, and run with `VK_ADD_LAYER_PATH=<its dir>`
`VK_INSTANCE_LAYERS=VK_LAYER_KHRONOS_validation`. The decode paths, the
capture GPU tests, NVENC and Vulkan Video report nothing (1.4.363).

## Not tried yet

- AMD's and Intel's cards on Linux (VA-API decoding, Vulkan Video encoding):
  both check themselves before they're used and fall back if they fail, but
  have only run on NVIDIA's (VA-API through nvidia-vaapi-driver, which hands
  frames over before its copy of them is done: 2 ms grace there, and only
  when asked for with `HASTECLIP_DECODE=vaapi`).
- Sound decoded in the app (waveforms, playback: `media::pcm` still runs
  ffmpeg once a track), and other apps' videos (MKV, WebM, HEVC, AV1).
