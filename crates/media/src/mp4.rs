//! What [`crate::probe`] needs from an MP4 or QuickTime file, read in the app
//! from its index (`moov`), as ffprobe tells it: under a millisecond, where
//! starting ffprobe took ~60 ms a clip (more on Windows). Anything else (MKV,
//! WebM, AVI, a fragmented or unfinished file) is ffprobe's still.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{Context, Result, bail};

/// A track's kind, from its handler.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Kind {
    Video,
    Audio,
    Other,
}

#[derive(Debug, Clone)]
pub(crate) struct Track {
    pub kind: Kind,
    /// Its name: the `udta/name` box if there is one, else the handler's.
    pub name: Option<String>,
    /// Frames and their bytes, over this many seconds.
    pub samples: u64,
    pub bytes: u64,
    pub seconds: f64,
    /// When it starts and ends on the file's timeline, in seconds, as
    /// ffprobe has a stream: where its edit list starts it, and then its
    /// media's length (at most what the edits show of it).
    pub start: f64,
    pub end: f64,
    /// A video track's size (as coded, from its sample entry).
    pub width: u32,
    pub height: u32,
    /// A video track's colour matrix (H.273: 1 BT.709, 5/6 BT.601), from its
    /// `colr` box or else its H.264 stream's SPS.
    pub matrix: Option<u8>,
}

#[derive(Debug, Clone)]
pub(crate) struct Movie {
    pub seconds: f64,
    pub tracks: Vec<Track>,
    /// The file's comment (ours: `hasteclip:mix=…`).
    pub comment: Option<String>,
}

/// Read `path`'s index. Errs on anything that isn't a plain MP4 or MOV.
pub(crate) fn read(path: &Path) -> Result<Movie> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    let moov = find_top(&mut f, len, b"moov")?.context("no moov box")?;
    let mvhd = child(&moov, b"mvhd").context("no mvhd")?;
    let (timescale, duration) = times(mvhd).context("a short mvhd")?;
    let mut tracks = Vec::new();
    for trak in children(&moov, b"trak") {
        tracks.push(track(trak, timescale)?);
    }
    // From the first stream's start to the last one's end, as ffprobe's
    // length: the movie's own length can run past it (ffmpeg's copies
    // starting on a keyframe edit their streams' starts in, and keep the
    // whole length after them: 3.24 s where the streams end by 3.14).
    let end = tracks.iter().map(|t| t.end).fold(0.0, f64::max);
    let start = tracks.iter().map(|t| t.start).fold(f64::INFINITY, f64::min);
    let seconds = if end > 0.0 { end - start.min(end) } else if timescale > 0 { duration as f64 / timescale as f64 } else { 0.0 };
    Ok(Movie { seconds, tracks, comment: comment(&moov) })
}

/// A track's edit list, in seconds: where it starts the media on the
/// file's timeline (after its empty edits), and how long the edits after
/// that show (`None`: all of it). `timescale`: the movie's.
fn edits(trak: &[u8], timescale: u32) -> (f64, Option<f64>) {
    let Some(elst) = child(trak, b"edts").and_then(|e| child(e, b"elst")) else { return (0.0, None) };
    let wide = elst.first() == Some(&1);
    let (size, time) = if wide { (20, 8) } else { (12, 4) };
    let (mut empty, mut shown) = (0u64, 0u64);
    for i in 0..be32(elst.get(4..)).unwrap_or(0) as usize {
        let at = 8 + i * size;
        let (Some(segment), Some(media)) = (elst.get(at..at + time), elst.get(at + time..at + 2 * time)) else { break };
        let segment = if wide { be64(Some(segment)).unwrap_or(0) } else { be32(Some(segment)).unwrap_or(0) as u64 };
        match media.iter().all(|&b| b == 0xFF) {
            true if shown == 0 => empty += segment,
            true => {}
            false => shown += segment,
        }
    }
    let seconds = |t: u64| if timescale > 0 { t as f64 / timescale as f64 } else { 0.0 };
    // A fragmented file's edits may give no length (0): all of it.
    (seconds(empty), (shown > 0).then(|| seconds(shown)))
}

