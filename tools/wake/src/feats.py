"""Training/eval features: openWakeWord embeddings exactly as the app computes
them live (feat.stream_embed), for

  scenes  synthetic takes heard through a mic (aug.py) with background
          before and after; every 80 ms window labelled by where the phrase
          is in it.
  long    hours of everyday audio (speech in 8 languages, music, noise),
          all windows negative.

usage:
  python feats.py scenes <gen.npz> <pos|neg> <copies> <out.npz> [seed]
  python feats.py long <source> <out.npz> [hours] [seed]
     sources: ls100, ls500, lseval, musan_speech, musan_music, musan_noise,
              mls_<lang>, mls_<lang>_test

Window labels (scenes): 1 the whole phrase is in the window; 2 "cut": the
phrase has started but > 0.2 s of its end isn't heard yet; 3 "late": its
start (> 0.25 s) is gone, the end is there ("...clip that"); 0 other speech
(talk around it, near misses); 4 background only; -1 don't train (edges).
-9: the row can't end a window (fewer than 16 rows of its scene before it).
"""
import glob, io, multiprocessing as mp, os, random, sys, time
import numpy as np

HERE = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(HERE, "src"))
SR, HOP, WIN = 16000, 1280, 16
WIN_SECS = 15 * HOP / SR + (75 * 160 + 512) / SR  # ~1.98 s heard by a window
LEAD = 1.2  # s of background before each scene: rows don't see the previous one

def _seed(s):
    random.seed(s)
    np.random.seed(s % 2**32)

# ---------------- scenes ----------------

_clips = None
def _init_scenes(path, seed):
    global _clips
    from gen import load
    _clips = load(path)
    _seed(seed + os.getpid())

def build_scene(args):
    import aug
    i, kind = args
    clips, spans = _clips
    x = clips[i].astype(np.float32) / 32768
    sp = [(a / SR, b / SR) for a, b in spans[i] if a >= 0]
    x, f = aug.voice_change(x)
    sp = [(a * f, b * f) for a, b in sp]
    # Room for every alignment: the phrase ending at a window's end (needs
    # ~2 s before it) through starting at its start (~2 s after).
    secs = len(x) / SR
    pre = random.uniform(0.15, 1.4) + max(0.0, 1.85 - secs)
    parts = [np.zeros(int((LEAD + pre) * SR), np.float32)]
    # Sometimes someone else was talking just before.
    if random.random() < 0.15:
        talk = aug.piece(aug.pools()["babble"], int(random.uniform(0.5, 1.5) * SR))
        talk *= np.abs(x).max() * random.uniform(0.3, 1.0) / (np.abs(talk).max() + 1e-6)
        parts[0][-len(talk):] += talk[: len(parts[0])]
    start = len(parts[0]) / SR
    parts.append(x)
    parts.append(np.zeros(int((random.uniform(0.3, 1.2) + max(0.0, 1.7 - secs)) * SR), np.float32))
    audio = np.concatenate(parts)
    audio = aug.mic(audio)
    sp = [(a + start, b + start) for a, b in sp]
    # Whole 80 ms chunks only.
    n = len(audio) // HOP * HOP
    return (audio[:n] * 32767).astype(np.int16), sp, start, start + len(x) / SR, kind

def label_rows(n_rows, sp, ustart, uend, kind):
    lab = np.full(n_rows, -9, np.int8)
    for k in range(n_rows):
        we = (k + 1) * HOP / SR
        ws = we - WIN_SECS
        if ws < LEAD - 0.4:  # sees the previous scene's audio
            continue
        if we < ustart + 0.05 or ws > uend:
            lab[k] = 4
            continue
        if kind == "neg":
            lab[k] = 0
            continue
        cls = []
        for a, b in sp:
            if a >= ws - 0.04 and b <= we + 0.03:
                cls.append(1)
            elif b > we + 0.2 and a < we - 0.1:
                cls.append(2)
            elif a < ws - 0.25 and b <= we + 0.03 and b > ws + 0.3:
                cls.append(3)
            elif b < ws or a > we - 0.1:
                cls.append(0)
            else:
                cls.append(-1)
        # A whole phrase anywhere wins; then "don't train"; then cut, late.
        best = next((c for c in (1, -1, 2, 3) if c in cls), 0)
        lab[k] = best
    return lab

