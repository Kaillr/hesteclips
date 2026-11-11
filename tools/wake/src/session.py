"""A session recording to check the app agrees with Python: real takes and
"ignore" clips one after another with gaps of their own noise, written as
a 16 kHz WAV, and the probabilities Python gives it every 80 ms.

usage: python session.py <model.onnx> <takes.npz> <clips.npz> <out.wav> [n_takes n_clips]
then:  HESTECLIPS_VOICE_TRACE=1 HESTECLIPS_VOICE_SESSION=<out.wav>
       cargo test --release counts_a_session -- --ignored --nocapture
and compare its "voice: t=... p=..." lines with <out.wav>.probs.txt
"""
import os, sys
import numpy as np
import soundfile as sf

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import feat
from gen import load

def main(model, takes, clips, out, n_takes=12, n_clips=6):
    rng = np.random.default_rng(0)
    t, _ = load(takes)
    c, _ = load(clips)
    parts = [np.zeros(32000, np.int16)]
    marks = []
    for kind, pool, n in (("take", t, n_takes), ("clip", c, n_clips)):
        for i in rng.choice(len(pool), min(n, len(pool)), replace=False):
            x = pool[i]
            fr = x[: len(x) // 400 * 400].astype(np.float32).reshape(-1, 400)
            floor = np.percentile(np.sqrt((fr ** 2).mean(1)), 10)
            gap = (rng.standard_normal(32000) * floor).astype(np.int16)
            marks.append((kind, sum(len(p) for p in parts) / 16000 + 2.0, len(x) / 16000))
            parts += [gap, x]
    parts.append(np.zeros(16000, np.int16))
    audio = np.concatenate(parts)
    sf.write(out, audio, 16000, subtype="PCM_16")
    import onnxruntime as ort
    s = ort.InferenceSession(model, providers=["CPUExecutionProvider"])
    # As the app hears it (voice.rs: 2 s of silence first, then 100 ms
    # chunks; features every 80 ms).
    x = np.concatenate([np.zeros(32000, np.int16), audio])
    rows = feat.stream_embed(x[: len(x) // 1280 * 1280], device="cpu")
    with open(out + ".probs.txt", "w") as f:
        for k in range(15, len(rows)):
            logit = s.run(None, {s.get_inputs()[0].name: rows[k - 15:k + 1][None].astype(np.float32)})[0].reshape(-1)[0]
            f.write(f"{feat.row_end(k):.2f} {1 / (1 + np.exp(-logit)):.4f}\n")
    with open(out + ".marks.txt", "w") as f:
        for kind, at, secs in marks:
            f.write(f"{kind} {at:.2f} {secs:.2f}\n")
    print(f"{out}: {len(audio) / 16000:.0f} s, {len(marks)} pieces")

if __name__ == "__main__":
    a = sys.argv[1:]
    main(a[0], a[1], a[2], a[3], *(int(v) for v in a[4:6]))