fn track(trak: &[u8], movie_timescale: u32) -> Result<Track> {
    let mdia = child(trak, b"mdia").context("no mdia")?;
    let hdlr = child(mdia, b"hdlr").context("no hdlr")?;
    let kind = match hdlr.get(8..12) {
        Some(b"vide") => Kind::Video,
        Some(b"soun") => Kind::Audio,
        _ => Kind::Other,
    };
    // The handler's name after 24 bytes: C string (MP4) or, from QuickTime
    // writers, Pascal (a length byte first).
    let handler = hdlr.get(24..).map(|n| {
        let n = match n.first() {
            Some(&l) if l as usize + 1 == n.len() || (l as usize + 1 < n.len() && n[l as usize + 1] == 0 && l > 0) => &n[1..1 + l as usize],
            _ => n,
        };
        String::from_utf8_lossy(n.split(|b| *b == 0).next().unwrap_or_default()).trim().to_owned()
    });
    // A `udta/name` box: the track's title as ffmpeg writes it.
    let titled = child(trak, b"udta").and_then(|u| child(u, b"name")).map(|n| String::from_utf8_lossy(n.split(|b| *b == 0).next().unwrap_or_default()).trim().to_owned());
    let name = titled.into_iter().chain(handler).find(|n| !n.is_empty());

    let mdhd = child(mdia, b"mdhd").context("no mdhd")?;
    let (timescale, duration) = times(mdhd).context("a short mdhd")?;
    let stbl = child(mdia, b"minf").and_then(|m| child(m, b"stbl")).context("no stbl")?;
    let stsz = child(stbl, b"stsz").context("no stsz")?;
    let (fixed, count) = (be32(stsz.get(4..)).context("a short stsz")?, be32(stsz.get(8..)).context("a short stsz")? as u64);
    let bytes = if fixed != 0 { fixed as u64 * count } else { (0..count as usize).map(|i| be32(stsz.get(12 + 4 * i..)).unwrap_or(0) as u64).sum() };
    // The samples' own time, as ffprobe counts it for the frame rate.
    let ticks: u64 = child(stbl, b"stts").map_or(duration, |s| {
        (0..be32(s.get(4..)).unwrap_or(0) as usize).map(|i| be32(s.get(8 + 8 * i..)).unwrap_or(0) as u64 * be32(s.get(12 + 8 * i..)).unwrap_or(0) as u64).sum()
    });
    let seconds = if timescale > 0 { (if ticks > 0 { ticks } else { duration }) as f64 / timescale as f64 } else { 0.0 };
    let (start, shown) = edits(trak, movie_timescale);
    let media = if timescale > 0 { duration as f64 / timescale as f64 } else { 0.0 };
    let end = start + shown.map_or(media, |s| media.min(s));

    let (mut width, mut height, mut matrix) = (0, 0, None);
    if kind == Kind::Video
        && let Some(stsd) = child(stbl, b"stsd")
        && let Some(entry) = stsd.get(8..)
        && entry.len() >= 8 + 78
    {
        // VisualSampleEntry: size, type, then width and height at 32.
        let (size, kind) = (be32(Some(entry)).unwrap_or(0) as usize, &entry[4..8]);
        width = be16(&entry[32..]) as u32;
        height = be16(&entry[34..]) as u32;
        let boxes = entry.get(86..size.min(entry.len())).unwrap_or_default();
        matrix = child(boxes, b"colr").filter(|c| c.get(..4) == Some(b"nclx") && c.len() >= 10).map(|c| be16(&c[8..]) as u8);
        if matrix.is_none() && (kind == b"avc1" || kind == b"avc3") {
            matrix = child(boxes, b"avcC").and_then(avcc_matrix);
        }
    }
    Ok(Track { kind, name, samples: count, bytes, seconds, start, end, width, height, matrix })
}

/// The file comment: `udta/meta/ilst/©cmt` (MP4) or `udta/©cmt` (QuickTime).
fn comment(moov: &[u8]) -> Option<String> {
    let udta = child(moov, b"udta")?;
    if let Some(meta) = child(udta, b"meta") {
        // `meta` is a full box (4 bytes of version and flags) in MP4, but
        // QuickTime writers can leave them out: its first child is `hdlr`.
        let body = if meta.get(4..8) == Some(b"hdlr") { meta } else { meta.get(4..)? };
        if let Some(data) = child(body, b"ilst").and_then(|i| child(i, b"\xa9cmt")).and_then(|c| child(c, b"data")) {
            // Type (4), locale (4), then the text.
            return Some(String::from_utf8_lossy(data.get(8..)?).into_owned());
        }
    }
    // Length (2), language (2), then the text.
    let cmt = child(udta, b"\xa9cmt")?;
    let n = be16(cmt.get(..2)?) as usize;
    Some(String::from_utf8_lossy(cmt.get(4..4 + n)?).into_owned())
}

/// The colour matrix in an `avcC`'s first SPS's VUI, where it says one.
fn avcc_matrix(avcc: &[u8]) -> Option<u8> {
    let count = *avcc.get(5)? as usize & 0x1F;
    if count == 0 {
        return None;
    }
    let len = be16(avcc.get(6..8)?) as usize;
    sps_matrix(avcc.get(8..8 + len)?)
}