def scenes(src, kind, copies, out, seed=0):
    import feat
    from gen import load
    n = len(load(src)[0])
    jobs = [(i, kind) for _ in range(copies) for i in range(n)]
    random.Random(seed).shuffle(jobs)
    rows, labels, starts, t0 = [], [], [], time.time()
    total = 0
    with mp.Pool(int(os.environ.get("FEATS_CPU", "6")), initializer=_init_scenes, initargs=(src, seed)) as pool:
        batch = []
        def flush():
            nonlocal total
            audio = np.concatenate([b[0] for b in batch])
            emb = feat.stream_embed(audio)
            o = 0
            for a, sp, us, ue, kd in batch:
                k = len(a) // HOP
                r = emb[o:o + k]
                o += k
                lab = label_rows(k, sp, us, ue, kd)
                keep = np.flatnonzero(lab != -9)
                if len(keep) == 0:
                    continue
                lo = max(keep[0] - (WIN - 1), 0)
                starts.append(total)
                rows.append(r[lo:].astype(np.float16))
                lb = lab[lo:].copy()
                lb[: keep[0] - lo] = -9
                labels.append(lb)
                total += len(r) - lo
            batch.clear()
        for i, s in enumerate(pool.imap_unordered(build_scene, jobs, chunksize=16), 1):
            batch.append(s)
            if len(batch) == 64:
                flush()
            if i % 2000 == 0:
                el = time.time() - t0
                print(f"scenes {i}/{len(jobs)} ({i / el:.0f}/s, ~{(len(jobs) - i) / (i / el) / 60:.1f} min left)", flush=True)
        if batch:
            flush()
    lab = np.concatenate(labels)
    np.savez(out, rows=np.concatenate(rows), labels=lab, starts=np.array(starts))
    u, c = np.unique(lab, return_counts=True)
    print(f"{out}: {len(starts)} scenes, windows by label {dict(zip(u.tolist(), c.tolist()))}, {time.time() - t0:.0f} s", flush=True)

# ---------------- long everyday audio ----------------

def source_files(source):
    D = os.path.join(HERE, "dl")
    if source == "ls100":
        return sorted(glob.glob(os.path.join(D, "LibriSpeech", "train-clean-100", "**", "*.flac"), recursive=True))
    if source == "ls500":
        return sorted(glob.glob(os.path.join(D, "LibriSpeech", "train-other-500", "**", "*.flac"), recursive=True))
    if source == "lseval":
        return sorted(glob.glob(os.path.join(D, "LibriSpeech", "test-other", "**", "*.flac"), recursive=True)
                      + glob.glob(os.path.join(D, "LibriSpeech", "dev-other", "**", "*.flac"), recursive=True))
    if source.startswith("musan_"):
        import musan_ok
        ok, _ = musan_ok.ok_files(os.path.join(D, "musan"))
        kind = source.split("_")[1]
        return sorted(f for f in glob.glob(os.path.join(D, "musan", kind, "**", "*.wav"), recursive=True)
                      if os.path.splitext(os.path.basename(f))[0] in ok)
    if source.startswith("mls_"):
        lang = source.split("_")[1]
        part = "test" if source.endswith("_test") else "train"
        return [f"{p}#{i}" for p in sorted(glob.glob(os.path.join(D, "mls", lang, f"{part}-*.parquet")))[:1] for i in range(10000)]
    raise ValueError(source)

