"""What the voices say: "hashtag HesteClip that" in the ways people really
say it, the talk around it, and near misses that must not save a clip.

Phonemes are espeak-style IPA (both engines take it). "Heste" is Norwegian
(/hˈɛstə/), the rest English. The forms follow how the phrase is really
said in the users' own takes (tools/wake/README.md): fast, the "h" of both
words often gone, "hashtag" and "heste" run together ("aʃaɡɛsklɪpta"), and
"that" cut to "ta"/"da". Accent forms come mostly from the accent voices
(Norwegian ones first), fast forms from fast takes.
"""
import random

# (form, weight)
HASHTAG = [("hˈæʃtæɡ", 12), ("hˈæʃtæk", 2), ("hˈæʃtˌæɡ", 2), ("hæʃtˈæɡ", 1)]
HASHTAG_ACCENT = [("hˈæʃtæɡ", 4), ("hˈæʃtɛɡ", 3), ("hˈɛʃtɛɡ", 2), ("hˈɑʃtɑɡ", 2), ("hˈæʃtæk", 2), ("hˈaʃtaɡ", 2), ("ˈæʃtæɡ", 1)]
HASHTAG_FAST = [("ˈæʃtæɡ", 4), ("hˈæʃæɡ", 3), ("ˈæʃæɡ", 5), ("ˈæʃɛɡ", 3), ("hˈæʃɛɡ", 2), ("ˈæʃtə", 1), ("hˈæʒdæɡ", 1), ("ˈaʃaɡ", 2), ("ʃtˈæɡ", 1)]
HESTE = [("hˈɛstə", 12), ("hˈɛstɪ", 4), ("hˈɛste", 2), ("hˈɛsti", 1), ("hˈɛstɛ", 1), ("hˈæstə", 1)]
HESTE_ACCENT = [("hˈɛstə", 8), ("hˈɛste", 3), ("hˈɛstɛ", 2), ("hˈɛːstə", 1)]
HESTE_FAST = [("ˈɛstə", 4), ("ˈɛs", 3), ("ˈɛst", 3), ("ˈɛstɪ", 3), ("hˈɛs", 2), ("hˈɛst", 2), ("ˈɛsə", 1), ("ɛsɪ", 1)]
CLIP = [("klˈɪp", 10), ("klɪp", 5), ("klˌɪp", 3)]
CLIP_ACCENT = [("klˈip", 4), ("klˈɪp", 4), ("klˈɪpː", 1)]
CLIP_FAST = [("klɪ", 3), ("klˈɪ", 2), ("kɪp", 2), ("klɪp", 3), ("kˈɪp", 1), ("lɪp", 1)]
THAT = [("ðˈæt", 10), ("ðæt", 4), ("dˈæt", 2), ("ðˈæʔ", 2), ("ðˈɛt", 1)]
THAT_ACCENT = [("dˈæt", 4), ("dˈɛt", 3), ("dˈæ", 2), ("tˈæ", 1), ("dˈɑt", 1), ("ðˈæt", 2)]
THAT_FAST = [("ðæ", 3), ("dæ", 3), ("tæ", 2), ("dɑ", 1), ("tɑ", 2), ("dæt", 2), ("ðæt", 1)]

FORMS = {
    "hashtag": (HASHTAG, HASHTAG_ACCENT, HASHTAG_FAST),
    "heste": (HESTE, HESTE_ACCENT, HESTE_FAST),
    "clip": (CLIP, CLIP_ACCENT, CLIP_FAST),
    "that": (THAT, THAT_ACCENT, THAT_FAST),
}

def pick(options):
    forms, weights = zip(*options)
    return random.choices(forms, weights)[0]

def part(name, accent=False, fast=False):
    base, acc, quick = FORMS[name]
    if fast and random.random() < 0.55:
        return pick(quick)
    return pick(acc) if accent and random.random() < 0.5 else pick(base)

