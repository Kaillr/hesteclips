//! Reading an MP4's index: where each video frame's bytes are, which frames
//! are keyframes, and the decoder's setup (SPS/PPS), so a frame can be handed
//! straight to the decoder without a file reader in between: on Windows,
//! Media Foundation's (whose seek costs ~30 ms of a ~40 ms jump); on macOS
//! AVFoundation's, which can't hand frames to our own decoder at all.
//!
//! Only what H.264 video in a plain (not fragmented) MP4 needs; anything
//! else is an error, and the caller falls back to another way.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};

/// One video frame in the file.
#[derive(Debug, Clone, Copy)]
pub struct Sample {
    pub offset: u64,
    pub size: u32,
    /// Presentation time, in the track's timescale.
    pub pts: i64,
    pub key: bool,
}

#[derive(Debug, Clone)]
pub struct Index {
    pub width: u32,
    pub height: u32,
    pub timescale: u32,
    /// In decode order.
    pub samples: Vec<Sample>,
    /// Bytes in each NAL unit's length prefix.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub nal_length: usize,
    #[cfg_attr(not(windows), allow(dead_code))]
    pub sps: Vec<Vec<u8>>,
    #[cfg_attr(not(windows), allow(dead_code))]
    pub pps: Vec<Vec<u8>>,
    /// The `avcC` box's body as stored: what a decoder setup takes whole.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub avcc: Vec<u8>,
    /// The colour matrix the file states (`colr` box, ISO 23091-2 numbers:
    /// 1 BT.709, 5/6 BT.601), if it does.
    pub matrix: Option<u16>,
}

impl Index {
    /// The video track of `path`.
    pub fn read(path: &Path) -> Result<Self> {
        let mut f = File::open(path)?;
        let len = f.metadata()?.len();
        let moov = find_top(&mut f, len, b"moov")?.context("no moov box (a fragmented or unfinished file?)")?;
        for trak in children(&moov, b"trak") {
            let Some(mdia) = child(trak, b"mdia") else { continue };
            let Some(hdlr) = child(mdia, b"hdlr") else { continue };
            if hdlr.get(8..12) != Some(b"vide") {
                continue;
            }
            return parse_video(mdia);
        }
        bail!("no video track")
    }

    /// Indices (into `samples`) of the keyframes, in decode order.
    pub fn keyframes(&self) -> Vec<usize> {
        self.samples.iter().enumerate().filter(|(_, s)| s.key).map(|(i, _)| i).collect()
    }
}

