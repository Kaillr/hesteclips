"""Put clean takes into 2 s windows the way a mic would hear them (noise, room,
mic colour, level, speed), and turn them into openWakeWord features.

usage: python feats.py   (reads gen/pos.npz, gen/neg.npz; writes feats/*.npy)

Window kinds:
  pos        the phrase, ending in the window's last 0.25 s (when it should fire)
  neg        near misses, anywhere in the window
  cut        the phrase with its end cut off by the window (heard only part
             of it: "hashtag heste…") — must not fire yet
  noise      background alone
"""
import glob, os, random, sys
import numpy as np
from scipy.signal import butter, fftconvolve, resample_poly, sosfilt
import soundfile as sf

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from gen import low_priority

SR = 16000
WIN = 32000  # 2.0 s: 16 feature frames

def load_noise():
    path = os.path.join(HERE, "data", "noise16k.npy")
    if os.path.exists(path):
        return [np.asarray(a, dtype=np.float32) for a in np.load(path, allow_pickle=True)]
    out = []
    for f in sorted(glob.glob(os.path.join(HERE, "data", "ESC-50-master", "audio", "*.wav"))):
        a, sr = sf.read(f, dtype="float32")
        a = resample_poly(a, 160, 441) if sr == 44100 else a
        if np.abs(a).max() > 1e-3:
            out.append(a.astype(np.float32))
    np.save(path, np.array(out, dtype=object), allow_pickle=True)
    return out

def load_rirs():
    out = []
    for f in glob.glob(os.path.join(HERE, "data", "mit_rir", "**", "*.wav"), recursive=True):
        a, sr = sf.read(f, dtype="float32")
        if a.ndim > 1:
            a = a[:, 0]
        if sr != SR:
            a = resample_poly(a, SR, sr)
        out.append(a / (np.abs(a).max() + 1e-9))
    return out

NOISE, RIRS = load_noise(), load_rirs()

def background(n):
    """n samples of background: a recorded sound, coloured noise, or both."""
    out = np.zeros(n, np.float32)
    if random.random() < 0.8:
        a = random.choice(NOISE)
        while len(a) < n:
            a = np.concatenate([a, random.choice(NOISE)])
        s = random.randrange(len(a) - n + 1)
        out += a[s:s + n] / (np.abs(a[s:s + n]).max() + 1e-6)
    if random.random() < 0.5:
        white = np.random.randn(n).astype(np.float32)
        # 0 = white, 1 = pink-ish, 2 = brown-ish
        k = random.choice([0, 1, 2])
        if k:
            spec = np.fft.rfft(white)
            f = np.arange(len(spec)) + 1.0
            white = np.fft.irfft(spec / f ** (0.5 * k), n).astype(np.float32)
        out += random.uniform(0.1, 1.0) * white / (np.abs(white).max() + 1e-6)
    return out

def colour(x):
    """A random mic: some low and high cut."""
    if random.random() < 0.6:
        lo = random.uniform(60, 400)
        hi = random.uniform(3500, 7900)
        x = sosfilt(butter(2, [lo, hi], "bandpass", fs=SR, output="sos"), x).astype(np.float32)
    return x

def speed(x, lo=1.0, hi=1.35):
    """Faster (pitch goes up with it, as another voice would be)."""
    f = random.uniform(lo, hi)
    return resample_poly(x, 100, int(100 * f)).astype(np.float32)

def room(x):
    if random.random() < 0.4:
        y = fftconvolve(x, random.choice(RIRS))[: len(x)]
        return (y / (np.abs(y).max() + 1e-6) * np.abs(x).max()).astype(np.float32)
    return x

def window(clip, kind):
    x = clip.astype(np.float32) / 32768
    if kind in ("pos", "cut") and random.random() < 0.35:
        x = speed(x)
    x = room(colour(x))
    w = np.zeros(WIN, np.float32)
    if kind == "pos":
        end = WIN - random.randint(0, int(0.25 * SR))
        start = end - len(x)
        a = max(start, 0)
        w[a:end] = x[a - start:]
    elif kind == "cut":
        # 35–75 % of the phrase heard so far.
        keep = int(len(x) * random.uniform(0.35, 0.75))
        end = WIN - random.randint(0, int(0.25 * SR))
        a = max(end - keep, 0)
        w[a:end] = x[keep - (end - a):keep]
    elif kind == "neg":
        if len(x) >= WIN:
            x = x[:WIN]
        start = random.randint(-len(x) // 3, WIN - len(x) * 2 // 3)
        a, b = max(start, 0), min(start + len(x), WIN)
        w[a:b] = x[a - start:b - start]
    voice = np.abs(w).max() + 1e-6
    if kind != "noise" and random.random() < 0.9:
        snr = random.uniform(0, 30)
        bg = background(WIN)
        w += bg / (np.abs(bg).max() + 1e-6) * voice / 10 ** (snr / 20)
    elif kind == "noise":
        w = background(WIN)
    # Mic level: peaks from quiet to loud.
    w = w / (np.abs(w).max() + 1e-6) * random.uniform(0.05, 0.9)
    return (w * 32767).astype(np.int16)

def main():
    low_priority()
    random.seed(0)
    np.random.seed(0)
    gpu = os.environ.get("FEATS_DEVICE") == "gpu"
    if gpu:
        import torch  # its CUDA libraries, for onnxruntime-gpu
    from openwakeword.utils import AudioFeatures
    # FEATS_TEST=1: the trial takes into feats_test/, to check this step first.
    test = os.environ.get("FEATS_TEST")
    ncpu = int(os.environ.get("FEATS_CPU", "4"))
    feats = AudioFeatures(os.path.join(HERE, "models", "melspectrogram.onnx"), os.path.join(HERE, "models", "embedding_model.onnx"), ncpu=ncpu, device="gpu" if gpu else "cpu")
    print("feature providers:", feats.melspec_model.get_providers() if hasattr(feats, "melspec_model") else "?", flush=True)
    src = ("trial_pos.npz", "trial_neg.npz") if test else ("pos.npz", "neg.npz")
    pos = [v for _, v in np.load(os.path.join(HERE, "gen", src[0])).items()]
    neg = [v for _, v in np.load(os.path.join(HERE, "gen", src[1])).items()]
    outdir = os.path.join(HERE, "feats_test" if test else "feats")
    os.makedirs(outdir, exist_ok=True)
    plan = [("pos", pos, 2), ("neg", neg, 2), ("cut", pos, 1), ("noise", pos, 0.4)]
    for kind, src, copies in plan:
        n = int(len(src) * copies)
        out = []
        for start in range(0, n, 2000):
            batch = np.stack([window(src[i % len(src)], kind) for i in range(start, min(start + 2000, n))])
            out.append(feats.embed_clips(batch, batch_size=256, ncpu=ncpu).astype(np.float16))
            print(kind, min(start + 2000, n), "/", n, flush=True)
        arr = np.concatenate(out)
        np.save(os.path.join(outdir, f"{kind}.npy"), arr)
        print(kind, arr.shape, flush=True)

if __name__ == "__main__":
    main()
