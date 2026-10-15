"""What the voices say: "HasteClip that" in many realistic ways, the talk
around it, and near misses that must not save a clip.

Phonemes are espeak-style IPA (both engines take it). English "haste" is
/heɪst/; accent and fast-speech forms are weighted lower so they widen what
counts without becoming the norm.
"""
import random

# (form, weight). Accent forms come mostly from the accent voices (ACCENT=True).
HASTE = [("hˈeɪst", 12), ("hˈeɪs", 1), ("hˌeɪst", 2)]
HASTE_ACCENT = [("hˈeːst", 3), ("hˈɛɪst", 2), ("hˈeist", 3), ("ˈeɪst", 1), ("xˈeɪst", 1), ("hˈejst", 2)]
CLIP = [("klˈɪp", 10), ("klɪp", 5), ("klˌɪp", 3), ("klˈɪ", 1)]
CLIP_ACCENT = [("klˈip", 4), ("klˈɪp", 4), ("kʰlˈɪp", 1)]
THAT = [("ðˈæt", 10), ("ðæt", 4), ("ðˈæʔ", 2), ("ðˈɛt", 1), ("ðˈat", 1), ("dˈæt", 1)]
THAT_ACCENT = [("dˈæt", 4), ("dˈɛt", 3), ("zˈæt", 2), ("ðˈɛt", 2), ("tˈæt", 1), ("dˈat", 2)]

def pick(options):
    forms, weights = zip(*options)
    return random.choices(forms, weights)[0]

def part(name, accent):
    base = {"haste": HASTE, "clip": CLIP, "that": THAT}[name]
    acc = {"haste": HASTE_ACCENT, "clip": CLIP_ACCENT, "that": THAT_ACCENT}[name]
    return pick(acc) if accent and random.random() < 0.5 else pick(base)

def phrase(accent=False):
    """The phrase as IPA. One word "HasteClip" (often), three words, or run
    together; now and then a short comma pause before "that"."""
    h, c, t = part("haste", accent), part("clip", accent), part("that", accent)
    r = random.random()
    if r < 0.40:
        s = f"{h}{c.replace('ˈ', 'ˌ')} {t}"
    elif r < 0.80:
        s = f"{h} {c} {t}"
    elif r < 0.90:
        s = f"{h}{c.replace('ˈ', '')}{t.replace('ˈ', '')}"  # fast, run together
    else:
        s = f"{h}{c.replace('ˈ', 'ˌ')}, {t}"
    return s

# Talk said around the phrase (English text, phonemized by the engine).
BEFORE = [
    "oh my god", "no way", "that was insane", "yo", "dude", "bro", "let's go", "wait wait wait", "did you see that",
    "okay", "holy", "nice", "what", "yes", "oh", "that was so clean", "guys", "bro what", "no shot", "are you kidding me",
    "that was crazy", "one tap", "ace", "easy", "insane", "that was sick", "hold on", "okay okay", "oh wow",
    "I can't believe that", "clutch", "he's dead", "first try", "gg", "let's goo", "what a shot", "that's huge",
    "unbelievable", "and that's the game", "triple kill", "look at that", "okay that was good", "yeah", "wow",
]
AFTER = [
    "that was sick", "right now", "please", "lol", "oh my god", "that was insane", "bro", "for real", "quick",
    "now", "yes", "haha", "I need that one", "send it", "nice", "that was clean", "for the montage", "okay",
]

def with_context(core, ctx_before=0.35, ctx_after=0.25):
    """core: ('ipa', s). Returns a list of segments; the phrase is marked."""
    segs = []
    if random.random() < ctx_before:
        segs.append(("text", random.choice(BEFORE) + random.choice([",", "!", ".", ""])))
    segs.append(core)
    if random.random() < 0.2:  # said twice
        segs.append(("ipa", ","))
        segs.append(("phrase", phrase()))
    if random.random() < ctx_after:
        segs.append(("text", random.choice([", ", "! ", ". "]) + random.choice(AFTER)))
    return segs

def positive(accent=False):
    end = random.choice(["", "", ".", "!", "!", "?", ","])
    return with_context(("phrase", phrase(accent) + end))

# --- near misses: none of these may save a clip ---

def _swap(first=None, second=None, third=None, accent=False):
    h = first or part("haste", accent)
    c = second or part("clip", accent)
    t = third if third is not None else part("that", accent)
    return f"{h} {c} {t}".strip()

