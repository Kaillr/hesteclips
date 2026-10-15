"""What phonemes a take really has (wav2vec2 trained on espeak phonemes),
and how far that is from the phrase. For checking generated takes only."""
import json, os
import numpy as np

_m = None
DEV = os.environ.get("PHON_DEVICE", "cuda")
def recognizer():
    global _m
    if _m is None:
        import torch
        from huggingface_hub import hf_hub_download
        from transformers import Wav2Vec2FeatureExtractor, Wav2Vec2ForCTC
        name = "facebook/wav2vec2-lv-60-espeak-cv-ft"
        vocab = json.load(open(hf_hub_download(name, "vocab.json"), encoding="utf-8"))
        inv = {v: k for k, v in vocab.items()}
        fe = Wav2Vec2FeatureExtractor.from_pretrained(name)
        model = Wav2Vec2ForCTC.from_pretrained(name).to(DEV).eval()
        _m = (fe, model, inv)
    return _m

def phonemes(audio_int16_list, batch=24):
    """Batched: sorted by length, padded, with an attention mask."""
    import torch
    fe, model, inv = recognizer()
    order = sorted(range(len(audio_int16_list)), key=lambda i: len(audio_int16_list[i]))
    out = [None] * len(order)
    for b in range(0, len(order), batch):
        idx = order[b:b + batch]
        xs = [audio_int16_list[i].astype(np.float32) / 32768 for i in idx]
        enc = fe(xs, sampling_rate=16000, return_tensors="pt", padding=True, return_attention_mask=True)
        with torch.no_grad():
            logits = model(enc.input_values.to(DEV), attention_mask=enc.attention_mask.to(DEV)).logits
        lens = model._get_feat_extract_output_lengths(enc.attention_mask.sum(-1)).tolist()
        ids = logits.argmax(-1).cpu().numpy()
        for i, row, n in zip(idx, ids, lens):
            seq, last = [], -1
            for t in row[:n]:
                if t != last and inv[t] not in ("<pad>", "<s>", "</s>", "<unk>"):
                    seq.append(inv[t])
                last = t
            out[i] = [q for q in seq if q != "|"]
    return out

def phonemes_one_by_one(audio_int16_list):
    import torch
    fe, model, inv = recognizer()
    out = []
    for a in audio_int16_list:
        x = fe(a.astype(np.float32) / 32768, sampling_rate=16000, return_tensors="pt").input_values.to(DEV)
        with torch.no_grad():
            ids = model(x).logits.argmax(-1)[0].cpu().numpy()
        seq, last = [], -1
        for i in ids:
            if i != last and inv[i] not in ("<pad>", "<s>", "</s>", "<unk>"):
                seq.append(inv[i])
            last = i
        out.append([p for p in seq if p != "|"])
    return out

def norm(p):
    """Collapse what accents and recognizers swap freely."""
    m = {"ɪ": "i", "iː": "i", "e": "eɪ", "ɛ": "eɪ", "eː": "eɪ", "d": "t", "z": "s", "ð": "t", "θ": "t", "ɾ": "t", "ʔ": "t",
         "æ": "a", "a": "a", "ɑ": "a", "ʌ": "a", "ɐ": "a", "ə": "a", "ɑː": "a", "aɪ": "a", "kʰ": "k", "ɡ": "k", "g": "k", "x": "h"}
    return [m.get(x, x) for x in p]

TARGET = norm(["h", "eɪ", "s", "t", "k", "l", "ɪ", "p", "ð", "æ", "t"])

def distance(a, b):
    d = np.arange(len(b) + 1)
    for i, x in enumerate(a, 1):
        prev, d[0] = d[0], i
        for j, y in enumerate(b, 1):
            prev, d[j] = d[j], min(d[j] + 1, d[j - 1] + 1, prev + (x != y))
    return d[-1]

def phrase_error(seq):
    """Best match of the phrase anywhere in seq, as a fraction of its length."""
    s = norm(seq)
    best = len(TARGET)
    n = len(TARGET)
    for i in range(len(s)):
        for L in range(max(1, n - 4), n + 5):
            best = min(best, distance(s[i:i + L], TARGET))
    return best / n
