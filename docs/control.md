# Controlling HasteClip from other apps

Other apps on the same computer can control a running HasteClip. They can
save clips, start and stop clipping, recording and streaming, switch scenes
and presets, show and hide sources, and mute or turn sounds up and down.
HasteClip tells them what it's doing as it changes.

The [Stream Deck plugin](https://github.com/hasteclip/hasteclip-streamdeck)
works this way, and anything else can too (Touch Portal, Bitfocus Companion,
a script).

The code is `crates/app/src/control.rs`.

## Connecting

1. Open a TCP connection to `127.0.0.1:52131`. It's the port HasteClip
   already holds so that only one copy of it runs.
2. Write the four bytes `ctrl`.
3. From then on, each side writes one JSON object per line (UTF-8, ending in
   `\n`). Your first line is your hello, and it has to arrive within 5
   seconds:

   ```json
   {"hello":1,"client":"My controller"}
   ```

   `hello` is the protocol version you speak (1). `client` is optional: a
   name for your app.
4. HasteClip says hello back with its own version, then sends its state:

   ```json
   {"hello":1,"app":"HasteClip","version":"0.14.0"}
   {"state":{"clipping":true,"recording":false,...}}
   ```

Only this computer can connect. Only the HasteClip that's running answers: a
development test instance running beside it doesn't hold the port. A HasteClip
from before control closes the connection without a word, so if that happens,
the user needs to update.

## State

HasteClip sends `{"state":{...}}` right after its hello, and again, whole,
whenever any of it changes. While recording or live, that's every second,
because the time changes.

| Field | Type | What it is |
|---|---|---|
| `clipping` | bool | The replay buffer is running. |
| `recording` | bool | Recording. |
| `streaming` | bool | Live. |
| `recording_secs` | number | Whole seconds since the recording started. Only there while recording. |
| `stream_secs` | number | Whole seconds since going live. Only there while live. |
| `clip_secs` | number | How far back a clip reaches. |
| `can_screenshot` | bool | Screenshots work on this system. |
| `can_stream` | bool | Streaming works on this system. |
| `scene` | number | The id of the scene shown. |
| `scenes` | array | The preset's scenes in order: `{"id":1,"name":"Gameplay"}`. |
| `preset` | number | The id of the preset in use. |
| `presets` | array | Every preset in order: `{"id":1,"name":"Clipping"}`. |
| `sources` | array | The preset's video sources (below). |
| `sounds` | array | The preset's sounds (below). |

A video source:

| Field | Type | What it is |
|---|---|---|
| `id` | string | `screen`, `webcam:<key>` or `visual:<key>`. |
| `name` | string | Its name, as the Sources page shows it. |
| `kind` | string | `screen`, `apps` (games and apps), `webcam`, `display`, `window`, `image`, `colour`, `text`, `web` or `video`. |
| `in_scene` | bool | It's in the scene shown. |
| `on` | bool | It's shown in that scene (its eye is open). |

Sources come in the scene's order, front first, followed by the preset's
sources that aren't in the scene.

A sound:

| Field | Type | What it is |
|---|---|---|
| `id` | string | Its id. It stays the same when the sound is renamed. |
| `name` | string | Its name. |
| `kind` | string | `microphone`, `desktop`, `app` or `video`. |
| `on` | bool | Captured at all (its switch). |
| `muted` | bool | Muted. |
| `db` | number | Its volume, from −60 (silent) to +12. |

## Requests

Each request is one line, `{"do":"<what>", ...}`. To hear how it went, add an
`id` (any number). HasteClip then answers that line:

```json
{"reply":7,"ok":true}
{"reply":8,"ok":false,"why":"Clipping isn't on"}
```

`why` is a few words, meant for showing to someone. For a toggle, the new
state shows in the next `state`, so you don't need to wait for the answer to
see it.

| Request | What it does |
|---|---|
| `{"do":"save_clip"}` | Saves a clip while clipping. While recording, it also marks the moment (and only marks it when clipping is off), as the Save clip shortcut does. |
| `{"do":"screenshot"}` | Takes a screenshot. |
| `{"do":"mark"}` | Marks the moment in the recording. |
| `{"do":"clipping"}` | Starts or stops clipping. Add `"on":true` or `"on":false` to only start or only stop. |
| `{"do":"recording"}` | Starts or stops recording, with `on` as above. |
| `{"do":"stream"}` | Goes live or ends the stream, with `on` as above. Going live is asked first, in HasteClip's window, as it always is. Ending isn't. |
| `{"do":"scene","scene":3}` | Shows scene 3. `{"do":"scene","step":1}` shows the next one and `"step":-1` the one before, going round at the end. |
| `{"do":"preset","preset":2}` | Uses preset 2, as picking it on the Sources page does. While only clipping, HasteClip asks first in its window, because clipping starts again. While recording or live, it can't. |
| `{"do":"source","source":"webcam:4"}` | Shows or hides a video source in the scene shown, with `on` as above. |
| `{"do":"mute","sound":"src-…"}` | Mutes or unmutes a sound. Add `"muted":true` or `"muted":false` to only mute or only unmute. |
| `{"do":"volume","sound":"src-…","db":-6}` | Sets a sound's volume. `"by":2` turns it up 2 dB instead, and `"by":-2` turns it down. |
| `{"do":"open"}` | Brings HasteClip's window to the front. |

Saving a clip, taking a screenshot, marking the moment, muting and changing
the volume happen the moment the request arrives, the same way a shortcut is
handled. The rest happens in HasteClip's next frame, which can be later while
a game covers its window.

## Versions

This is version 1. New fields and requests may be added without a new
version, so ignore what you don't know. A request HasteClip doesn't know is
answered with `"ok":false` and `"why":"Not understood"`.

## Trying it

From Python:

```python
import json, socket

s = socket.create_connection(("127.0.0.1", 52131))
s.sendall(b'ctrl{"hello":1,"client":"example"}\n')
lines = s.makefile()
print(lines.readline())  # HasteClip's hello
print(lines.readline())  # its state
s.sendall(b'{"id":1,"do":"save_clip"}\n')
for line in lines:
    message = json.loads(line)
    if message.get("reply") == 1:
        print(message)
        break
```
