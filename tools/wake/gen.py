"""Synthesize clean takes with Piper: the phrase (positives) and near misses
(adversarial negatives), 16 kHz int16, each its own length.

usage: python gen.py <kind: pos|neg> <count> <out.npz> [seed]
"""
import json, multiprocessing as mp, os, random, sys
import numpy as np
from scipy.signal import resample_poly

HERE = os.path.dirname(os.path.abspath(__file__))
V = os.path.join(HERE, "voices")
# (file, speakers, accent): english voices get English-style phonemes, the
# Scandinavian ones Norwegian-style.
VOICES = [
    ("en_US-libritts_r-medium", 904, "en", 5),
    ("en_GB-vctk-medium", 109, "en", 3),
    ("en_US-arctic-medium", 18, "en", 1),
    ("no_NO-talesyntese-medium", 1, "no", 2),
    ("sv_SE-nst-medium", 1, "no", 1),
    ("da_DK-talesyntese-medium", 1, "no", 1),
]

# The phrase as phonemes, part by part. "HesteClip" is one word, Norwegian
# "heste" + English "clip".
EN = {
    "hashtag": ["hˈæʃtæɡ", "hˈæʃtæk", "hˈɛʃtæɡ", "hˈæʃtɛɡ", "hˈæʃ tˈæɡ", "hˈɑʃtæɡ", "hæʃtˈæɡ"],
    "heste": ["hˈɛstə", "hˈɛstɪ", "hˈɛstɛ", "hˈɛste", "hˈɛstɐ", "hˈæstə", "hˈɛsti"],
    "clip": ["klˌɪp", "klˈɪp", "klɪp", "klˈɪpː"],
    "that": ["ðˈæt", "dˈæt", "ðˈɛt", "dˈɛt", "ðæt"],
}
# Slurred, fast-speech forms: sounds swallowed, words run together. Taken
# for a part a quarter of the time. "Heste" keeps at least "hɛs", so the
# phrase never becomes "hashtag clip that" (a near miss).
SLUR = {
    "hashtag": ["hˈæʃtæ", "hˈæʃtə", "hˈæʃtɪ", "hæʃtˌæ", "hˈɛʃtæ", "æʃtæɡ"],
    "heste": ["hˈɛs", "hˈɛst", "hɛstə", "hˈɛsə", "hˈɪstə"],
    "clip": ["klɪ", "klˈɪ", "kɪp", "klɪp"],
    "that": ["dæ", "ðæ", "ðˈæ", "dɛ", "æt"],
}

NO = {
    "hashtag": ["hˈæʃtæɡ", "hˈɛʃtɛːɡ", "hˈaʃtɛːɡ", "hˈæʃtɛɡ", "hˈɑʃtaɡ"],
    "heste": ["hˈɛstə", "hˈɛste", "hˈɛstɛ"],
    "clip": ["klˌɪp", "klˌɪpː", "klˈɪp"],
    "that": ["dˈɛt", "dˈæt", "ðˈæt", "dˈɛːt", "dˈatː"],
}

def phrase(p, parts, slur=False):
    """The phrase (or some of its parts), glued as said: heste+clip is one word."""
    words = []
    for i, part in enumerate(parts):
        ph = random.choice(SLUR[part] if slur and random.random() < 0.25 else p[part])
        glue = 0.8 if not slur else 0.9
        if part == "clip" and i > 0 and parts[i - 1] == "heste" and random.random() < glue:
            words[-1] += ph.replace("ˈ", "ˌ")
        else:
            words.append(ph)
    return " ".join(words)

# Near misses: parts of the phrase, and things that sound like it. None of
# these may save a clip.
NEG_PARTS = [
    ["hashtag"], ["heste"], ["hashtag", "heste"], ["heste", "clip"], ["heste", "clip", "that"],
    ["hashtag", "heste", "clip"], ["hashtag", "clip", "that"], ["hashtag", "clip"], ["clip", "that"],
    ["hashtag", "that"], ["heste", "that"], ["hashtag", "hashtag"],
]
NEG_TEXT_EN = [
    "has the clip that", "has to clip that", "hashtag clips", "hashtag hesteclips", "open hesteclips",
    "hesteclips", "hashtag best clip that", "hashtag festival clip", "hey steve clip that", "hashtag hasty clip",
    "hashtag this clip", "hashtag hester", "clip it", "clip that please", "did you clip that", "hashtag", "hashtag blessed",
    "hashtag test clip", "has the clip", "that clip was crazy", "nice clip", "hashtag guest list", "I'll get the helmet",
    "hastily clip that", "hash browns", "chester clipped that", "hester clip that", "the best clip", "hesitate",
    "let's go", "what was that", "oh my god", "no way", "get him", "behind you", "reload", "nice shot", "good game",
    "one more game", "hold on", "heal me", "I'm going in", "that's crazy", "wait what", "come here",
]
NEG_TEXT_NO = [
    "heste", "hest", "hestene", "klipp det", "kast det", "hast deg", "hashtag", "hei hvordan går det", "hæ", "hester",
    "heste klipp", "hashtag hest", "jeg klipper det", "har du klippet det", "hesteklipp", "hva skjer", "kom igjen",
    "hestesko", "dette er bra", "neste klipp", "beste klipp", "hasteklipp", "klipp", "det var gøy", "skyt han",
]

