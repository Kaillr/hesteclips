//! Name the tracks of a finished MP4/MOV file in place.
//!
//! Players and ffprobe show a track's name from its handler box (`hdlr`), but
//! AVAssetWriter always writes "Core Media Audio" there and puts names where
//! almost nothing reads them. This rewrites the `hdlr` names in `moov`, growing
//! or shrinking the boxes around them and fixing chunk offsets when the media
//! data comes after `moov` (fast-start files). When `moov` is at the end — long
//! recordings — only the file's tail is rewritten.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};

/// Set the handler names of `path`'s tracks: `video` for video tracks, and
/// `audio[i]` for the i-th audio track (extra tracks keep their names).
pub fn name_tracks(path: &Path, video: &str, audio: &[String]) -> Result<()> {
    let mut file = OpenOptions::new().read(true).write(true).open(path).context("open for track naming")?;
    let len = file.metadata()?.len();
    let (moov_at, moov_len) = find_top_level(&mut file, len, b"moov")?.context("no moov box")?;
    let mut moov = vec![0u8; moov_len as usize];
    file.seek(SeekFrom::Start(moov_at))?;
    file.read_exact(&mut moov)?;

    let mut audio_names = audio.iter();
    let new_moov = rewrite(&moov, &mut |handler, name| match handler {
        b"vide" => Some(video.to_owned()),
        b"soun" => audio_names.next().cloned().or(Some(name)),
        _ => None,
    })?;
    let delta = new_moov.len() as i64 - moov.len() as i64;
    if delta == 0 && new_moov == moov {
        return Ok(());
    }

    if moov_at + moov_len == len {
        // moov is last: offsets into mdat (before it) don't move.
        file.seek(SeekFrom::Start(moov_at))?;
        file.write_all(&new_moov)?;
        file.set_len(moov_at + new_moov.len() as u64)?;
        return Ok(());
    }

    // moov before the media: every chunk offset past it shifts by delta.
    let new_moov = shift_chunk_offsets(&new_moov, moov_at, delta)?;
    let mut rest = Vec::new();
    file.seek(SeekFrom::Start(moov_at + moov_len))?;
    file.read_to_end(&mut rest)?;
    file.seek(SeekFrom::Start(moov_at))?;
    file.write_all(&new_moov)?;
    file.write_all(&rest)?;
    file.set_len(moov_at + new_moov.len() as u64 + rest.len() as u64)?;
    Ok(())
}

fn find_top_level(file: &mut File, len: u64, want: &[u8; 4]) -> Result<Option<(u64, u64)>> {
    let mut off = 0u64;
    while off + 8 <= len {
        let mut hdr = [0u8; 16];
        file.seek(SeekFrom::Start(off))?;
        file.read_exact(&mut hdr[..8])?;
        let mut size = u32::from_be_bytes(hdr[..4].try_into().unwrap()) as u64;
        if size == 1 {
            file.read_exact(&mut hdr[8..16])?;
            size = u64::from_be_bytes(hdr[8..16].try_into().unwrap());
        } else if size == 0 {
            size = len - off;
        }
        if size < 8 {
            bail!("corrupt box at {off}");
        }
        if &hdr[4..8] == want {
            return Ok(Some((off, size)));
        }
        off += size;
    }
    Ok(None)
}

/// Boxes that only contain other boxes, on the way down to `hdlr` and `stco`.
const CONTAINERS: [&[u8; 4]; 5] = [b"moov", b"trak", b"mdia", b"minf", b"stbl"];

/// Children of a box body as (type, full box bytes).
fn children(body: &[u8]) -> Result<Vec<([u8; 4], &[u8])>> {
    let mut out = Vec::new();
    let mut off = 0usize;
    while off + 8 <= body.len() {
        let size = u32::from_be_bytes(body[off..off + 4].try_into().unwrap()) as usize;
        let size = if size == 0 { body.len() - off } else { size };
        if size < 8 || off + size > body.len() {
            bail!("corrupt box inside moov");
        }
        out.push((body[off + 4..off + 8].try_into().unwrap(), &body[off..off + size]));
        off += size;
    }
    Ok(out)
}

fn make_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(body.len() + 8);
    b.extend_from_slice(&((body.len() + 8) as u32).to_be_bytes());
    b.extend_from_slice(kind);
    b.extend_from_slice(body);
    b
}

/// Rebuild a box tree, letting `rename(handler_type, current_name)` replace each
/// `hdlr` name (None keeps it). `boxed` is one complete box.
fn rewrite(boxed: &[u8], rename: &mut dyn FnMut(&[u8; 4], String) -> Option<String>) -> Result<Vec<u8>> {
    let kind: [u8; 4] = boxed[4..8].try_into().unwrap();
    let body = &boxed[8..];
    if kind == *b"hdlr" && body.len() >= 24 {
        // version/flags(4) pre_defined(4) handler_type(4) reserved(12) name...
        let handler: [u8; 4] = body[8..12].try_into().unwrap();
        let name = String::from_utf8_lossy(&body[24..]).trim_end_matches('\0').to_owned();
        return Ok(match rename(&handler, name) {
            Some(new) => {
                let mut b = body[..24].to_vec();
                b.extend_from_slice(new.as_bytes());
                b.push(0);
                make_box(&kind, &b)
            }
            None => boxed.to_vec(),
        });
    }
    if !CONTAINERS.contains(&&kind) {
        return Ok(boxed.to_vec());
    }
    let mut out = Vec::with_capacity(body.len());
    for (_, child) in children(body)? {
        out.extend(rewrite(child, rename)?);
    }
    Ok(make_box(&kind, &out))
}

