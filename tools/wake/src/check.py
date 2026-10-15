"""Drop takes that don't say what they should: positives whose marked
phrase the phoneme recognizer doesn't hear as "HasteClip that" (error >
0.5), and near misses that sound exactly like it (error <= 0.1).

usage: python check.py <in.npz> <pos|neg> <out.npz>"""
import json, os, sys, time
import numpy as np
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from gen import load
import phon

def wait_for_gpu(need_mib=2500):
    """One check at a time (a lock file), and only once the GPU has room:
    two recognizers next to the Kokoro workers didn't fit in 8 GB."""
    import msvcrt, subprocess
    lock = open(os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "logs", "check.lock"), "a+")
    t0 = time.time()
    while True:
        try:
            msvcrt.locking(lock.fileno(), msvcrt.LK_NBLCK, 1)
            break
        except OSError:
            time.sleep(10)
    while True:
        used, total = map(int, subprocess.run(["nvidia-smi", "--query-gpu=memory.used,memory.total", "--format=csv,noheader,nounits"],
                                              capture_output=True, text=True).stdout.split(","))
        if total - used >= need_mib:
            break
        time.sleep(15)
    print(f"waited {time.time() - t0:.0f} s for the GPU", flush=True)
    return lock

def main(src, kind, dst):
    _lock = wait_for_gpu()
    clips, spans = load(src)
    meta = open(src + ".jsonl", encoding="utf-8").read().splitlines()
    keep, errs, t0 = [], [], time.time()
    B = 64
    # Batched recognition, if it agrees with clip-by-clip on the first ones.
    def cuts_of(idx):
        if kind == "pos":
            return [clips[j][max(spans[j][0][0] - 800, 0):spans[j][0][1] + 800] for j in idx]
        return [clips[j] for j in idx]
    recog = phon.phonemes
    try:
        test = cuts_of(range(min(32, len(clips))))
        same = sum(a == b for a, b in zip(phon.phonemes_one_by_one(test), phon.phonemes(test)))
        if same < 0.9 * len(test):
            recog = phon.phonemes_one_by_one
        print(f"batched recognition agrees on {same}/{len(test)}: using {recog.__name__}", flush=True)
    except Exception as e:
        recog = phon.phonemes_one_by_one
        print("batched recognition failed, clip by clip:", e, flush=True)
    for i in range(0, len(clips), B):
        idx = range(i, min(i + B, len(clips)))
        for j, s in zip(idx, recog(cuts_of(idx))):
            e = phon.phrase_error(s)
            errs.append(e)
            if (kind == "pos" and e <= 0.5) or (kind == "neg" and e > 0.1):
                keep.append(j)
        if (i // B) % 20 == 0:
            print(f"checked {min(i + B, len(clips))}/{len(clips)}, kept {len(keep)} ({time.time() - t0:.0f} s)", flush=True)
    sel = [clips[j] for j in keep]
    np.savez(dst, audio=np.concatenate(sel), lens=np.array([len(c) for c in sel]), spans=spans[keep])
    with open(dst + ".jsonl", "w", encoding="utf-8") as f:
        for j in keep:
            m = json.loads(meta[j])
            m["err"] = round(errs[j], 3)
            f.write(json.dumps(m, ensure_ascii=False) + "\n")
    errs = np.array(errs)
    print(f"{kind}: kept {len(keep)}/{len(clips)}; error quartiles {np.percentile(errs, [25, 50, 75]).round(2)}", flush=True)

if __name__ == "__main__":
    main(*sys.argv[1:4])
