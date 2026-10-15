"""Synthesize utterances with Piper and Kokoro: the phrase (with the talk
around it) and near misses, 16 kHz int16, with the sample range of every
time the phrase is said (from the engines' phoneme timings).

usage: python gen.py <pos|neg> <count> <split: train|test> <out.npz> [seed]
       GEN_PIPER (CPU workers, default 7), GEN_KOKORO (GPU workers, default 3):
       Piper is as fast on one CPU thread as on the GPU, Kokoro needs the GPU,
       and more than ~4 GPU processes fill an 8 GB card and stall.

Test voices and speakers are never used for train, so the test split hears
people the model hasn't.
"""
import json, os, random, subprocess, sys, time
import numpy as np
from scipy.signal import resample_poly

HERE = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(HERE, "src"))
sys.path.insert(0, os.path.join(HERE, "voices"))
import text as T
from pick import EN, ACCENT

SKIP = {"lt_LT-reginute1-medium", "zh_CN-chaowen-medium"}  # not espeak phonemes
# Voices that don't say a plain "HasteClip that" recognizably (mean phoneme
# error > 0.45 in voice_qa.py; vctk is kept: its 109 speakers vary, bad
# takes are dropped one by one).
QUALITY = json.load(open(os.path.join(HERE, "voices", "quality.json")))
SKIP |= {k for k, v in QUALITY.items() if v > 0.45 and k != "en_GB-vctk-medium" and not k.startswith("kokoro:")}
KOKORO_SKIP = {k.split(":")[1] for k, v in QUALITY.items() if v > 0.45 and k.startswith("kokoro:")}
TEST_VOICES = {"en_US-norman-medium", "en_GB-alba-medium", "de_DE-thorsten_emotional-medium", "fr_FR-siwis-medium",
               "es_MX-claude-high", "pl_PL-mc_speech-medium", "hu_HU-imre-medium", "te_IN-venkatesh-medium",
               "ur_PK-aegis_female-medium", "uk_UA-tetiana-high"}
KOKORO_TEST = {"af_nova", "am_eric", "bf_lily", "bm_lewis", "hf_beta", "pm_alex", "if_sara"}
# Sounds a voice doesn't have: the nearest one its speakers would say.
FALLBACK = {"ð": ["d", "z", "t"], "θ": ["t", "s", "f"], "æ": ["ɛ", "a", "e"], "ɪ": ["i", "e"], "ɹ": ["r", "ɾ", "ʁ"],
            "ʌ": ["a", "ɐ", "ə"], "ə": ["e", "ɐ", "a"], "ɑ": ["a"], "ɒ": ["o", "ɔ", "a"], "ʊ": ["u"], "ɜ": ["ə", "e", "ø"],
            "ɚ": ["ə", "e"], "ʔ": ["t", ""], "ː": [""], "ˌ": [""], "ʰ": [""], "x": ["h", "k"], "h": ["x", ""],
            "ɡ": ["g"], "ʃ": ["s"], "ʒ": ["ʃ", "z"], "tʃ": ["ts"], "ŋ": ["n"], "e": ["ɛ"], "ɛ": ["e"], "j": ["i"]}

def low_priority():
    if os.name == "nt":
        import ctypes
        k = ctypes.windll.kernel32
        k.SetPriorityClass(k.GetCurrentProcess(), 0x4000)  # below normal

def session(path_or_bytes, gpu):
    import onnxruntime as ort
    ort.set_default_logger_severity(3)
    so = ort.SessionOptions()
    so.intra_op_num_threads = 1
    so.inter_op_num_threads = 1
    prov = [("CUDAExecutionProvider", {"cudnn_conv_algo_search": "HEURISTIC", "arena_extend_strategy": "kSameAsRequested", "gpu_mem_limit": str(1100 * 1024 * 1024)}), "CPUExecutionProvider"] if gpu else ["CPUExecutionProvider"]
    return ort.InferenceSession(path_or_bytes, so, providers=prov)