/// Add `delta` to every chunk offset (`stco`/`co64`) that points past `after`.
fn shift_chunk_offsets(boxed: &[u8], after: u64, delta: i64) -> Result<Vec<u8>> {
    let kind: [u8; 4] = boxed[4..8].try_into().unwrap();
    let body = &boxed[8..];
    if kind == *b"stco" || kind == *b"co64" {
        let mut b = body.to_vec();
        let n = u32::from_be_bytes(b[4..8].try_into().unwrap()) as usize;
        let wide = kind == *b"co64";
        for i in 0..n {
            if wide {
                let at = 8 + i * 8;
                let v = u64::from_be_bytes(b[at..at + 8].try_into().unwrap());
                if v > after {
                    b[at..at + 8].copy_from_slice(&((v as i64 + delta) as u64).to_be_bytes());
                }
            } else {
                let at = 8 + i * 4;
                let v = u32::from_be_bytes(b[at..at + 4].try_into().unwrap()) as u64;
                if v > after {
                    let nv = (v as i64 + delta) as u64;
                    if nv > u32::MAX as u64 {
                        bail!("chunk offset overflow; file too large to rename tracks in place");
                    }
                    b[at..at + 4].copy_from_slice(&(nv as u32).to_be_bytes());
                }
            }
        }
        return Ok(make_box(&kind, &b));
    }
    if !CONTAINERS.contains(&&kind) {
        return Ok(boxed.to_vec());
    }
    let mut out = Vec::with_capacity(body.len());
    for (_, child) in children(body)? {
        out.extend(shift_chunk_offsets(child, after, delta)?);
    }
    Ok(make_box(&kind, &out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdlr(handler: &[u8; 4], name: &str) -> Vec<u8> {
        let mut b = vec![0u8; 8];
        b.extend_from_slice(handler);
        b.extend_from_slice(&[0u8; 12]);
        b.extend_from_slice(name.as_bytes());
        b.push(0);
        make_box(b"hdlr", &b)
    }

    fn stco(offsets: &[u32]) -> Vec<u8> {
        let mut b = vec![0u8; 4];
        b.extend_from_slice(&(offsets.len() as u32).to_be_bytes());
        for o in offsets {
            b.extend_from_slice(&o.to_be_bytes());
        }
        make_box(b"stco", &b)
    }

    fn trak(handler: &[u8; 4], name: &str, offset: u32) -> Vec<u8> {
        let stbl = make_box(b"stbl", &stco(&[offset]));
        let minf = make_box(b"minf", &stbl);
        let mut mdia = hdlr(handler, name);
        mdia.extend(minf);
        make_box(b"trak", &make_box(b"mdia", &mdia))
    }

    /// A file: ftyp, then moov and mdat in either order. mdat holds one marker
    /// byte per track, which the chunk offsets point at.
    fn file(moov_first: bool) -> Vec<u8> {
        let ftyp = make_box(b"ftyp", b"isom");
        let build = |mdat_at: u32| {
            let mut m = trak(b"vide", "Core Media Video", mdat_at + 8);
            m.extend(trak(b"soun", "Core Media Audio", mdat_at + 9));
            m.extend(trak(b"soun", "Core Media Audio", mdat_at + 10));
            make_box(b"moov", &m)
        };
        let mdat = make_box(b"mdat", b"VAB");
        let mut out = ftyp.clone();
        if moov_first {
            let moov_len = build(0).len() as u32;
            out.extend(build(ftyp.len() as u32 + moov_len));
            out.extend(mdat);
        } else {
            out.extend(mdat);
            out.extend(build(ftyp.len() as u32));
        }
        out
    }

    /// (name, byte its chunk offset points at) per track.
    fn read_back(bytes: &[u8]) -> Vec<(String, u8)> {
        let mut out = Vec::new();
        let moov = children(bytes).unwrap().into_iter().find(|(k, _)| k == b"moov").unwrap().1;
        for (_, trak) in children(&moov[8..]).unwrap() {
            let mdia = children(&trak[8..]).unwrap()[0].1;
            let parts = children(&mdia[8..]).unwrap();
            let name = String::from_utf8_lossy(&parts[0].1[8 + 24..]).trim_end_matches('\0').to_owned();
            let stbl = children(&parts[1].1[8..]).unwrap()[0].1;
            let stco = children(&stbl[8..]).unwrap()[0].1;
            let off = u32::from_be_bytes(stco[16..20].try_into().unwrap()) as usize;
            out.push((name, bytes[off]));
        }
        out
    }

    #[test]
    fn renames_and_keeps_offsets_valid() {
        for moov_first in [true, false] {
            let path = std::env::temp_dir().join(format!("hc-mp4meta-{moov_first}.mp4"));
            std::fs::write(&path, file(moov_first)).unwrap();
            assert_eq!(read_back(&std::fs::read(&path).unwrap())[1].1, b'A');
            name_tracks(&path, "Video", &["Mix".into(), "A much longer microphone name".into()]).unwrap();
            let got = read_back(&std::fs::read(&path).unwrap());
            assert_eq!(
                got,
                vec![
                    ("Video".to_owned(), b'V'),
                    ("Mix".to_owned(), b'A'),
                    ("A much longer microphone name".to_owned(), b'B')
                ],
                "moov_first={moov_first}"
            );
            let _ = std::fs::remove_file(path);
        }
    }
}