_pq = {}
def read_any(f):
    import aug
    if "#" in f and f.split("#")[0].endswith(".parquet"):
        p, i = f.split("#")
        if p not in _pq:
            import pyarrow.parquet as pq
            _pq.clear()
            _pq[p] = pq.read_table(p, columns=["audio"]).column("audio")
        col = _pq[p]
        if int(i) >= len(col):
            return None
        import soundfile as sf
        a, sr = sf.read(io.BytesIO(col[int(i)].as_py()["bytes"]), dtype="float32")
        return a if sr == SR else None
    return aug._read(f)

def build_long(args):
    """~60 s of a source, through a random mic and background."""
    import aug
    files, seed, augment = args
    _seed(seed)
    parts = [np.zeros(int(LEAD * SR), np.float32)]
    for f in files:
        a = read_any(f)
        if a is None or len(a) == 0:
            continue
        if np.abs(a).max() > 0:
            a = a / np.abs(a).max() * 0.9
        parts.append(a.astype(np.float32))
        if random.random() < 0.3:  # pauses between sentences, as in conversation
            parts.append(np.zeros(int(random.uniform(0.2, 1.5) * SR), np.float32))
    x = np.concatenate(parts)
    if augment:
        x = aug.mic(x, snr=(5, 35), clean=0.25)
    else:
        x = x / (np.abs(x).max() + 1e-6) * random.uniform(0.1, 0.9)
    n = len(x) // HOP * HOP
    return (x[:n] * 32767).astype(np.int16)

def long(source, out, hours=None, seed=0):
    import feat
    files = source_files(source)
    rng = random.Random(seed)
    if not source.startswith("mls_"):
        rng.shuffle(files)
    # Group files into ~60 s pieces (MUSAN files are long already: one each).
    per = 1 if source.startswith("musan_") else (5 if source.startswith("mls_") else 4)
    groups = [files[i:i + per] for i in range(0, len(files), per)]
    augment = not source.endswith("eval") and not source.endswith("_test")
    jobs = [(g, seed * 100000 + j, augment) for j, g in enumerate(groups)]
    rows, starts, total, secs, t0 = [], [], 0, 0.0, time.time()
    limit = hours * 3600 if hours else float("inf")
    with mp.Pool(int(os.environ.get("FEATS_CPU", "6"))) as pool:
        batch = []
        def flush():
            nonlocal total, secs
            audio = np.concatenate(batch)
            emb = feat.stream_embed(audio)
            o = 0
            for a in batch:
                k = len(a) // HOP
                skip = int(np.ceil((LEAD - 0.4 + WIN_SECS) * SR / HOP))  # rows before a clean window
                if k > skip + WIN:
                    starts.append(total)
                    rows.append(emb[o:o + k].astype(np.float16))
                    total += k
                    secs += (k - skip) * HOP / SR
                o += k
            batch.clear()
        for i, a in enumerate(pool.imap(build_long, jobs, chunksize=4), 1):
            batch.append(a)
            if sum(len(b) for b in batch) > 20 * 60 * SR:
                flush()
                el = time.time() - t0
                print(f"{source}: {secs / 3600:.1f} h of audio ({secs / el:.0f}x realtime) after {el / 60:.1f} min", flush=True)
                if secs >= limit:
                    pool.terminate()
                    break
        if batch and secs < limit:
            flush()
    np.savez(out, rows=np.concatenate(rows), starts=np.array(starts), lead=int(np.ceil((LEAD - 0.4 + WIN_SECS) * SR / HOP)))
    print(f"{out}: {secs / 3600:.2f} h, {total} rows, {time.time() - t0:.0f} s", flush=True)

if __name__ == "__main__":
    from gen import low_priority
    low_priority()
    if sys.argv[1] == "scenes":
        scenes(sys.argv[2], sys.argv[3], int(sys.argv[4]), sys.argv[5], int(sys.argv[6]) if len(sys.argv) > 6 else 0)
    elif sys.argv[1] == "long":
        long(sys.argv[2], sys.argv[3], float(sys.argv[4]) if len(sys.argv) > 4 else None, int(sys.argv[5]) if len(sys.argv) > 5 else 0)