class Piper:
    def __init__(self, name, gpu):
        import onnx
        from piper import PiperVoice
        from piper.patch_voice_with_alignment import add_alignment_output
        from piper.config import PiperConfig
        path = os.path.join(HERE, "voices", name + ".onnx")
        cfg = json.load(open(path + ".json", encoding="utf-8"))
        m = onnx.load(path)
        add_alignment_output(m)
        self.config = PiperConfig.from_dict(cfg)
        self.voice = PiperVoice(config=self.config, session=session(m.SerializeToString(), gpu), espeak_data_dir=__import__("piper").voice.ESPEAK_DATA_DIR)
        self.name, self.ids = name, self.config.phoneme_id_map
        self.speakers = self.config.num_speakers
        self.rate = self.config.sample_rate
        self.accent = name not in EN

    def phonemes_of(self, text):
        from piper.voice import EspeakPhonemizer
        import piper.voice as pv
        if pv._ESPEAK_PHONEMIZER is None:
            pv._ESPEAK_PHONEMIZER = EspeakPhonemizer(pv.ESPEAK_DATA_DIR)
        sents = pv._ESPEAK_PHONEMIZER.phonemize("en-us", text)
        out = []
        for s in sents:
            out += s + [" "]
        return out[:-1]

    def fit(self, ph):
        """Phonemes this voice has: missing ones swapped for the nearest."""
        out = []
        for p in ph:
            if p in self.ids:
                out.append(p)
                continue
            for alt in FALLBACK.get(p, []):
                if alt == "" or alt in self.ids:
                    if alt:
                        out.append(alt)
                    break
        return out

    def say(self, segs, speaker, length_scale):
        from piper import SynthesisConfig
        ph, marks = [], []
        for kind, s in segs:
            if ph and ph[-1] != " ":
                ph.append(" ")
            p = self.fit(self.phonemes_of(s) if kind == "text" else list(s))
            if kind == "phrase":
                marks.append((len(ph), len(ph) + len(p)))
            ph += p
        # ids: BOS PAD (p PAD)* EOS; phoneme i is ids 2 + 2i (and its PAD).
        ids = list(self.ids["^"]) + list(self.ids["_"])
        for p in ph:
            ids += list(self.ids[p]) + list(self.ids["_"])
        ids += list(self.ids["$"])
        cfg = SynthesisConfig(speaker_id=speaker, length_scale=length_scale,
                              noise_scale=random.uniform(0.45, 0.9), noise_w_scale=random.uniform(0.5, 1.1))
        audio, per_id = self.voice.phoneme_ids_to_audio(ids, cfg, include_alignments=True)
        edges = np.concatenate([[0], np.cumsum(per_id)])
        spans = [(int(edges[2 + 2 * a]), int(edges[2 + 2 * b])) for a, b in marks]
        return audio, self.rate, spans

class Kokoro:
    def __init__(self, gpu):
        from kokoro_onnx import Kokoro as K
        self.k = K.from_session(session(os.path.join(HERE, "kokoro", "kokoro-timed.onnx"), gpu), os.path.join(HERE, "kokoro", "voices-v1.0.bin"))
        self.names = sorted(self.k.voices.files)

    def voice(self, split):
        names = [n for n in self.names if (n in KOKORO_TEST) == (split == "test") and n not in KOKORO_SKIP]
        if split == "train" and random.random() < 0.6:
            # A new speaker: a blend of two or three voices.
            pick = random.sample(names, random.choice([2, 3]))
            w = np.random.dirichlet(np.ones(len(pick)))
            return "+".join(pick), sum(wi * self.k.voices[n] for wi, n in zip(w, pick))
        n = random.choice(names)
        return n, self.k.voices[n]

    def say(self, segs, style, speed):
        parts, marks, pos = [], [], 0
        for kind, s in segs:
            p = self.k.tokenizer.phonemize(s, "en-us").strip() if kind == "text" else s
            if parts:
                parts.append(" ")
                pos += 1
            if kind == "phrase":
                marks.append((pos, pos + len(p)))
            parts.append(p)
            pos += len(p)
        ph = "".join(parts)
        audio, rate, tim = self.k.create_timed(ph, style, speed=speed, is_phonemes=True)
        # One timing per spoken token, in order; spaces/marks may be absent.
        spans = []
        if tim:
            idx = [i for i, ch in enumerate(ph) if ch in self.k.tokenizer.vocab]
            if len(idx) == len(tim):
                for a, b in marks:
                    inside = [t for i, t in zip(idx, tim) if a <= i < b]
                    if inside:
                        spans.append((int(inside[0].start * rate), int(inside[-1].end * rate)))
        if len(spans) != len(marks):
            raise ValueError(f"timings didn't line up ({len(tim)} timings)")
        return audio, rate, spans

