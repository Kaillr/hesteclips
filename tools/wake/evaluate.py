"""Score a wake-word model on held-out real and unseen-engine recordings.

predict(windows: (N, 16, 96) float32) -> (N,) probabilities.
"""
import glob, os, re, subprocess, wave
import numpy as np

HERE = os.path.dirname(os.path.abspath(__file__))
# Real recordings of the phrase (session16.wav, live16.wav, take1/2.wav,
# no1-3.wav), kept out of the repo: a folder of your own.
OLD = os.environ.get("HC_TAKES", os.path.join(HERE, "takes"))
REFRACTORY = 1.2  # s: one save per take (the app's QUIET_AFTER)

_feats = None
def features():
    global _feats
    if _feats is None:
        from openwakeword.utils import AudioFeatures
        _feats = AudioFeatures(os.path.join(HERE, "models", "melspectrogram.onnx"), os.path.join(HERE, "models", "embedding_model.onnx"), ncpu=4)
    return _feats

def read(path):
    with wave.open(path) as w:
        assert w.getframerate() == 16000 and w.getnchannels() == 1, path
        return np.frombuffer(w.readframes(w.getnframes()), np.int16)

STREAM = os.environ.get("STREAM") == "1"

def embed(audio, pad_before=2.0, pad_after=1.0):
    """Embeddings of a whole recording, with quiet room noise around it.
    STREAM=1: fed 80 ms at a time, as the app (and openWakeWord) does live."""
    rng = np.random.default_rng(0)
    pre = (rng.standard_normal(int(pad_before * 16000)) * 3).astype(np.int16)
    post = (rng.standard_normal(int(pad_after * 16000)) * 3).astype(np.int16)
    x = np.concatenate([pre, audio, post])
    if not STREAM:
        return features()._get_embeddings(x), pad_before
    from openwakeword.utils import AudioFeatures
    f = AudioFeatures(os.path.join(HERE, "models", "melspectrogram.onnx"), os.path.join(HERE, "models", "embedding_model.onnx"), ncpu=4)
    rows = []
    for c in range(0, len(x) - 1279, 1280):
        f._streaming_features(x[c:c + 1280])
        rows.append(f.feature_buffer[-1])
    # Row c ends at 0.08 (c + 1) s; whole-file row j ends at 0.08 j + 0.775 s.
    return np.array(rows[9:]), pad_before

def frame_end(j, pad):
    """Time (s, in the original recording) where the window ending at embedding j ends."""
    return (8 * j + 76) * 0.01 + 0.015 - pad

def scores(predict, emb):
    n = len(emb) - 15
    if n <= 0:
        return np.zeros(0)
    win = np.stack([emb[j:j + 16] for j in range(n)]).astype(np.float32)
    return predict(win)

NEED = 1  # frames in a row at or above the threshold before it fires

def fires(sc, thr, pad):
    out, last, run = [], -1e9, 0
    for j, s in enumerate(sc):
        t = frame_end(j + 15, pad)
        run = run + 1 if s >= thr else 0
        if run >= NEED and t - last >= REFRACTORY:
            out.append(t)
            last = t
    return out

def pieces(path):
    out = subprocess.run(["ffmpeg", "-hide_banner", "-i", path, "-af", "silencedetect=noise=-45dB:d=0.6", "-f", "null", "-"], capture_output=True, text=True).stderr
    starts = [float(x) for x in re.findall(r"silence_start: ([0-9.]+)", out)]
    ends = [float(x) for x in re.findall(r"silence_end: ([0-9.]+)", out)]
    return [(e, s) for e, s in zip(ends, starts[1:]) if s - e >= 0.4]

_cache = {}
def cached(path, **kw):
    if path not in _cache:
        _cache[path] = embed(read(path), **kw)
    return _cache[path]

def session(predict, thr, path, other=()):
    """Takes caught, takes, and false saves (on the `other` pieces, which are
    other speech, or anywhere outside a take)."""
    emb, pad = cached(path)
    f = fires(scores(predict, emb), thr, pad)
    ps = pieces(path)
    takes = [p for i, p in enumerate(ps) if i not in other]
    caught = sum(any(a <= t <= b + 1.0 for t in f) for a, b in takes)
    in_take = sum(any(a <= t <= b + 1.0 for a, b in takes) for t in f)
    return caught, len(takes), len(f) - in_take

def files(predict, thr, paths):
    hit = 0
    for p in paths:
        emb, pad = cached(p)
        hit += bool(fires(scores(predict, emb), thr, pad))
    return hit, len(paths)

_val = None
def per_hour(predict, thr, half=0):
    """False saves per hour of everyday audio: half 0 (for choosing settings)
    or half 1 (for reporting)."""
    global _val
    if _val is None:
        _val = np.load(os.path.join(HERE, "data", "validation_set_features.npy"), mmap_mode="r")
    n, f, last = 0, 0, -1e9
    step = 20000
    mid = len(_val) // 2
    lo, hi = (0, mid) if half == 0 else (mid, len(_val))
    for start in range(lo, hi - 16, step):
        chunk = np.asarray(_val[start:min(start + step + 15, hi)], dtype=np.float32)
        m = len(chunk) - 15
        win = np.lib.stride_tricks.sliding_window_view(chunk, (16, 96))[:m, 0]
        sc = predict(np.ascontiguousarray(win))
        above = sc >= thr
        # Runs of NEED frames in a row: frame k ends one.
        ok = above.copy()
        for d in range(1, NEED):
            ok[d:] &= above[:-d]
            ok[:d] = False
        for k in np.flatnonzero(ok):
            t = (start + k) * 0.08
            if t - last >= REFRACTORY:
                f += 1
                last = t
        n += m
    hours = n * 0.08 / 3600
    return f / hours

def report(predict, thr, val=True):
    s = session(predict, thr, os.path.join(OLD, "session16.wav"))
    # Pieces 2-4 of the live recording are other speech ("Vedta", "Ja!", laughing).
    l = session(predict, thr, os.path.join(OLD, "live16.wav"), other={2, 3, 4})
    t = files(predict, thr, [os.path.join(OLD, n) for n in ("take1.wav", "take2.wav")])
    ph = files(predict, thr, sorted(glob.glob(os.path.join(HERE, "test", "phrase_*.wav"))))
    near = files(predict, thr, sorted(glob.glob(os.path.join(HERE, "test", "near_*.wav"))) + [os.path.join(OLD, f"no{i}.wav") for i in (1, 2, 3)])
    line = (f"thr {thr:.2f}: session {s[0]}/{s[1]} (false {s[2]})  live {l[0]}/{l[1]} (false {l[2]})  takes {t[0]}/{t[1]}"
            f"  win-tts phrase {ph[0]}/{ph[1]}  near-miss fires {near[0]}/{near[1]}")
    if val:
        line += f"  false/hour {per_hour(predict, thr, 0):.2f} (tune) {per_hour(predict, thr, 1):.2f} (test)"
    return line