def unstress(s):
    return s.replace("ˈ", "ˌ")

def phrase(accent=False, fast=False):
    """The phrase as IPA. "HesteClip" one word; "hashtag" apart, or run into
    it (as said fast); now and then all of it run together, or a short
    pause after "hashtag"."""
    h, e, c, t = (part(n, accent, fast) for n in ("hashtag", "heste", "clip", "that"))
    r = random.random()
    if fast and random.random() < 0.5:
        r = 0.40 + r * 0.40  # fast: run together more
    if r < 0.40:
        return f"{h} {e}{unstress(c)} {t}"
    if r < 0.62:
        return f"{h}{unstress(e)}{unstress(c)} {t}"  # "aʃaɡɛsklɪp ta"
    if r < 0.80:
        return f"{h}{unstress(e)}{unstress(c)}{t.replace('ˈ', '')}"  # all in one go
    if r < 0.92:
        return f"{h} {e} {c} {t}"
    return f"{h}, {e}{unstress(c)} {t}"

# The same in Norwegian spelling, for the Norwegian voices to read with
# their own phonemes: a Norwegian accent as Norwegians have it.
PHRASE_NO = ["hæsjtæg hesteklipp dæt", "hæsjtæg hesteklipp dett", "hæsjtag hesteklipp dæt", "hæsjtæg hesteklipp dæ",
             "æsjtæg esteklipp dæt", "hæsjtæghesteklipp dæt", "hæsjtæg heste klipp dæt", "hæsjtægg hestklipp dæt",
             "æsjtæghesteklipp dæ", "hæsjtæg hesteklipp tætt", "hæsjtæk hesteklipp dæt", "hasjtag hesteklipp dett"]

# Talk said around the phrase.
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
# Norwegian talk, for the Norwegian voices (read with Norwegian phonemes).
BEFORE_NO = ["herregud", "nei", "ja", "jøss", "se der", "kom igjen", "det var sykt", "så bra", "hæ", "neimen",
             "så du det", "vent vent", "yes", "lett", "nå", "okei", "for et skudd", "helt rått", "hallo", "wow"]
AFTER_NO = ["nå", "fort", "takk", "det var sykt", "ass", "hehe", "ja", "kjapt", "please", "den må vi ha"]

def with_context(core, norsk=False, ctx_before=0.35, ctx_after=0.25):
    """core: (kind, s). Returns segments; the phrase is marked."""
    segs = []
    before, after, kind = (BEFORE_NO, AFTER_NO, "text_no") if norsk else (BEFORE, AFTER, "text")
    if random.random() < ctx_before:
        segs.append((kind, random.choice(before) + random.choice([",", "!", ".", ""])))
    segs.append(core)
    if random.random() < 0.15:  # said twice
        segs.append(("ipa", ","))
        segs.append(core)
    if random.random() < ctx_after:
        segs.append((kind, random.choice([", ", "! ", ". "]) + random.choice(after)))
    return segs

def positive(accent=False, fast=False, norsk=False):
    """norsk: a Norwegian voice: half the time the phrase in Norwegian
    spelling, read with its own phonemes."""
    end = random.choice(["", "", ".", "!", "!", "?", ","])
    if norsk and random.random() < 0.5:
        return with_context(("phrase_no", random.choice(PHRASE_NO) + end), norsk=True)
    return with_context(("phrase", phrase(accent, fast) + end), norsk=norsk and random.random() < 0.5)

# --- near misses: none of these may save a clip ---

def _join(*parts):
    return " ".join(p for p in parts if p)

# Sound-alikes of "heste" between "hashtag" and "clip that". (Not "hest",
# "hɛs", "hæst", "heist": said fast or with an accent, the phrase is so.)
HESTE_LIKE = ["bˈɛst", "tˈɛst", "fˈɛst", "ɡˈɛst", "nˈɛkst", "wˈɛst", "tʃˈɛstɚ", "hˈɛstɚ", "lˈæst", "fˈæst", "mˈʌst",
              "tˈeɪst", "pˈeɪst", "dʒˈʌst", "hˈɛlθ", "hˈoʊst", "fˈɛstɪvəl", "ɹˈɛst", "hˈɛlp", "hˈæpi"]
