"""Make clean audio sound like a gamer's microphone: room, mic colour,
background (noise, music, other people talking), level, and for synthetic
voices also other pitches and vocal tracts. All sources licence-clean.

Pools are cached as one int16 .npy each (data/pool_*.npy) on first use.
"""
import glob, os, random
import numpy as np
import soundfile as sf
from scipy.signal import butter, fftconvolve, resample_poly, sosfilt

HERE = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DL = os.path.join(HERE, "dl")
SR = 16000

def _read(f):
    a, sr = sf.read(f, dtype="float32", always_2d=True)
    a = a[:, 0]
    if sr != SR:
        from math import gcd
        g = gcd(SR, sr)
        a = resample_poly(a, SR // g, sr // g).astype(np.float32)
    return a

def _pool(name, files, max_secs=None, cut=None):
    """Concatenated int16 audio + offsets, cached."""
    path = os.path.join(HERE, "data", f"pool_{name}.npz")
    flat = os.path.join(HERE, "data", f"pool_{name}_audio.npy")
    if os.path.exists(flat):  # memory-mapped: worker processes share it
        audio = np.load(flat, mmap_mode="r")
        lens = np.load(os.path.join(HERE, "data", f"pool_{name}_lens.npy"))
        ends = np.cumsum(lens)
        return [audio[e - l:e] for e, l in zip(ends, lens)]
    if not os.path.exists(path):
        os.makedirs(os.path.dirname(path), exist_ok=True)
        rng = random.Random(0)
        files = sorted(files())  # listed only when the cache is missing
        rng.shuffle(files)
        parts, total = [], 0
        for f in files:
            a = _read(f)
            if cut and len(a) > cut * SR:  # long music/speech: a random piece
                s = rng.randrange(len(a) - int(cut * SR))
                a = a[s:s + int(cut * SR)]
            peak = np.abs(a).max()
            if peak < 1e-4:
                continue
            parts.append((a / peak * 0.9 * 32767).astype(np.int16))
            total += len(a)
            if max_secs and total > max_secs * SR:
                break
        lens = np.array([len(p) for p in parts])
        np.savez(path, audio=np.concatenate(parts), lens=lens)
    d = np.load(path)
    np.save(flat, d["audio"])
    np.save(os.path.join(HERE, "data", f"pool_{name}_lens.npy"), d["lens"])
    d.close()
    os.remove(path)
    return _pool(name, files, max_secs, cut)

_P = {}
def pools():
    if not _P:
        rirs = os.path.join(DL, "RIRS_NOISES")
        def musan(kind):
            import musan_ok
            ok, _ = musan_ok.ok_files(os.path.join(DL, "musan"))
            return [f for f in glob.glob(os.path.join(DL, "musan", kind, "**", "*.wav"), recursive=True)
                    if os.path.splitext(os.path.basename(f))[0] in ok]
        def rir_files():
            sim = glob.glob(os.path.join(rirs, "simulated_rirs", "**", "*.wav"), recursive=True)
            random.Random(1).shuffle(sim)
            real = [f for f in glob.glob(os.path.join(rirs, "real_rirs_isotropic_noises", "*.wav")) if "noise" not in os.path.basename(f).lower()]
            return sim[:6000] + real
        # Background sounds: MUSAN noise (games, rooms, things) + OpenSLR point sources.
        _P["noise"] = _pool("noise", lambda: musan("noise") + glob.glob(os.path.join(rirs, "pointsource_noises", "*.wav")), cut=30)
        _P["music"] = _pool("music", lambda: musan("music"), max_secs=4 * 3600, cut=60)
        # Other people talking: LibriSpeech train-clean-100 (not the eval sets).
        _P["babble"] = _pool("babble", lambda: glob.glob(os.path.join(DL, "LibriSpeech", "train-clean-100", "**", "*.flac"), recursive=True), max_secs=6 * 3600)
        _P["rir"] = _pool("rir", rir_files)
    return _P

def piece(pool, n):
    """n samples from a random place in the pool (looping short sounds)."""
    a = random.choice(pool)
    if len(a) < n:
        a = np.tile(a, n // len(a) + 1)
    s = random.randrange(len(a) - n + 1)
    return a[s:s + n].astype(np.float32) / 32767

def room(x, p=0.5):
    if random.random() >= p:
        return x
    r = random.choice(pools()["rir"]).astype(np.float32) / 32767
    r = r[np.argmax(np.abs(r)):]  # direct sound first: no added delay
    y = fftconvolve(x, r[: SR])[: len(x)]
    return (y / (np.abs(y).max() + 1e-6) * (np.abs(x).max() + 1e-6)).astype(np.float32)

def colour(x):
    """A random mic and its placement: band limits, tilt, sometimes a resonance."""
    r = random.random()
    if r < 0.15:
        return x
    lo = random.choice([random.uniform(40, 150), random.uniform(150, 400)])
    hi = random.choice([random.uniform(3400, 4000), random.uniform(4000, 7900), 7900])
    x = sosfilt(butter(2, [lo, hi], "bandpass", fs=SR, output="sos"), x).astype(np.float32)
    if random.random() < 0.3:  # peaky cheap headset
        f = random.uniform(800, 4000)
        x = x + random.uniform(0.3, 1.0) * sosfilt(butter(2, [f / 1.2, f * 1.2], "bandpass", fs=SR, output="sos"), x).astype(np.float32)
    return x

def background(n):
    """What's heard besides the speaker: noise, game/music, people, hiss."""
    P = pools()
    out = np.zeros(n, np.float32)
    k = random.random()
    if k < 0.45:
        out += piece(P["noise"], n)
    elif k < 0.70:
        out += piece(P["music"], n)
    elif k < 0.85:
        out += piece(P["babble"], n) * random.uniform(0.3, 1.0)
        if random.random() < 0.5:
            out += piece(P["babble"], n) * random.uniform(0.3, 1.0)
    if random.random() < 0.4 or not out.any():
        w = np.random.randn(n).astype(np.float32)
        c = random.choice([0, 1, 2])  # white, pink-ish, brown-ish
        if c:
            spec = np.fft.rfft(w)
            w = np.fft.irfft(spec / (np.arange(len(spec)) + 1.0) ** (0.5 * c), n).astype(np.float32)
        out += random.uniform(0.05, 1.0) * w / (np.abs(w).max() + 1e-6)
    return out

def mix(voice, snr_db):
    bg = background(len(voice))
    v = np.sqrt(np.mean(voice ** 2)) + 1e-6
    b = np.sqrt(np.mean(bg ** 2)) + 1e-6
    return voice + bg * (v / b) / 10 ** (snr_db / 20)

def level(x):
    """Mic level: mostly normal, sometimes very quiet or clipping."""
    peak = np.exp(random.uniform(np.log(0.02), np.log(0.95)))
    x = x / (np.abs(x).max() + 1e-6) * peak
    if random.random() < 0.05:  # overdriven
        g = random.uniform(1.5, 4)
        x = np.tanh(x * g) / np.tanh(g) * peak
    return x

def voice_change(x):
    """Other voices from a synthetic one: pitch and vocal tract (WORLD), or
    plain speed (pitch and tempo together)."""
    r = random.random()
    if r < 0.30:
        import pyworld as pw
        d = x.astype(np.float64)
        f0, t = pw.dio(d, SR, frame_period=5.0)
        f0 = pw.stonemask(d, f0, t, SR)
        sp = pw.cheaptrick(d, f0, t, SR)
        ap = pw.d4c(d, f0, t, SR)
        # Deeper voices weighted (users missed with a deep voice before).
        semis = random.uniform(-7, -1) if random.random() < 0.6 else random.uniform(-1, 4)
        f0 = f0 * 2 ** (semis / 12)
        a = random.uniform(0.88, 1.12)  # vocal tract length
        if abs(a - 1) > 0.02:
            n = sp.shape[1]
            src = np.clip(np.arange(n) * a, 0, n - 1)
            sp = np.stack([np.interp(src, np.arange(n), row) for row in sp])
            ap = np.stack([np.interp(src, np.arange(n), row) for row in ap])
        y = pw.synthesize(f0, sp, ap, SR, 5.0).astype(np.float32)
        return y[: len(x)] if len(y) >= len(x) else np.pad(y, (0, len(x) - len(y))), 1.0
    if r < 0.55:
        f = random.uniform(0.88, 1.18)
        y = resample_poly(x, 100, int(round(100 * f))).astype(np.float32)
        return y, len(y) / len(x)
    return x, 1.0

def mic(x, snr=(0, 30), clean=0.1):
    """Room, mic, background, level."""
    x = colour(room(x))
    if random.random() >= clean:
        x = mix(x, random.uniform(*snr))
    return level(x).astype(np.float32)