def low_priority():
    """Below-normal CPU priority: games and the desktop come first."""
    if os.name == "nt":
        import ctypes
        k = ctypes.windll.kernel32
        k.SetPriorityClass(k.GetCurrentProcess(), 0x4000)

def worker(args):
    low_priority()
    kind, n, seed = args
    random.seed(seed)
    from piper import PiperVoice, SynthesisConfig
    cuda = os.environ.get("GEN_DEVICE") == "cuda"
    if cuda:
        import torch  # its CUDA libraries, for onnxruntime-gpu
    import onnxruntime as ort
    ort.set_default_logger_severity(3)
    voices = {}
    for name, *_ in VOICES:
        v = PiperVoice.load(os.path.join(V, name + ".onnx"))
        # One core per worker: ten workers each spreading over every core
        # fought each other and ran ~30x slower.
        so = ort.SessionOptions()
        so.intra_op_num_threads = 1
        so.inter_op_num_threads = 1
        providers = ["CUDAExecutionProvider", "CPUExecutionProvider"] if cuda else ["CPUExecutionProvider"]
        v.session = ort.InferenceSession(os.path.join(V, name + ".onnx"), sess_options=so, providers=providers)
        voices[name] = v
    weights = [w for *_, w in VOICES]
    out = []
    while len(out) < n:
        name, speakers, accent, _ = random.choices(VOICES, weights)[0]
        voice = voices[name]
        p = EN if accent == "en" else NO
        if kind == "pos":
            words = phrase(p, ["hashtag", "heste", "clip", "that"], slur=random.random() < 0.4)
            # Run together: no pause between some words.
            if random.random() < 0.3:
                words = words.replace(" ", "", 1)
            text = "[[ " + words + " ]]"
        elif random.random() < 0.55:
            text = "[[ " + phrase(p, random.choice(NEG_PARTS)) + " ]]"
        else:
            text = random.choice(NEG_TEXT_EN if accent == "en" else NEG_TEXT_NO)
        cfg = SynthesisConfig(
            speaker_id=random.randrange(speakers) if speakers > 1 else None,
            # The user says it fast: lean short.
            length_scale=random.uniform(0.5, 0.8) if random.random() < 0.5 else random.uniform(0.8, 1.2),
            noise_scale=random.uniform(0.4, 0.9),
            noise_w_scale=random.uniform(0.5, 1.1),
        )
        try:
            audio = np.concatenate([c.audio_float_array for c in voice.synthesize(text, syn_config=cfg)])
        except Exception as e:  # an unknown phoneme in some voice
            print("skip", name, text, e, file=sys.stderr)
            continue
        audio = resample_poly(audio, 320, 441)  # 22050 -> 16000
        # Trim the voice's own silence, keep 50 ms.
        loud = np.flatnonzero(np.abs(audio) > 0.02)
        if len(loud) == 0:
            continue
        a, b = max(loud[0] - 800, 0), min(loud[-1] + 800, len(audio))
        audio = audio[a:b]
        if len(audio) > 1.9 * 16000:
            continue
        out.append((np.clip(audio, -1, 1) * 32767).astype(np.int16))
    return out

if __name__ == "__main__":
    low_priority()
    if sys.argv[1] == "part":
        # One worker: python gen.py part <kind> <n> <seed> <out.npz>
        kind, n, seed, out = sys.argv[2], int(sys.argv[3]), int(sys.argv[4]), sys.argv[5]
        clips = []
        for chunk in range(0, n, 100):
            clips += worker((kind, min(100, n - chunk), seed * 100 + chunk))
            print(f"{len(clips)}/{n}", flush=True)
        np.savez(out, *clips)
        sys.exit(0)
    import subprocess, time
    kind, count, path = sys.argv[1], int(sys.argv[2]), sys.argv[3]
    seed = int(sys.argv[4]) if len(sys.argv) > 4 else 0
    jobs = int(os.environ.get("GEN_JOBS", "10"))
    per = -(-count // jobs)
    parts = [f"{path}.part{j}.npz" for j in range(jobs)]
    logs = [f"{path}.part{j}.log" for j in range(jobs)]
    procs = [subprocess.Popen([sys.executable, __file__, "part", kind, str(per), str(seed * 1000 + j), parts[j]], stdout=open(logs[j], "w"), stderr=subprocess.STDOUT) for j in range(jobs)]
    while any(p.poll() is None for p in procs):
        time.sleep(30)
        done = [open(l).read().split()[-1:] for l in logs]
        print(kind, "progress", " ".join(d[0] if d else "0" for d in done), flush=True)
    bad = [j for j, p in enumerate(procs) if p.returncode != 0]
    if bad:
        sys.exit(f"workers {bad} failed: see {logs[bad[0]]}")
    clips = [c for f in parts for _, c in np.load(f).items()]
    np.savez(path, *clips[:count])
    for f in parts:
        os.remove(f)
    lens = [len(c) / 16000 for c in clips]
    print(f"{kind}: {len(clips)} clips, {np.mean(lens):.2f} s mean, {np.min(lens):.2f}-{np.max(lens):.2f} s", flush=True)
