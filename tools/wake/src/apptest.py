"""The app's own voice test (voice.rs, hears_the_phrase_and_nothing_else) in
Python, to check a model before building the app: the same sentences from
Windows' default voice (Windows PowerShell, System.Speech, 16 kHz), fed the
same way (2 s of quiet, the speech, 1 s of quiet), the same rule (a save
when the NEEDth frame in a row reaches THRESHOLD, not within QUIET_AFTER of
the last). With --voices, also
each of Windows' voices by name (PowerShell 7 sees the OneCore ones).

usage: python apptest.py <thr> <need> <model> [<model> ...] [--voices]
"""
import os, subprocess, sys, tempfile
import numpy as np
import soundfile as sf
import torch

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import feat
from choose import predictor

# voice.rs, in its order.
CASES = [
    ("hashtag hesteclip that", True),
    ("okay that was insane. hashtag heste clip that!", True),
    ("I love this game, let's go again", False),
    ("hashtag blessed, clip it later", False),
    ("has the clip that we made been saved yet", False),
    ("can you clip that for me", False),
    ("that was a nice clip", False),
    ("hashtag gaming", False),
    ("check the hashtag on twitter", False),
    ("stag party this weekend", False),
    ("he has to clip through the wall", False),
    ("hashtag", False),
    ("hashtag hesteclip", False),
    ("hesteclip that", False),
    ("hashtag clip that", False),
]
VOICES = ["Microsoft Zira Desktop", "Microsoft David Desktop", "Microsoft Mark"]
QUIET_AFTER = 1.2

def speak(text, path, voice=None):
    q = chr(39)
    pick = f"$s.SelectVoice({q}{voice}{q}); " if voice else ""
    script = ("Add-Type -AssemblyName System.Speech; "
              "$f = New-Object System.Speech.AudioFormat.SpeechAudioFormatInfo(16000, "
              "[System.Speech.AudioFormat.AudioBitsPerSample]::Sixteen, [System.Speech.AudioFormat.AudioChannel]::Mono); "
              f"$s = New-Object System.Speech.Synthesis.SpeechSynthesizer; {pick}"
              f"$s.SetOutputToWaveFile({q}{path}{q}, $f); $s.Speak({q}{text.replace(q, q + q)}{q}); $s.Dispose()")
    subprocess.run(["pwsh" if voice else "powershell", "-NoProfile", "-Command", script], check=True)
    a, sr = sf.read(path, dtype="int16")
    assert sr == 16000
    return a

def heard(predict, audio, thr, need):
    """Times the phrase is heard, as voice.rs heard_in reports them."""
    x = np.concatenate([np.zeros(32000, np.int16), audio, np.zeros(16000, np.int16)])
    rows = feat.stream_embed(x[: len(x) // 1280 * 1280])
    w = np.stack([rows[k - 15:k + 1] for k in range(15, len(rows))]).astype(np.float32)
    p = predict(torch.from_numpy(w).cuda()).cpu().numpy()
    sure, said, at = 0, None, []
    for i, pk in enumerate(p):
        t = feat.row_end(i + 15)
        sure = sure + 1 if pk >= thr else 0
        if sure != need or (said is not None and t - said < QUIET_AFTER):
            continue
        said = t
        at.append(round(t - 2.0, 2))
    return at, float(p.max())

def main(thr, need, models, voices):
    predict = predictor(models)
    tmp = tempfile.mkdtemp()
    wrong = 0
    for voice in [None] + (VOICES if voices else []):
        bad = []
        for i, (text, wanted) in enumerate(CASES):
            a = speak(text, os.path.join(tmp, f"{i}.wav"), voice)
            at, top = heard(predict, a, thr, need)
            if bool(at) != wanted:
                bad.append(f"{text!r} (wanted {wanted}, max p {top:.2f})")
        print(f"{voice or 'default voice'}: {len(CASES) - len(bad)}/{len(CASES)} right" + "".join("\n    wrong: " + b for b in bad), flush=True)
        wrong += len(bad) if voice is None else 0
    return wrong

if __name__ == "__main__":
    a = sys.argv[1:]
    voices = "--voices" in a
    a = [v for v in a if v != "--voices"]
    sys.exit(1 if main(float(a[0]), int(a[1]), a[2:], voices) else 0)