fn parse_video(mdia: &[u8]) -> Result<Index> {
    let mdhd = child(mdia, b"mdhd").context("no mdhd")?;
    let timescale = if mdhd[0] == 1 { be32(&mdhd[20..]) } else { be32(&mdhd[12..]) };
    let stbl = child(mdia, b"minf").and_then(|m| child(m, b"stbl")).context("no stbl")?;

    // Sample description: avc1 with its avcC.
    let stsd = child(stbl, b"stsd").context("no stsd")?;
    let entry = stsd.get(8..).context("short stsd")?;
    let (kind, body) = boxes(entry).next().context("empty stsd")?;
    ensure!(kind == *b"avc1" || kind == *b"avc3", "not H.264 ({})", String::from_utf8_lossy(&kind));
    ensure!(body.len() >= 78, "short sample entry");
    let (width, height) = (be16(&body[24..]) as u32, be16(&body[26..]) as u32);
    let avcc = child(&body[78..], b"avcC").context("no avcC")?;
    let matrix = child(&body[78..], b"colr").filter(|c| c.get(..4) == Some(b"nclx") && c.len() >= 10).map(|c| be16(&c[8..]));
    ensure!(avcc.len() >= 7, "short avcC");
    let nal_length = (avcc[4] & 3) as usize + 1;
    let mut at = 6;
    let list = |at: &mut usize, count: usize| -> Result<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        for _ in 0..count {
            let n = be16(avcc.get(*at..).context("short avcC")?) as usize;
            out.push(avcc.get(*at + 2..*at + 2 + n).context("short avcC")?.to_vec());
            *at += 2 + n;
        }
        Ok(out)
    };
    let sps = list(&mut at, (avcc[5] & 31) as usize)?;
    let n_pps = *avcc.get(at).context("short avcC")? as usize;
    at += 1;
    let pps = list(&mut at, n_pps)?;

    // Sizes.
    let stsz = child(stbl, b"stsz").context("no stsz")?;
    let fixed = be32(&stsz[4..]);
    let count = be32(&stsz[8..]) as usize;
    let sizes: Vec<u32> = if fixed != 0 { vec![fixed; count] } else { (0..count).map(|i| be32(&stsz[12 + 4 * i..])).collect() };

    // Chunk offsets, and how many samples each chunk holds.
    let offsets: Vec<u64> = if let Some(stco) = child(stbl, b"stco") {
        (0..be32(&stco[4..]) as usize).map(|i| be32(&stco[8 + 4 * i..]) as u64).collect()
    } else {
        let co64 = child(stbl, b"co64").context("no chunk offsets")?;
        (0..be32(&co64[4..]) as usize).map(|i| be64(&co64[8 + 8 * i..])).collect()
    };
    let stsc = child(stbl, b"stsc").context("no stsc")?;
    let runs: Vec<(u32, u32)> = (0..be32(&stsc[4..]) as usize).map(|i| (be32(&stsc[8 + 12 * i..]), be32(&stsc[12 + 12 * i..]))).collect();
    let mut sample_offsets = Vec::with_capacity(count);
    for (chunk, &base) in offsets.iter().enumerate() {
        let n = runs.iter().rev().find(|r| r.0 as usize <= chunk + 1).map_or(0, |r| r.1);
        let mut off = base;
        for _ in 0..n {
            if sample_offsets.len() == count {
                break;
            }
            sample_offsets.push(off);
            off += sizes[sample_offsets.len() - 1] as u64;
        }
    }
    ensure!(sample_offsets.len() == count, "chunks hold {} samples, not {count}", sample_offsets.len());

    // Times: decode times from stts, plus ctts' offsets when frames are reordered.
    let stts = child(stbl, b"stts").context("no stts")?;
    let mut dts = Vec::with_capacity(count);
    let mut t = 0i64;
    for i in 0..be32(&stts[4..]) as usize {
        let (n, delta) = (be32(&stts[8 + 8 * i..]), be32(&stts[12 + 8 * i..]));
        for _ in 0..n {
            dts.push(t);
            t += delta as i64;
        }
    }
    ensure!(dts.len() >= count, "stts covers {} samples, not {count}", dts.len());
    let mut pts = dts;
    if let Some(ctts) = child(stbl, b"ctts") {
        let mut i = 0;
        for e in 0..be32(&ctts[4..]) as usize {
            let (n, off) = (be32(&ctts[8 + 8 * e..]), be32(&ctts[12 + 8 * e..]) as i32);
            for _ in 0..n {
                if i < count {
                    pts[i] += off as i64;
                }
                i += 1;
            }
        }
    }
    // Keyframes: all of them when there's no stss.
    let keys: Option<Vec<u32>> = child(stbl, b"stss").map(|s| (0..be32(&s[4..]) as usize).map(|i| be32(&s[8 + 4 * i..])).collect());
    let samples = (0..count)
        .map(|i| Sample {
            offset: sample_offsets[i],
            size: sizes[i],
            pts: pts[i],
            key: keys.as_ref().is_none_or(|k| k.binary_search(&(i as u32 + 1)).is_ok()),
        })
        .collect();
    Ok(Index { width, height, timescale, samples, nal_length, sps, pps, avcc: avcc.to_vec(), matrix })
}

/// A top-level box's body, read into memory.
fn find_top(f: &mut File, len: u64, kind: &[u8; 4]) -> Result<Option<Vec<u8>>> {
    let mut at = 0u64;
    while at + 8 <= len {
        f.seek(SeekFrom::Start(at))?;
        let mut h = [0u8; 16];
        f.read_exact(&mut h[..8])?;
        let mut size = be32(&h) as u64;
        let mut header = 8;
        if size == 1 {
            f.read_exact(&mut h[8..16])?;
            size = be64(&h[8..]);
            header = 16;
        } else if size == 0 {
            size = len - at;
        }
        ensure!(size >= header, "bad box size");
        if &h[4..8] == kind {
            let mut body = vec![0u8; (size - header) as usize];
            f.read_exact(&mut body)?;
            return Ok(Some(body));
        }
        at += size;
    }
    Ok(None)
}

/// Boxes in `data`: (type, body).
fn boxes(data: &[u8]) -> impl Iterator<Item = ([u8; 4], &[u8])> {
    let mut at = 0usize;
    std::iter::from_fn(move || {
        let h = data.get(at..at + 8)?;
        let mut size = be32(h) as usize;
        let mut header = 8;
        if size == 1 {
            size = be64(data.get(at + 8..at + 16)?) as usize;
            header = 16;
        } else if size == 0 {
            size = data.len() - at;
        }
        let body = data.get(at + header..at + size)?;
        let kind = h[4..8].try_into().ok()?;
        at += size.max(8);
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

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

fn be64(b: &[u8]) -> u64 {
    u64::from_be_bytes(b[..8].try_into().unwrap())
}