# Sound-alikes of "clip" after "hashtag heste".
CLIP_LIKE = ["flˈɪp", "klˈɪk", "tʃˈɪp", "ʃˈɪp", "zˈɪp", "klˈæp", "kˈʌp", "klˈiːn", "klˈɪps", "ɡɹˈɪp", "kˈɪt", "slˈɪp", "dɹˈɪp"]
# Other words where "that" goes ("" = the phrase without "that").
THAT_LIKE = ["ðˈɪs", "ˈɪt", "ðə", "bˈæk", "hˈæt", "ðˈɛm", "ðˈɛɹ", "ˈæt", "kˈæt", "nˈaʊ", "ˈʌp", "", "", ""]
# Sound-alikes of "hashtag" before "hesteclip that".
HASHTAG_LIKE = ["hˈæz ðə", "hˈæz tə", "hˈæʃ", "hˈʌʃ", "kˈæʃ tˈæɡ", "hˈæt stˈæk", "hˈɑːstə", "hˈeɪ stˈæɡ", "ɹˈæɡ tˈæɡ",
                "bˈæk tˈæk", "hˈæʃ bɹˈaʊn"]

def near_ipa(accent=False, fast=False):
    p = lambda n: part(n, accent, fast)
    r = random.random()
    if r < 0.18:  # another "heste"
        return f"{p('hashtag')} {random.choice(HESTE_LIKE)}{unstress(p('clip'))} {p('that')}"
    if r < 0.28:  # another "clip"
        return f"{p('hashtag')} {p('heste')}{unstress(random.choice(CLIP_LIKE))} {p('that')}"
    if r < 0.40:  # another (or no) "that"
        return _join(p("hashtag"), p("heste") + unstress(p("clip")), random.choice(THAT_LIKE))
    if r < 0.48:  # another "hashtag"
        return f"{random.choice(HASHTAG_LIKE)} {p('heste')}{unstress(p('clip'))} {p('that')}"
    if r < 0.70:  # a part missing
        return random.choice([
            p("hashtag"),
            f"{p('hashtag')} {p('heste')}",
            f"{p('heste')}{unstress(p('clip'))}",
            f"{p('heste')}{unstress(p('clip'))} {p('that')}",  # no "hashtag"
            f"{p('hashtag')} {p('clip')} {p('that')}",  # no "heste"
            f"{p('hashtag')} {p('that')}",
            f"{p('clip')} {p('that')}",
            f"{p('hashtag')} {p('hashtag')}",
        ])
    if r < 0.80:  # wrong order
        return random.choice([
            f"{p('heste')}{unstress(p('clip'))} {p('hashtag')} {p('that')}",
            f"{p('that')} {p('hashtag')} {p('heste')}{unstress(p('clip'))}",
            f"{p('clip')} {p('that')} {p('hashtag')}",
            f"{p('hashtag')} {p('that')} {p('heste')}{unstress(p('clip'))}",
        ])
    if r < 0.90:  # "clip that" after other words
        return f"{random.choice(['hˈeɪ', 'dʒˈʌst', 'ˈoʊ', 'jˈoʊ', 'plˈiːz', 'sˈʌmwʌn', 'pɹˈɑːbli', 'kˈæn juː', 'hˈeɪ stˈiːv'])} {p('clip')} {p('that')}"
    return f"{p('hashtag')} {random.choice(['ɡˈeɪmɪŋ', 'blˈɛsɪd', 'wˈɪnɪŋ', 'ɡˈoʊlz', 'nˈoʊ fˈɪltɚ', 'θɹˈoʊbæk', 'mˈʌndeɪ'])}"