# Sound-alikes of "haste" before "clip that".
# (Not "hɛst", "hɪst", "haɪst", "hæst": accented speakers say the phrase so.)
HASTE_LIKE = ["pˈeɪst", "tˈeɪst", "wˈeɪst", "bˈeɪs", "fˈeɪs", "kˈeɪs", "tʃˈeɪs", "ɹˈeɪs", "plˈeɪs", "hˈeɪ", "hˈeɪt",
              "ˈeɪt", "sˈeɪ", "stˈeɪ", "hˈeɪsti", "dʒˈʌst", "lˈæst", "fˈæst", "mˈeɪk", "tˈeɪk",
              "bˈɛst", "nˈɛkst", "lˈɛts", "hˈɛlθ", "hˈoʊst", "dʒˈɛst"]
# Sound-alikes of "clip" between "haste" and "that".
# (Not "slip", "skip": "haste clip" said fast sounds like them.)
CLIP_LIKE = ["flˈɪp", "klˈɪk", "tʃˈɪp", "ʃˈɪp", "zˈɪp", "klˈæp", "kˈʌp", "klˈiːn", "klˈɪps", "lˈɪp", "ɡɹˈɪp", "kˈɪt"]
# Other words where "that" goes.
THAT_LIKE = ["ðˈɪs", "ˈɪt", "ðə", "bˈæk", "hˈæt", "ðˈɛm", "ðˈɛɹ", "ˈæt", "kˈæt", "ðˈoʊz", "ðˈiːz", "ˈʌp", "nˈaʊ", ""]

def near_ipa(accent=False):
    r = random.random()
    if r < 0.30:
        return _swap(first=random.choice(HASTE_LIKE), accent=accent)
    if r < 0.42:
        return _swap(second=random.choice(CLIP_LIKE), accent=accent)
    if r < 0.62:
        t = random.choice(THAT_LIKE)
        return _swap(third=t, accent=accent)
    if r < 0.74:  # a part alone
        return random.choice([part("haste", accent), _swap(third=""), f"{part('clip', accent)} {part('that', accent)}"])
    if r < 0.82:  # "clip that" with something else first
        return f"{random.choice(['hˈeɪ', 'dʒˈʌst', 'ˈoʊ', 'jˈoʊ', 'plˈiːz', 'sˈʌmwʌn', 'pɹˈɑːbli', 'kˈæn juː'])} {part('clip', accent)} {part('that', accent)}"
    if r < 0.90:  # wrong order
        return random.choice([f"{part('clip', accent)} {part('that', accent)} {part('haste', accent)}",
                              f"{part('that', accent)} {part('haste', accent)} {part('clip', accent)}",
                              f"{part('clip', accent)} {part('haste', accent)} {part('that', accent)}"])
    return f"{part('haste', accent)}{part('clip', accent).replace('ˈ', 'ˌ')}"  # "HasteClip" alone

NEAR_TEXT = [
    "clip that", "someone clip that", "please clip that", "did you clip that", "clip it", "clip that please", "I clipped that",
    "clipping that", "nice clip", "that clip was crazy", "hasta la vista", "hasta luego", "hesitate", "hey stop", "hey Steve clip that",
    "they stay clipped at", "paste clip that", "taste clip that", "waste clip that", "face clip that", "base clip that",
    "case clip that", "chase clip that", "race clip that", "place clip that", "make haste", "in haste", "hasten", "hasty",
    "haste makes waste", "the paste clipped", "I use HasteClip", "HasteClip is open", "is HasteClip recording", "open HasteClip",
    "HasteClip the last one", "HasteClip this", "HasteClip it", "HasteClip is great", "HasteClip app", "close HasteClip",
    "my HasteClip settings", "hastily clip that", "a stick clip that", "hey just clip that", "just clip that", "haste flip that",
    "haste click that", "hey clip that", "eight clip that", "the best clip", "last clip",
    "fast clip", "Chase, clip that later", "his clip that", "this clip", "these clips", "hey stick", "heist", "the heist",
    "hey Siri", "okay Google", "hey Alexa", "pay stub", "hay stack", "haystack", "taste test", "cut that", "keep that", "save that",
    "record that", "hey Steve", "Hastings", "haste", "clip", "that", "nice shot", "good game", "one more game", "reload",
    "behind you", "heal me", "I'm going in", "watch out", "get him", "he's one shot", "rotate", "push B", "I'm lagging",
    "what was that", "hey guys", "that's crazy", "wait what", "come here", "let's go", "oh my god", "no way",
]

def negative(accent=False):
    if random.random() < 0.6:
        core = ("ipa", near_ipa(accent) + random.choice(["", "", ".", "!", "?"]))
    else:
        core = ("text", random.choice(NEAR_TEXT) + random.choice(["", ".", "!", "?"]))
    segs = []
    if random.random() < 0.3:
        segs.append(("text", random.choice(BEFORE) + random.choice([",", "!", "."])))
    segs.append(core)
    if random.random() < 0.2:
        segs.append(("text", random.choice([", ", "! ", ". "]) + random.choice(AFTER)))
    return segs
