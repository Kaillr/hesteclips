"""MUSAN files whose own licence allows commercial use without share-alike
(CC BY / CC0 / public domain), from each folder's LICENSE blocks."""
import glob, os, re

def classify(text):
    t = text.lower()
    if re.search(r"non-?commercial|\bnc\b|by-nc|noncommercial|no deriv|\bnd\b|by-nd", t):
        return "nc"
    if re.search(r"share ?-?alike|by-sa|\bsa\b", t):
        return "sa"
    if re.search(r"public domain|cc0|attribution|cc by|cc-by|creative commons", t):
        return "ok"
    return "?"

def ok_files(root):
    keep, why = set(), {}
    for lic in glob.glob(os.path.join(root, "**", "LICENSE"), recursive=True):
        d = os.path.dirname(lic)
        text = open(lic, encoding="utf-8", errors="replace").read()
        wavs = {os.path.splitext(os.path.basename(w))[0] for w in glob.glob(os.path.join(d, "*.wav"))}
        blocks = re.split(r"\n=+\n", text)
        if len(blocks) == 1 and not re.search(r"(music|noise|speech)-[\w-]+-\d{4}", text):
            c = classify(text)  # one licence for the folder
            for w in wavs:
                why[w] = c
            continue
        # A licence block with no files of its own (rfm) covers the folder.
        head = classify(blocks[0]) if not re.search(r"(music|noise|speech)-[\w-]+-\d{4}", blocks[0]) else "?"
        for b in blocks:
            ids = set(re.findall(r"((?:music|noise|speech)-[\w-]+?-\d{4})", b))
            c = classify(b)
            c = head if c == "?" else c
            for i in ids:
                why[i] = c
        for w in wavs:
            why.setdefault(w, "?")
    for w, c in why.items():
        if c == "ok":
            keep.add(w)
    return keep, why

if __name__ == "__main__":
    import collections
    keep, why = ok_files(os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "dl", "musan"))
    c = collections.Counter((w.rsplit("-", 1)[0], v) for w, v in why.items())
    for k in sorted(c): print(k, c[k])