NEAR_TEXT = [
    "clip that", "someone clip that", "please clip that", "did you clip that", "clip it", "clip that please", "I clipped that",
    "clipping that", "nice clip", "that clip was crazy", "hashtag", "hashtag blessed", "hashtag gaming", "hashtag winning",
    "check the hashtag on twitter", "what's the hashtag", "use the hashtag", "hashtag clip that", "hashtag clips",
    "hashtag best clip", "hashtag best clip ever", "the best clip", "best clip that", "hashtag test clip", "hashtag festival",
    "has the clip that we made been saved yet", "has the clip been saved", "has to clip that", "hashtag blessed, clip it later",
    "stag party this weekend", "hey Steve clip that", "Chester clip that", "Hester clip that", "hesitate", "hasta la vista",
    "hash browns", "cash tag", "hat stack", "the hashtag", "hashtag that", "that hashtag", "hashtag hashtag",
    "HesteClips", "open HesteClips", "is HesteClips recording", "HesteClips is open", "close HesteClips", "I use HesteClips",
    "HesteClip it", "HesteClip this", "HesteClip that", "Hesteclip that one", "hashtag HesteClip", "hashtag HesteClips",
    "hashtag HesteClip is great", "my HesteClips settings", "festival clip", "test clip", "the last clip", "next clip",
    "HasteClip that", "haste clip that", "paste clip that", "taste clip that", "this clip", "these clips",
    "nice shot", "good game", "one more game", "reload", "behind you", "heal me", "I'm going in", "watch out", "get him",
    "he's one shot", "rotate", "push B", "I'm lagging", "what was that", "hey guys", "that's crazy", "wait what", "come here",
    "let's go", "oh my god", "no way", "hey Siri", "okay Google", "hey Alexa", "save that", "record that", "keep that",
]
NEAR_TEXT_NO = [
    "hest", "heste", "hester", "hestene", "hesteklipp", "heste klipp", "hesteklipp da", "bare hest klipp da", "bare heste klipp",
    "klipp det", "klipp det da", "har du klippet det", "jeg klipper det", "klipp av", "klipp", "hashtag", "hashtag hest",
    "hashtag hester", "hashtag klipp det", "kast det", "hast deg", "neste klipp", "beste klipp", "beste klippet",
    "hestesko", "festen", "testen", "det var gøy", "dette er bra", "hæ", "kom igjen", "ta det", "har det", "hva skjer",
    "skyt han", "bak deg", "jeg dør", "han er lav", "pass på", "nei nei nei", "så dårlig", "vi vant", "god runde",
    "en gang til", "herregud", "hva var det", "kan du klippe det", "noen må klippe det", "lagre det", "ta opp det",
]

def negative(accent=False, fast=False, norsk=False):
    r = random.random()
    if norsk and r < 0.45:
        core = ("text_no", random.choice(NEAR_TEXT_NO) + random.choice(["", ".", "!", "?"]))
    elif r < 0.6:
        core = ("ipa", near_ipa(accent, fast) + random.choice(["", "", ".", "!", "?"]))
    else:
        core = ("text", random.choice(NEAR_TEXT) + random.choice(["", ".", "!", "?"]))
    segs = []
    before, after, kind = (BEFORE_NO, AFTER_NO, "text_no") if norsk and random.random() < 0.5 else (BEFORE, AFTER, "text")
    # Nothing after it that starts with "that" (or Norwegian "det"): after
    # "hashtag HesteClip" that would be the phrase itself.
    after = [a for a in after if not a.startswith(("that", "det"))]
    if random.random() < 0.3:
        segs.append((kind, random.choice(before) + random.choice([",", "!", "."])))
    segs.append(core)
    if random.random() < 0.2:
        segs.append((kind, random.choice([", ", "! ", ". "]) + random.choice(after)))
    return segs