KOKORO_SHARE = 0.4

def worker(kind, n, split, seed, gpu, out):
    low_priority()
    random.seed(seed)
    np.random.seed(seed % 2**32)
    if gpu:
        import torch  # noqa: F401  CUDA libraries for onnxruntime-gpu
    MULTI = ("en_US-libritts_r-medium", "en_GB-vctk-medium", "de_DE-mls-medium", "fr_FR-mls-medium", "nl_NL-mls-medium",
             "no_NO-nvcc-medium", "en_US-arctic-medium", "en_GB-aru-medium")
    kok = None
    clips, spans, meta = [], [], []
    t0, fails = time.time(), 0
    batch = []  # (say, accent, who) for the voice in use: ~25 takes, then the next voice
    while len(clips) < n:
        if not batch:
            if random.random() < KOKORO_SHARE:
                if kok is None:
                    kok = Kokoro(gpu)
                for _ in range(25):
                    who, style = kok.voice(split)
                    fast = random.random() < 0.45
                    speed = random.uniform(1.1, 1.5) if fast else random.uniform(0.8, 1.1)
                    accent = who.split("+")[0][:1] not in "ab"
                    batch.append((lambda segs, st=style, sp=speed: kok.say(segs, st, sp), accent, "kokoro:" + who))
            else:
                # Native English voices about twice as often as accented ones.
                pool = [v for v in (EN if random.random() < 0.65 else ACCENT) if v not in SKIP]
                # Held-out voices only in test; multi-speaker voices are split by speaker.
                pool = [v for v in pool if (v in TEST_VOICES) == (split == "test") or v in MULTI]
                name = random.choice(pool)
                try:
                    v = Piper(name, gpu)
                except Exception as e:
                    print("voice failed:", name, e, file=sys.stderr, flush=True)
                    continue
                for _ in range(25):
                    sp = None
                    if v.speakers > 1:
                        sp = random.choice([s for s in range(v.speakers) if (s % 10 == 0) == (split == "test")])
                    ls = random.uniform(0.6, 0.85) if random.random() < 0.45 else random.uniform(0.85, 1.3)
                    batch.append((lambda segs, v=v, sp=sp, ls=ls: v.say(segs, sp, ls), v.accent, f"{name}#{sp}"))
        say, accent, who = batch.pop()
        segs = T.positive(accent) if kind == "pos" else T.negative(accent)
        try:
            audio, rate, sp_ = say(segs)
        except Exception as e:
            fails += 1
            if fails < 20:
                print("skip:", who, e, file=sys.stderr, flush=True)
            continue
        if rate != 16000:
            from math import gcd
            g = gcd(16000, rate)
            audio = resample_poly(audio, 16000 // g, rate // g)
            sp_ = [(a * 16000 // rate, b * 16000 // rate) for a, b in sp_]
        audio = np.asarray(audio, np.float32)
        peak = np.abs(audio).max()
        if peak < 1e-3 or (kind == "pos" and not sp_):
            continue
        # Trim the engine's own silence (keep 80 ms), shifting the spans.
        loud = np.flatnonzero(np.abs(audio) > 0.02 * peak)
        a, b = max(loud[0] - 1280, 0), min(loud[-1] + 1280, len(audio))
        audio = audio[a:b] / peak * 0.9
        sp_ = [(max(s - a, 0), min(e - a, b - a)) for s, e in sp_]
        if len(audio) > 6 * 16000:
            continue
        clips.append((audio * 32767).astype(np.int16))
        spans.append(sp_ + [(-1, -1)] * (2 - len(sp_)))
        meta.append({"who": who, "segs": segs})
        if len(clips) % 50 == 0:
            print(f"{len(clips)}/{n} {time.time() - t0:.0f}s", flush=True)
    lens = np.array([len(c) for c in clips])
    np.savez(out, audio=np.concatenate(clips), lens=lens, spans=np.array(spans, np.int64)[:, :2])
    with open(out + ".jsonl", "w", encoding="utf-8") as f:
        for m in meta:
            f.write(json.dumps(m, ensure_ascii=False) + "\n")
    print(f"done {len(clips)} in {time.time() - t0:.0f}s, {fails} skipped", flush=True)

def load(path):
    d = np.load(path)
    audio, lens = d["audio"], d["lens"]  # (each d[...] reads the file again)
    ends = np.cumsum(lens)
    return [audio[e - l:e] for e, l in zip(ends, lens)], d["spans"]

if __name__ == "__main__":
    if sys.argv[1] == "part":
        _, _, kind, n, split, seed, gpu, share, out = sys.argv
        KOKORO_SHARE = float(share)
        worker(kind, int(n), split, int(seed), gpu == "1", out)
        sys.exit(0)
    low_priority()
    kind, count, split, out = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4]
    seed = int(sys.argv[5]) if len(sys.argv) > 5 else 0
    np_, nk = int(os.environ.get("GEN_PIPER", "7")), int(os.environ.get("GEN_KOKORO", "3"))
    # Kokoro makes ~35 % of the takes.
    plan = [(0.0, "0", int(count * 0.65 / np_) + 1) for _ in range(np_)] + [(1.0, "1", int(count * 0.35 / nk) + 1) for _ in range(nk)]
    jobs = len(plan)
    parts = [f"{out}.part{j}.npz" for j in range(jobs)]
    logs = [open(f"{out}.part{j}.log", "w") for j in range(jobs)]
    procs = [subprocess.Popen([sys.executable, __file__, "part", kind, str(n), split, str(seed * 1000 + j), gpu, str(share), parts[j]],
                              stdout=logs[j], stderr=subprocess.STDOUT) for j, (share, gpu, n) in enumerate(plan)]
    count = sum(n for *_, n in plan)
    t0 = time.time()
    while any(p.poll() is None for p in procs):
        time.sleep(20)
        done = []
        for j in range(jobs):
            lines = [l for l in open(f"{out}.part{j}.log", encoding="utf-8", errors="replace").read().splitlines() if "/" in l and l.split("/")[0].isdigit()]
            done.append(int(lines[-1].split("/")[0]) if lines else 0)
        rate = sum(done) / max(time.time() - t0, 1)
        left = (count - sum(done)) / rate / 60 if rate else float("nan")
        print(f"{kind} {split}: {sum(done)}/{count} ({rate:.1f}/s, ~{left:.0f} min left)", flush=True)
    bad = [j for j, p in enumerate(procs) if p.returncode != 0]
    if bad:
        sys.exit(f"workers {bad} failed: see {out}.part{bad[0]}.log")
    clips, spans, meta = [], [], []
    for p in parts:
        c, s = load(p)
        clips += c
        spans.append(s)
        meta += open(p + ".jsonl", encoding="utf-8").read().splitlines()
    spans = np.concatenate(spans)
    np.savez(out, audio=np.concatenate(clips), lens=np.array([len(c) for c in clips]), spans=spans)
    open(out + ".jsonl", "w", encoding="utf-8").write("\n".join(meta) + "\n")
    for p in parts:
        os.remove(p)
        os.remove(p + ".jsonl")
    print(f"{kind} {split}: {len(clips)} clips, {np.mean([len(c) for c in clips]) / 16000:.2f} s mean, {time.time() - t0:.0f} s", flush=True)