/// The colour matrix from an H.264 SPS (with its NAL header), if its VUI
/// has a colour description.
fn sps_matrix(nal: &[u8]) -> Option<u8> {
    // Without the emulation-prevention bytes (00 00 03 → 00 00).
    let mut rbsp = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal.get(1..)? {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        rbsp.push(b);
    }
    let mut r = Bits { data: &rbsp, at: 0 };
    let profile = r.bits(8)?;
    r.bits(16)?; // constraint flags, level
    r.ue()?; // seq_parameter_set_id
    if matches!(profile, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        let chroma = r.ue()?;
        if chroma == 3 {
            r.bits(1)?;
        }
        r.ue()?;
        r.ue()?;
        r.bits(1)?;
        if r.bits(1)? == 1 {
            // Scaling matrices: rarely there; not worth walking.
            return None;
        }
    }
    r.ue()?; // log2_max_frame_num_minus4
    match r.ue()? {
        0 => {
            r.ue()?;
        }
        1 => {
            r.bits(1)?;
            r.se()?;
            r.se()?;
            for _ in 0..r.ue()? {
                r.se()?;
            }
        }
        _ => {}
    }
    r.ue()?; // max_num_ref_frames
    r.bits(1)?;
    r.ue()?;
    r.ue()?;
    if r.bits(1)? == 0 {
        r.bits(1)?; // mb_adaptive_frame_field_flag
    }
    r.bits(1)?;
    if r.bits(1)? == 1 {
        for _ in 0..4 {
            r.ue()?;
        }
    }
    if r.bits(1)? == 0 {
        return None; // no VUI
    }
    if r.bits(1)? == 1 {
        // aspect_ratio_info: 255 is extended (two 16-bit numbers).
        if r.bits(8)? == 255 {
            r.bits(32)?;
        }
    }
    if r.bits(1)? == 1 {
        r.bits(1)?; // overscan
    }
    if r.bits(1)? == 0 {
        return None; // no video signal type
    }
    r.bits(3)?;
    r.bits(1)?;
    if r.bits(1)? == 0 {
        return None; // no colour description
    }
    r.bits(8)?;
    r.bits(8)?;
    Some(r.bits(8)? as u8)
}

/// Reads bits, most significant first.
struct Bits<'a> {
    data: &'a [u8],
    at: usize,
}

impl Bits<'_> {
    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = *self.data.get(self.at / 8)?;
            v = (v << 1) | u32::from((byte >> (7 - self.at % 8)) & 1);
            self.at += 1;
        }
        Some(v)
    }

    /// Unsigned Exp-Golomb.
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bits(1)? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some((1u32 << zeros) - 1 + self.bits(zeros)?)
    }

    /// Signed Exp-Golomb.
    fn se(&mut self) -> Option<i32> {
        let k = self.ue()?;
        Some(if k % 2 == 1 { k.div_ceil(2) as i32 } else { -((k / 2) as i32) })
    }
}

/// A top-level box's body, read from the file (`mdat` is skipped over).
fn find_top(f: &mut File, len: u64, kind: &[u8; 4]) -> Result<Option<Vec<u8>>> {
    let mut at = 0u64;
    while at + 8 <= len {
        let mut head = [0u8; 16];
        f.seek(SeekFrom::Start(at))?;
        f.read_exact(&mut head[..8])?;
        let mut size = be32(Some(&head)).unwrap_or(0) as u64;
        let mut header = 8;
        if size == 1 {
            f.read_exact(&mut head[8..16])?;
            size = be64(Some(&head[8..])).context("box size")?;
            header = 16;
        } else if size == 0 {
            size = len - at;
        }
        if size < header {
            bail!("a broken box");
        }
        if &head[4..8] == kind {
            let mut body = vec![0u8; (size - header) as usize];
            f.read_exact(&mut body)?;
            return Ok(Some(body));
        }
        at += size;
    }
    Ok(None)
}

/// The boxes in `data`: their type and body.
fn boxes(mut data: &[u8]) -> impl Iterator<Item = ([u8; 4], &[u8])> {
    std::iter::from_fn(move || {
        if data.len() < 8 {
            return None;
        }
        let (size, kind) = (be32(Some(data))? as usize, [data[4], data[5], data[6], data[7]]);
        let (header, size) = match size {
            1 => (16, be64(data.get(8..))? as usize),
            0 => (8, data.len()),
            s => (8, s),
        };
        if size < header || size > data.len() {
            return None;
        }
        let body = &data[header..size];
        data = &data[size..];
        Some((kind, body))
    })
}

fn child<'a>(data: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
    boxes(data).find(|(k, _)| k == kind).map(|(_, b)| b)
}

fn children<'a>(data: &'a [u8], kind: &'a [u8; 4]) -> impl Iterator<Item = &'a [u8]> {
    boxes(data).filter(move |(k, _)| k == kind).map(|(_, b)| b)
}

fn be16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}

fn be32(b: Option<&[u8]>) -> Option<u32> {
    Some(u32::from_be_bytes(b?.get(..4)?.try_into().ok()?))
}

fn be64(b: Option<&[u8]>) -> Option<u64> {
    Some(u64::from_be_bytes(b?.get(..8)?.try_into().ok()?))
}

/// A movie or media header's timescale and duration (version 0 or 1).
fn times(header: &[u8]) -> Option<(u32, u64)> {
    match header.first()? {
        1 => Some((be32(header.get(20..))?, be64(header.get(24..))?)),
        _ => Some((be32(header.get(12..))?, be32(header.get(16..))? as u64)),
    }
}
