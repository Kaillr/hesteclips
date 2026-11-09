//! A small MP4/MOV writer for exactly what we record: one H.264 video track
//! followed by AAC audio tracks (48 kHz stereo).
//!
//! - **Fragmented** (recordings): the file starts with an index-less `moov` and
//!   gets a `moof`+`mdat` fragment every second, so a crash or power loss leaves
//!   a file that plays up to the last fragment. On a clean finish it's turned
//!   into an ordinary MP4 in place — a full `moov` is appended and the old
//!   `moov`/`moof` boxes are relabelled `free` — so players get a normal,
//!   instantly seekable file without the media being copied.
//! - **Plain** (replay clips): everything is in memory already, so the `moov` is
//!   written first ("fast start"), which lets a shared clip start playing before
//!   it has fully downloaded.
//!
//! Track names (`hdlr`) and the mix tag (`©cmt`, read by ffprobe as `comment`)
//! are written directly.
//!
//! On macOS only exports' pictures are written here (`mac::export`; the
//! recordings by AVAssetWriter): the sound and the encoders' Annex B go unused.

#![cfg_attr(target_os = "macos", allow(dead_code))]

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};

/// Shared sample bytes (a frame in the replay ring and in a clip being saved).
pub(crate) type Data = Arc<[u8]>;

/// Movie timescale (mvhd, tkhd, elst).
const MOVIE_TS: u32 = 1000;
/// Audio is always 48 kHz; one AAC packet is 1024 frames.
const AUDIO_RATE: u32 = 48_000;
const AAC_FRAMES: u32 = 1024;
/// A fragment is written once this much video is pending.
const FRAGMENT_SECS: u32 = 1;

/// What goes in the file, known before the first frame.
#[derive(Debug, Clone)]
pub(crate) struct Spec {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// One per audio track, in file order.
    pub audio_titles: Vec<String>,
    pub audio_bitrate: u32,
    /// Stored as the file's comment (the mix tag); empty for none.
    pub comment: String,
    /// QuickTime brand instead of MP4.
    pub mov: bool,
    /// Video frames that may come before ones shown earlier (an encoder's
    /// B-frames: frames come in the order they're decoded): each is then
    /// one frame long, decoded this many frames before it's shown at the
    /// latest, and an edit takes that back out. 0: frames come as shown.
    pub reorder: u32,
}

/// H.264 parameter sets, from the first keyframe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AvcConfig {
    pub sps: Vec<u8>,
    pub pps: Vec<u8>,
}

impl AvcConfig {
    /// As an `avcC` box holds them (the box's content): what a decoder is
    /// set up from, for frames with 4-byte NAL lengths.
    pub(crate) fn record(&self) -> Vec<u8> {
        let (sps, pps) = (&self.sps, &self.pps);
        let mut avcc = vec![1, sps.get(1).copied().unwrap_or(100), sps.get(2).copied().unwrap_or(0), sps.get(3).copied().unwrap_or(40)];
        avcc.push(0xFF); // 4-byte NAL lengths
        avcc.push(0xE1);
        avcc.put_u16(sps.len() as u16);
        avcc.extend_from_slice(sps);
        avcc.push(1);
        avcc.put_u16(pps.len() as u16);
        avcc.extend_from_slice(pps);
        if matches!(sps.get(1), Some(100 | 110 | 122 | 144)) {
            // 4:2:0, 8-bit, no SPS extensions.
            avcc.extend_from_slice(&[0xFD, 0xF8, 0xF8, 0]);
        }
        avcc
    }
}

struct Track {
    id: u32,
    video: bool,
    timescale: u32,
    name: String,
    sizes: Vec<u32>,
    durations: Vec<u32>,
    /// 1-based numbers of sync samples (video only).
    syncs: Vec<u32>,
    /// How long after its decoding each sample is shown, where frames are
    /// reordered (else none).
    shown_after: Vec<u32>,
    /// (file offset, sample count) per chunk.
    chunks: Vec<(u64, u32)>,
    /// Where the track's media starts on the file's timeline, in its timescale
    /// (written as an empty edit).
    offset: u64,
    /// Fragmented: samples not yet written (data, duration, sync, shown
    /// after), and the decode time of the first.
    pending: Vec<(Data, u32, bool, u32)>,
    decode_time: u64,
}

impl Track {
    fn duration(&self) -> u64 {
        self.durations.iter().map(|&d| d as u64).sum()
    }

    fn pending_duration(&self) -> u64 {
        self.pending.iter().map(|p| p.1 as u64).sum()
    }

    fn movie_duration(&self) -> u64 {
        (self.offset + self.duration()) * MOVIE_TS as u64 / self.timescale as u64
    }
}

enum Mode {
    Fragmented { sequence: u32, moov_at: u64, moofs: Vec<u64> },
    /// (track index, data) in arrival order; chunk = run of one track.
    Plain { samples: Vec<(usize, Data)> },
}

pub(crate) struct Muxer {
    file: BufWriter<File>,
    /// Bytes written so far (the file offset of the next write).
    pos: u64,
    spec: Spec,
    avc: AvcConfig,
    tracks: Vec<Track>,
    mode: Mode,
    /// The newest video frame, held until the next one gives its duration.
    held: Option<(Data, i64, bool)>,
    /// Video frames so far, where frames are reordered.
    frames: i64,
    /// Next expected frame per audio track (None until its first packet).
    audio_next: Vec<Option<i64>>,
}

impl Muxer {
    pub(crate) fn create(path: &Path, spec: &Spec, avc: AvcConfig, fragmented: bool) -> Result<Self> {
        let file = File::create(path).with_context(|| format!("can't create {}", path.display()))?;
        let video_ts = video_timescale(spec.fps);
        let mut tracks = vec![Track::new(1, true, video_ts, "Video".into())];
        for (i, title) in spec.audio_titles.iter().enumerate() {
            tracks.push(Track::new(i as u32 + 2, false, AUDIO_RATE, title.clone()));
        }
        let mode = if fragmented {
            Mode::Fragmented { sequence: 0, moov_at: 0, moofs: Vec::new() }
        } else {
            Mode::Plain { samples: Vec::new() }
        };
        let mut mux = Self {
            file: BufWriter::with_capacity(1 << 20, file),
            pos: 0,
            spec: spec.clone(),
            avc,
            audio_next: vec![None; spec.audio_titles.len()],
            tracks,
            mode,
            held: None,
            frames: 0,
        };
        if fragmented {
            let ftyp = mux.ftyp();
            mux.write(&ftyp)?;
            let moov_at = mux.pos;
            let moov = mux.moov(true);
            mux.write(&moov)?;
            mux.file.flush()?;
            if let Mode::Fragmented { moov_at: at, .. } = &mut mux.mode {
                *at = moov_at;
            }
        }
        Ok(mux)
    }

    /// A video frame (AVCC, 4-byte lengths) presented at `t` seconds into the file.
    pub(crate) fn video(&mut self, data: Data, t: f64, sync: bool) -> Result<()> {
        let tick = (t * self.tracks[0].timescale as f64).round() as i64;
        if self.spec.reorder > 0 {
            // In decode order: frame `frames` is decoded at that many frames
            // in and shown at `tick`, `reorder` frames later than it is.
            let frame = self.frame_ticks() as i64;
            let shown_after = tick + self.spec.reorder as i64 * frame - self.frames * frame;
            self.frames += 1;
            return self.push(0, data, frame as u32, sync, shown_after.max(0) as u32);
        }
        if let Some((prev, prev_tick, prev_sync)) = self.held.take() {
            let dur = (tick - prev_tick).max(1) as u32;
            self.push(0, prev, dur, prev_sync, 0)?;
        }
        self.held = Some((data, tick, sync));
        Ok(())
    }

    /// A video frame's length, in its track's timescale.
    fn frame_ticks(&self) -> u32 {
        (self.tracks[0].timescale / self.spec.fps.max(1)).max(1)
    }

    /// An AAC packet starting at 48 kHz frame `frame` of the file (≥ 0).
    pub(crate) fn audio(&mut self, track: usize, data: Data, frame: i64) -> Result<()> {
        let Some(next) = self.audio_next.get_mut(track) else { return Ok(()) };
        match next {
            None => self.tracks[track + 1].offset = frame.max(0) as u64,
            // Packets are contiguous; anything overlapping what's written is a repeat.
            Some(n) if frame < *n => return Ok(()),
            _ => {}
        }
        *next = Some(frame.max(next.unwrap_or(0)) + AAC_FRAMES as i64);
        self.push(track + 1, data, AAC_FRAMES, true, 0)
    }

    fn push(&mut self, track: usize, data: Data, dur: u32, sync: bool, shown_after: u32) -> Result<()> {
        let reordered = track == 0 && self.spec.reorder > 0;
        match &mut self.mode {
            Mode::Plain { samples } => {
                let t = &mut self.tracks[track];
                t.sizes.push(data.len() as u32);
                t.durations.push(dur);
                if t.video && sync {
                    t.syncs.push(t.sizes.len() as u32);
                }
                if reordered {
                    t.shown_after.push(shown_after);
                }
                samples.push((track, data));
            }
            Mode::Fragmented { .. } => {
                if track == 0 && sync && self.tracks[0].pending_duration() >= (FRAGMENT_SECS * self.tracks[0].timescale) as u64 {
                    self.flush_fragment()?;
                }
                self.tracks[track].pending.push((data, dur, sync, shown_after));
                // Never let a fragment grow past a few seconds, keyframes or not.
                if track == 0 && self.tracks[0].pending_duration() >= (4 * FRAGMENT_SECS * self.tracks[0].timescale) as u64 {
                    self.flush_fragment()?;
                }
            }
        }
        Ok(())
    }

    /// Write every pending sample as one `moof` + `mdat`.
    fn flush_fragment(&mut self) -> Result<()> {
        let Mode::Fragmented { sequence, moofs, .. } = &mut self.mode else { return Ok(()) };
        if self.tracks.iter().all(|t| t.pending.is_empty()) {
            return Ok(());
        }
        *sequence += 1;
        let seq = *sequence;
        let moof_at = self.pos;
        moofs.push(moof_at);

        // The moof's size doesn't depend on the data offsets, so build it once to
        // measure, then for real.
        let reorder = self.spec.reorder > 0;
        let build = |tracks: &[Track], data_start: u64| -> Vec<u8> {
            let mut trafs = Vec::new();
            let mut off = data_start;
            for t in tracks.iter().filter(|t| !t.pending.is_empty()) {
                trafs.extend(traf(t, off as i32, t.video && reorder));
                off += t.pending.iter().map(|p| p.0.len() as u64).sum::<u64>();
            }
            let mut body = full_box(b"mfhd", 0, 0, &seq.to_be_bytes());
            body.extend(trafs);
            make_box(b"moof", &body)
        };
        let moof_len = build(&self.tracks, 0).len() as u64;
        let moof = build(&self.tracks, moof_len + 8);
        let data_len: u64 = self.tracks.iter().flat_map(|t| &t.pending).map(|p| p.0.len() as u64).sum();
        self.write(&moof)?;
        self.write_mdat_header(data_len)?;
        for i in 0..self.tracks.len() {
            let pending = std::mem::take(&mut self.tracks[i].pending);
            if pending.is_empty() {
                continue;
            }
            let t = &mut self.tracks[i];
            t.chunks.push((self.pos, pending.len() as u32));
            for (data, dur, sync, shown_after) in &pending {
                t.sizes.push(data.len() as u32);
                t.durations.push(*dur);
                t.decode_time += *dur as u64;
                if t.video && *sync {
                    t.syncs.push(t.sizes.len() as u32);
                }
                if t.video && self.spec.reorder > 0 {
                    t.shown_after.push(*shown_after);
                }
            }
            for (data, ..) in &pending {
                self.file.write_all(data)?;
                self.pos += data.len() as u64;
            }
        }
        self.file.flush()?;
        Ok(())
    }

    pub(crate) fn finish(mut self) -> Result<()> {
        if let Some((data, _, sync)) = self.held.take() {
            let dur = self.frame_ticks();
            self.push(0, data, dur, sync, 0)?;
        }
        match std::mem::replace(&mut self.mode, Mode::Plain { samples: Vec::new() }) {
            Mode::Fragmented { sequence, moov_at, moofs } => {
                self.mode = Mode::Fragmented { sequence, moov_at, moofs: Vec::new() };
                self.flush_fragment()?;
                let Mode::Fragmented { moofs: last, .. } = &self.mode else { unreachable!() };
                let moofs: Vec<u64> = moofs.into_iter().chain(last.iter().copied()).collect();
                // The complete index goes at the end; only then are the fragment
                // boxes hidden, so the file is playable at every step.
                let moov = self.moov(false);
                self.write(&moov)?;
                self.file.flush()?;
                self.file.get_ref().sync_data()?;
                for at in std::iter::once(moov_at).chain(moofs) {
                    self.relabel(at, b"free")?;
                }
                self.file.flush()?;
            }
            Mode::Plain { samples } => {
                // Chunks are runs of one track, in arrival order (= time order).
                let mut runs: Vec<(usize, u32, u64)> = Vec::new(); // (track, count, bytes)
                for (track, data) in &samples {
                    match runs.last_mut() {
                        Some((t, n, b)) if t == track => {
                            *n += 1;
                            *b += data.len() as u64;
                        }
                        _ => runs.push((*track, 1, data.len() as u64)),
                    }
                }
                let ftyp = self.ftyp();
                let place = |tracks: &mut [Track], base: u64| {
                    let mut off = base;
                    for t in tracks.iter_mut() {
                        t.chunks.clear();
                    }
                    for (track, n, bytes) in &runs {
                        tracks[*track].chunks.push((off, *n));
                        off += bytes;
                    }
                };
                let data_len: u64 = runs.iter().map(|r| r.2).sum();
                let header = mdat_header_len(data_len);
                // co64 vs stco depends on the offsets, so place twice: the second
                // pass only shifts offsets by a size that's now known.
                place(&mut self.tracks, ftyp.len() as u64 + header);
                let moov_len = self.moov(false).len() as u64;
                place(&mut self.tracks, ftyp.len() as u64 + moov_len + header);
                let moov = self.moov(false);
                self.write(&ftyp)?;
                self.write(&moov)?;
                self.write_mdat_header(data_len)?;
                for (_, data) in &samples {
                    self.file.write_all(data)?;
                }
                self.file.flush()?;
            }
        }
        self.file.get_ref().sync_all()?;
        Ok(())
    }

    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.file.write_all(bytes)?;
        self.pos += bytes.len() as u64;
        Ok(())
    }

    fn write_mdat_header(&mut self, data_len: u64) -> Result<()> {
        if mdat_header_len(data_len) == 8 {
            self.write(&((data_len + 8) as u32).to_be_bytes())?;
            self.write(b"mdat")
        } else {
            self.write(&1u32.to_be_bytes())?;
            self.write(b"mdat")?;
            self.write(&(data_len + 16).to_be_bytes())
        }
    }

    /// Change the type of the box at `at` (keeping its size).
    fn relabel(&mut self, at: u64, kind: &[u8; 4]) -> Result<()> {
        let f = self.file.get_mut();
        f.seek(SeekFrom::Start(at + 4))?;
        f.write_all(kind)?;
        f.seek(SeekFrom::End(0))?;
        Ok(())
    }

    fn ftyp(&self) -> Vec<u8> {
        let mut b = Vec::new();
        if self.spec.mov {
            b.extend_from_slice(b"qt  ");
            b.put_u32(0x200);
            b.extend_from_slice(b"qt  ");
        } else {
            b.extend_from_slice(b"isom");
            b.put_u32(0x200);
            for brand in [b"isom", b"iso2", b"iso6", b"avc1", b"mp41"] {
                b.extend_from_slice(brand);
            }
        }
        make_box(b"ftyp", &b)
    }

    /// The movie box: empty sample tables + `mvex` for a fragmented file's
    /// header (`init`), else the complete index.
    fn moov(&self, init: bool) -> Vec<u8> {
        let duration = if init { 0 } else { self.tracks.iter().map(Track::movie_duration).max().unwrap_or(0) };
        let mut body = mvhd(duration, self.tracks.len() as u32 + 1);
        for t in &self.tracks {
            body.extend(self.trak(t, init));
        }
        if init {
            let mut mvex = Vec::new();
            for t in &self.tracks {
                let mut trex = Vec::new();
                trex.put_u32(t.id);
                trex.put_u32(1); // sample description
                trex.put_u32(0);
                trex.put_u32(0);
                trex.put_u32(0);
                mvex.extend(full_box(b"trex", 0, 0, &trex));
            }
            body.extend(make_box(b"mvex", &mvex));
        }
        if !self.spec.comment.is_empty() {
            body.extend(udta_comment(&self.spec.comment));
        }
        make_box(b"moov", &body)
    }

    fn trak(&self, t: &Track, init: bool) -> Vec<u8> {
        let first_audio = t.id == 2;
        let (media_dur, movie_dur) = if init { (0, 0) } else { (t.duration(), t.movie_duration()) };

        // tkhd. Only the mix is enabled among the audio tracks (alternate group 1),
        // so players play it alone instead of every track at once.
        let mut tkhd = Vec::new();
        tkhd.put_u32(0);
        tkhd.put_u32(0);
        tkhd.put_u32(t.id);
        tkhd.put_u32(0);
        tkhd.put_u32(movie_dur as u32);
        tkhd.put_u64(0);
        tkhd.put_u16(0); // layer
        tkhd.put_u16(if t.video { 0 } else { 1 });
        tkhd.put_u16(if t.video { 0 } else { 0x0100 });
        tkhd.put_u16(0);
        put_matrix(&mut tkhd);
        let (w, h) = if t.video { (self.spec.width, self.spec.height) } else { (0, 0) };
        tkhd.put_u32(w << 16);
        tkhd.put_u32(h << 16);
        let flags = if t.video || first_audio { 3 } else { 2 };
        let mut trak = full_box(b"tkhd", 0, flags, &tkhd);

        // The audio may start a moment after the video (a clip starts on a video
        // keyframe): an empty edit puts it in its place.
        if t.offset > 0 {
            let empty = t.offset * MOVIE_TS as u64 / t.timescale as u64;
            let mut elst = Vec::new();
            elst.put_u32(2);
            elst.put_u32(empty as u32);
            elst.put_u32(u32::MAX); // media_time -1: empty
            elst.put_u32(0x0001_0000);
            elst.put_u32(movie_dur.saturating_sub(empty) as u32);
            elst.put_u32(0);
            elst.put_u32(0x0001_0000);
            trak.extend(make_box(b"edts", &full_box(b"elst", 0, 0, &elst)));
        }
        // Reordered video is shown `reorder` frames after it's decoded: the
        // file starts there.
        if t.video && self.spec.reorder > 0 {
            let mut elst = Vec::new();
            elst.put_u32(1);
            elst.put_u32(movie_dur as u32);
            elst.put_u32(self.spec.reorder * self.frame_ticks());
            elst.put_u32(0x0001_0000);
            trak.extend(make_box(b"edts", &full_box(b"elst", 0, 0, &elst)));
        }

        // The track's length in its own timescale, in 64 bits (version 1)
        // when it doesn't fit 32: the video's timescale is a multiple of its
        // frame rate (540 000 at 540 fps), so a recording longer than 2.2
        // hours at 540 fps (7.2 at 165, 13.25 at 60) said it was a fraction
        // of its length.
        let long = media_dur > u32::MAX as u64;
        let mut mdhd = Vec::new();
        if long {
            mdhd.put_u64(0);
            mdhd.put_u64(0);
            mdhd.put_u32(t.timescale);
            mdhd.put_u64(media_dur);
        } else {
            mdhd.put_u32(0);
            mdhd.put_u32(0);
            mdhd.put_u32(t.timescale);
            mdhd.put_u32(media_dur as u32);
        }
        mdhd.put_u16(0x55C4); // "und"
        mdhd.put_u16(0);
        let mut mdia = full_box(b"mdhd", long as u8, 0, &mdhd);
        mdia.extend(hdlr(if t.video { b"vide" } else { b"soun" }, &t.name));

        let mut minf = if t.video {
            full_box(b"vmhd", 0, 1, &[0; 8])
        } else {
            full_box(b"smhd", 0, 0, &[0; 4])
        };
        let mut dref = Vec::new();
        dref.put_u32(1);
        dref.extend(full_box(b"url ", 0, 1, &[]));
        minf.extend(make_box(b"dinf", &full_box(b"dref", 0, 0, &dref)));
        minf.extend(self.stbl(t));
        mdia.extend(make_box(b"minf", &minf));
        trak.extend(make_box(b"mdia", &mdia));
        make_box(b"trak", &trak)
    }

    fn stbl(&self, t: &Track) -> Vec<u8> {
        let mut stsd = Vec::new();
        stsd.put_u32(1);
        stsd.extend(if t.video { self.avc1() } else { self.mp4a() });
        let mut stbl = full_box(b"stsd", 0, 0, &stsd);

        // Run-length sample durations.
        let mut runs: Vec<(u32, u32)> = Vec::new();
        for &d in &t.durations {
            match runs.last_mut() {
                Some((n, delta)) if *delta == d => *n += 1,
                _ => runs.push((1, d)),
            }
        }
        let mut stts = Vec::new();
        stts.put_u32(runs.len() as u32);
        for (n, d) in runs {
            stts.put_u32(n);
            stts.put_u32(d);
        }
        stbl.extend(full_box(b"stts", 0, 0, &stts));

        if !t.shown_after.is_empty() {
            let mut runs: Vec<(u32, u32)> = Vec::new();
            for &d in &t.shown_after {
                match runs.last_mut() {
                    Some((n, after)) if *after == d => *n += 1,
                    _ => runs.push((1, d)),
                }
            }
            let mut ctts = Vec::new();
            ctts.put_u32(runs.len() as u32);
            for (n, d) in runs {
                ctts.put_u32(n);
                ctts.put_u32(d);
            }
            stbl.extend(full_box(b"ctts", 0, 0, &ctts));
        }

        if t.video {
            let mut stss = Vec::new();
            stss.put_u32(t.syncs.len() as u32);
            for &s in &t.syncs {
                stss.put_u32(s);
            }
            stbl.extend(full_box(b"stss", 0, 0, &stss));
        }

        let mut stsc_entries: Vec<(u32, u32)> = Vec::new(); // (first chunk, samples per chunk)
        for (i, &(_, n)) in t.chunks.iter().enumerate() {
            if stsc_entries.last().is_none_or(|&(_, last)| last != n) {
                stsc_entries.push((i as u32 + 1, n));
            }
        }
        let mut stsc = Vec::new();
        stsc.put_u32(stsc_entries.len() as u32);
        for (first, n) in stsc_entries {
            stsc.put_u32(first);
            stsc.put_u32(n);
            stsc.put_u32(1);
        }
        stbl.extend(full_box(b"stsc", 0, 0, &stsc));

        let mut stsz = Vec::new();
        stsz.put_u32(0);
        stsz.put_u32(t.sizes.len() as u32);
        for &s in &t.sizes {
            stsz.put_u32(s);
        }
        stbl.extend(full_box(b"stsz", 0, 0, &stsz));

        let wide = t.chunks.iter().any(|&(o, _)| o > u32::MAX as u64);
        let mut co = Vec::new();
        co.put_u32(t.chunks.len() as u32);
        for &(o, _) in &t.chunks {
            if wide { co.put_u64(o) } else { co.put_u32(o as u32) }
        }
        stbl.extend(full_box(if wide { b"co64" } else { b"stco" }, 0, 0, &co));
        make_box(b"stbl", &stbl)
    }

    fn avc1(&self) -> Vec<u8> {
        let mut b = vec![0u8; 6];
        b.put_u16(1); // data reference
        b.extend_from_slice(&[0; 16]);
        b.put_u16(self.spec.width as u16);
        b.put_u16(self.spec.height as u16);
        b.put_u32(0x0048_0000);
        b.put_u32(0x0048_0000);
        b.put_u32(0);
        b.put_u16(1); // frames per sample
        b.extend_from_slice(&[0; 32]); // compressor name
        b.put_u16(0x18);
        b.put_u16(0xFFFF);

        b.extend(make_box(b"avcC", &self.avc.record()));
        // BT.709, limited range — what the encoder is given.
        let mut colr = b"nclx".to_vec();
        colr.put_u16(1);
        colr.put_u16(1);
        colr.put_u16(1);
        colr.push(0);
        b.extend(make_box(b"colr", &colr));
        let mut pasp = Vec::new();
        pasp.put_u32(1);
        pasp.put_u32(1);
        b.extend(make_box(b"pasp", &pasp));
        make_box(b"avc1", &b)
    }

    fn mp4a(&self) -> Vec<u8> {
        let mut b = vec![0u8; 6];
        b.put_u16(1);
        b.put_u64(0);
        b.put_u16(2); // channels
        b.put_u16(16);
        b.put_u32(0);
        b.put_u32(AUDIO_RATE << 16);

        // AudioSpecificConfig: AAC-LC (2), 48 kHz (index 3), stereo (2).
        let asc = [0x11u8, 0x90];
        let dec_specific = descriptor(0x05, &asc);
        let mut dcd = vec![0x40, 0x15]; // MPEG-4 audio, audio stream
        dcd.extend_from_slice(&[0, 0x18, 0]); // buffer size
        dcd.put_u32(self.spec.audio_bitrate);
        dcd.put_u32(self.spec.audio_bitrate);
        dcd.extend(dec_specific);
        let mut es = Vec::new();
        es.put_u16(0);
        es.push(0);
        es.extend(descriptor(0x04, &dcd));
        es.extend(descriptor(0x06, &[0x02]));
        b.extend(full_box(b"esds", 0, 0, &descriptor(0x03, &es)));
        make_box(b"mp4a", &b)
    }
}

impl Track {
    fn new(id: u32, video: bool, timescale: u32, name: String) -> Self {
        Self {
            id,
            video,
            timescale,
            name,
            sizes: Vec::new(),
            durations: Vec::new(),
            syncs: Vec::new(),
            shown_after: Vec::new(),
            chunks: Vec::new(),
            offset: 0,
            pending: Vec::new(),
            decode_time: 0,
        }
    }
}

/// A timescale every common frame rate divides, so frame times are exact.
fn video_timescale(fps: u32) -> u32 {
    if fps > 0 && 90_000 % fps == 0 { 90_000 } else { fps.max(1) * 1000 }
}

fn mdat_header_len(data_len: u64) -> u64 {
    if data_len + 8 <= u32::MAX as u64 { 8 } else { 16 }
}

/// A track fragment for `t`'s pending samples, whose data starts `data_offset`
/// bytes after the start of the `moof`.
fn traf(t: &Track, data_offset: i32, reordered: bool) -> Vec<u8> {
    let mut tfhd = Vec::new();
    tfhd.put_u32(t.id);
    let mut b = full_box(b"tfhd", 0, 0x02_0000, &tfhd); // default-base-is-moof
    let mut tfdt = Vec::new();
    tfdt.put_u64(t.decode_time);
    b.extend(full_box(b"tfdt", 1, 0, &tfdt));
    // data offset, durations, sizes, and per-sample flags for video (and
    // how long after its decoding each is shown, where reordered)
    let flags = 0x001 | 0x100 | 0x200 | if t.video { 0x400 } else { 0 } | if reordered { 0x800 } else { 0 };
    let mut trun = Vec::new();
    trun.put_u32(t.pending.len() as u32);
    trun.put_u32(data_offset as u32);
    for (data, dur, sync, shown_after) in &t.pending {
        trun.put_u32(*dur);
        trun.put_u32(data.len() as u32);
        if t.video {
            trun.put_u32(if *sync { 0x0200_0000 } else { 0x0101_0000 });
        }
        if reordered {
            trun.put_u32(*shown_after);
        }
    }
    b.extend(full_box(b"trun", 0, flags, &trun));
    make_box(b"traf", &b)
}

fn mvhd(duration: u64, next_track: u32) -> Vec<u8> {
    let mut b = Vec::new();
    b.put_u32(0);
    b.put_u32(0);
    b.put_u32(MOVIE_TS);
    b.put_u32(duration as u32);
    b.put_u32(0x0001_0000); // rate
    b.put_u16(0x0100); // volume
    b.extend_from_slice(&[0; 10]);
    put_matrix(&mut b);
    b.extend_from_slice(&[0; 24]);
    b.put_u32(next_track);
    full_box(b"mvhd", 0, 0, &b)
}

fn hdlr(handler: &[u8; 4], name: &str) -> Vec<u8> {
    let mut b = Vec::new();
    b.put_u32(0);
    b.extend_from_slice(handler);
    b.extend_from_slice(&[0; 12]);
    b.extend_from_slice(name.as_bytes());
    b.push(0);
    full_box(b"hdlr", 0, 0, &b)
}

/// `udta/meta/ilst/©cmt` — iTunes-style comment, which ffprobe reports as
/// `comment`.
fn udta_comment(text: &str) -> Vec<u8> {
    let mut data = Vec::new();
    data.put_u32(1); // UTF-8
    data.put_u32(0);
    data.extend_from_slice(text.as_bytes());
    let item = make_box(&[0xA9, b'c', b'm', b't'], &make_box(b"data", &data));
    let mut hd = Vec::new();
    hd.put_u32(0);
    hd.extend_from_slice(b"mdir");
    hd.extend_from_slice(b"appl");
    hd.extend_from_slice(&[0; 8]);
    hd.push(0);
    let mut meta = full_box(b"hdlr", 0, 0, &hd);
    meta.extend(make_box(b"ilst", &item));
    make_box(b"udta", &full_box(b"meta", 0, 0, &meta))
}

fn put_matrix(b: &mut Vec<u8>) {
    for v in [0x0001_0000u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000] {
        b.put_u32(v);
    }
}

/// An MPEG-4 descriptor (tag, one-byte length — ours are all small).
fn descriptor(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut b = vec![tag, body.len() as u8];
    b.extend_from_slice(body);
    b
}

fn make_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(body.len() + 8);
    b.put_u32(body.len() as u32 + 8);
    b.extend_from_slice(kind);
    b.extend_from_slice(body);
    b
}

fn full_box(kind: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(body.len() + 4);
    b.put_u32((version as u32) << 24 | (flags & 0xFF_FFFF));
    b.extend_from_slice(body);
    make_box(kind, &b)
}

trait Put {
    fn put_u16(&mut self, v: u16);
    fn put_u32(&mut self, v: u32);
    fn put_u64(&mut self, v: u64);
}

impl Put for Vec<u8> {
    fn put_u16(&mut self, v: u16) {
        self.extend_from_slice(&v.to_be_bytes());
    }
    fn put_u32(&mut self, v: u32) {
        self.extend_from_slice(&v.to_be_bytes());
    }
    fn put_u64(&mut self, v: u64) {
        self.extend_from_slice(&v.to_be_bytes());
    }
}

// ---------------------------------------------------------------------------
// H.264 byte streams
// ---------------------------------------------------------------------------

/// An encoded frame converted for MP4: the NAL units as 4-byte-length-prefixed
/// AVCC, without parameter sets or access unit delimiters (those live in the
/// `avcC` box), plus the parameter sets if the frame carried them and whether
/// it's an IDR frame.
pub(crate) struct AvccFrame {
    pub data: Vec<u8>,
    pub config: Option<AvcConfig>,
    pub idr: bool,
}

/// Convert an Annex B access unit (start-code separated NAL units, as Media
/// Foundation encoders produce) to AVCC.
pub(crate) fn annexb_to_avcc(au: &[u8]) -> AvccFrame {
    let mut data = Vec::with_capacity(au.len() + 16);
    let (mut sps, mut pps, mut idr) = (None, None, false);
    for nal in nal_units(au) {
        let Some(&header) = nal.first() else { continue };
        match header & 0x1F {
            7 => sps = Some(nal.to_vec()),
            8 => pps = Some(nal.to_vec()),
            9 => {} // access unit delimiter
            t => {
                idr |= t == 5;
                data.put_u32(nal.len() as u32);
                data.extend_from_slice(nal);
            }
        }
    }
    let config = sps.zip(pps).map(|(sps, pps)| AvcConfig { sps, pps });
    AvccFrame { data, config, idr }
}

/// NAL units of an Annex B stream (3- or 4-byte start codes), without them.
fn nal_units(mut b: &[u8]) -> impl Iterator<Item = &[u8]> {
    std::iter::from_fn(move || {
        // Skip to just past the next start code.
        let start = find_start_code(b)?;
        b = &b[start..];
        let end = find_start_code(b).map_or(b.len(), |next| {
            // Back up over the start code itself (and a 4-byte one's extra zero).
            let mut e = next - 3;
            if e > 0 && b[e - 1] == 0 {
                e -= 1;
            }
            e
        });
        let nal = &b[..end];
        b = &b[end..];
        Some(nal)
    })
    .filter(|n| !n.is_empty())
}

/// Index just past the first `00 00 01` in `b`.
fn find_start_code(b: &[u8]) -> Option<usize> {
    b.windows(3).position(|w| w == [0, 0, 1]).map(|i| i + 3)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(mov: bool) -> Spec {
        Spec {
            width: 64,
            height: 32,
            fps: 60,
            audio_titles: vec!["Mix".into(), "Mic".into()],
            audio_bitrate: 192_000,
            comment: "hasteclip:mix=1".into(),
            mov,
            reorder: 0,
        }
    }

    fn avc() -> AvcConfig {
        AvcConfig { sps: vec![0x67, 100, 0, 40, 1, 2], pps: vec![0x68, 3, 4] }
    }

    /// Top-level boxes as (type, offset, size).
    fn top_level(bytes: &[u8]) -> Vec<([u8; 4], usize, usize)> {
        let mut out = Vec::new();
        let mut off = 0;
        while off + 8 <= bytes.len() {
            let mut size = u32::from_be_bytes(bytes[off..off + 4].try_into().unwrap()) as usize;
            if size == 1 {
                size = u64::from_be_bytes(bytes[off + 8..off + 16].try_into().unwrap()) as usize;
            }
            out.push((bytes[off + 4..off + 8].try_into().unwrap(), off, size));
            off += size;
        }
        assert_eq!(off, bytes.len(), "boxes cover the file exactly");
        out
    }

    fn find<'a>(b: &'a [u8], path: &[&[u8; 4]]) -> Option<&'a [u8]> {
        let mut cur = b;
        'outer: for (depth, kind) in path.iter().enumerate() {
            let mut off = 0;
            while off + 8 <= cur.len() {
                let size = u32::from_be_bytes(cur[off..off + 4].try_into().unwrap()) as usize;
                if &cur[off + 4..off + 8] == *kind {
                    cur = &cur[off + 8..off + size];
                    if depth + 1 < path.len() && **kind == *b"meta" {
                        cur = &cur[4..];
                    }
                    continue 'outer;
                }
                off += size.max(8);
            }
            return None;
        }
        Some(cur)
    }

    /// Write 3 s of 60 fps video (keyframe every second) and two audio tracks;
    /// return the file's bytes.
    fn write(fragmented: bool, audio_offset: i64) -> Vec<u8> {
        let path = std::env::temp_dir().join(format!("hc-mux-{fragmented}-{audio_offset}-{}.mp4", std::process::id()));
        let mut m = Muxer::create(&path, &spec(false), avc(), fragmented).unwrap();
        let mut next_audio = audio_offset;
        for n in 0..180 {
            let t = n as f64 / 60.0;
            m.video(Arc::from(vec![n as u8; 10 + n % 7]), t, n % 60 == 0).unwrap();
            while (next_audio as f64) / 48_000.0 <= t {
                for track in 0..2 {
                    m.audio(track, Arc::from(vec![0xA0 + track as u8; 20]), next_audio).unwrap();
                }
                next_audio += 1024;
            }
        }
        m.finish().unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        bytes
    }

    fn be32(b: &[u8], at: usize) -> u32 {
        u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
    }

    /// Every chunk offset points at the bytes that sample was written with.
    fn check_tables(bytes: &[u8]) {
        let boxes = top_level(bytes);
        let moov = boxes.iter().find(|b| &b.0 == b"moov").expect("one moov");
        let moov = &bytes[moov.1 + 8..moov.1 + moov.2];
        let mut off = 0;
        let mut tracks = 0;
        while off + 8 <= moov.len() {
            let size = be32(moov, off) as usize;
            if &moov[off + 4..off + 8] == b"trak" {
                tracks += 1;
                let trak = &moov[off + 8..off + size];
                let stbl = find(trak, &[b"mdia", b"minf", b"stbl"]).unwrap();
                let stsz = find(stbl, &[b"stsz"]).unwrap();
                let stco = find(stbl, &[b"stco"]).unwrap();
                let stsc = find(stbl, &[b"stsc"]).unwrap();
                let samples = be32(stsz, 8) as usize;
                assert!(samples > 0);
                // Expand stsc to samples-per-chunk.
                let chunks = be32(stco, 4) as usize;
                let entries = be32(stsc, 4) as usize;
                let mut per_chunk = vec![0u32; chunks];
                for e in 0..entries {
                    let first = be32(stsc, 8 + e * 12) as usize;
                    let n = be32(stsc, 12 + e * 12);
                    let last = if e + 1 < entries { be32(stsc, 8 + (e + 1) * 12) as usize } else { chunks + 1 };
                    for c in first..last {
                        per_chunk[c - 1] = n;
                    }
                }
                assert_eq!(per_chunk.iter().sum::<u32>() as usize, samples);
                let mut s = 0;
                for c in 0..chunks {
                    let mut at = be32(stco, 8 + c * 4) as usize;
                    for _ in 0..per_chunk[c] {
                        let size = be32(stsz, 12 + s * 4) as usize;
                        let first = bytes[at];
                        // Video samples are filled with their frame number, audio with A0/A1.
                        assert!(bytes[at..at + size].iter().all(|&x| x == first), "sample {s} intact");
                        at += size;
                        s += 1;
                    }
                }
            }
            off += size;
        }
        assert_eq!(tracks, 3);
    }

    #[test]
    fn plain_file_is_fast_start_and_consistent() {
        let bytes = write(false, 0);
        let kinds: Vec<[u8; 4]> = top_level(&bytes).iter().map(|b| b.0).collect();
        assert_eq!(kinds, [*b"ftyp", *b"moov", *b"mdat"]);
        check_tables(&bytes);
    }

    #[test]
    fn fragmented_file_becomes_plain_on_finish() {
        let bytes = write(true, 0);
        let kinds: Vec<[u8; 4]> = top_level(&bytes).iter().map(|b| b.0).collect();
        assert_eq!(kinds.first(), Some(b"ftyp"));
        assert_eq!(kinds.last(), Some(b"moov"));
        assert!(!kinds.contains(b"moof"), "fragments relabelled");
        assert_eq!(kinds.iter().filter(|k| *k == b"moov").count(), 1);
        assert!(kinds.iter().filter(|k| *k == b"mdat").count() >= 3, "a fragment per second");
        check_tables(&bytes);
    }

    /// A recording too long for its track's timescale in 32 bits (at 540
    /// fps, past 2.2 hours) says its whole length, in 64 bits; a short one
    /// keeps the 32-bit box every player knows.
    #[test]
    fn a_long_recording_says_its_length() {
        let mdhd = |frames: &[f64]| {
            let path = std::env::temp_dir().join(format!("hc-mux-long-{}-{}.mp4", frames.len(), std::process::id()));
            let mut m = Muxer::create(&path, &Spec { fps: 540, audio_titles: Vec::new(), ..spec(false) }, avc(), false).unwrap();
            for (n, &t) in frames.iter().enumerate() {
                m.video(Arc::from(vec![n as u8; 16]), t, n == 0).unwrap();
            }
            m.finish().unwrap();
            let bytes = std::fs::read(&path).unwrap();
            let _ = std::fs::remove_file(&path);
            let moov = top_level(&bytes).into_iter().find(|b| &b.0 == b"moov").unwrap();
            find(&bytes[moov.1..moov.1 + moov.2], &[b"moov", b"trak", b"mdia", b"mdhd"]).unwrap().to_vec()
        };
        // Three hours at 540 fps, a frame an hour (each frame's own length
        // fits; the track's doesn't): 5.8 billion of its 540 000ths.
        let long = mdhd(&[0.0, 3600.0, 7200.0, 10800.0]);
        assert_eq!(long[0], 1, "version 1");
        assert_eq!(u32::from_be_bytes(long[20..24].try_into().unwrap()), 540_000);
        let ticks = u64::from_be_bytes(long[24..32].try_into().unwrap());
        assert_eq!(ticks, 3 * 3600 * 540_000 + 1000, "three hours and a frame");
        // A minute: version 0, as before.
        let short = mdhd(&[0.0, 30.0, 60.0]);
        assert_eq!(short[0], 0);
        assert_eq!(u32::from_be_bytes(short[16..20].try_into().unwrap()), 60 * 540_000 + 1000);
    }

    #[test]
    fn names_comment_and_audio_offset() {
        let bytes = write(false, 300);
        let moov = top_level(&bytes).into_iter().find(|b| &b.0 == b"moov").unwrap();
        let moov = &bytes[moov.1..moov.1 + moov.2];
        let text = String::from_utf8_lossy(moov);
        assert!(text.contains("Video\0") && text.contains("Mix\0") && text.contains("Mic\0"));
        assert!(text.contains("hasteclip:mix=1"));
        assert!(text.contains("elst"), "audio starting late gets an empty edit");
    }

    /// B-frames, in the order they're decoded (each anchor before the
    /// B-frames shown before it, the middle one decoded first): every frame
    /// is shown at its own time, from 0, both ways of writing.
    #[test]
    fn reordered_frames_are_shown_at_their_times() {
        let order = [0, 4, 2, 1, 3, 8, 6, 5, 7, 11, 9, 10];
        for fragmented in [false, true] {
            let path = std::env::temp_dir().join(format!("hc-mux-reorder-{fragmented}-{}.mp4", std::process::id()));
            let spec = Spec { audio_titles: Vec::new(), reorder: 3, ..spec(false) };
            let mut m = Muxer::create(&path, &spec, avc(), fragmented).unwrap();
            for &n in &order {
                m.video(Arc::from(vec![n as u8; 10]), n as f64 / 60.0, n == 0).unwrap();
            }
            m.finish().unwrap();
            let bytes = std::fs::read(&path).unwrap();
            let _ = std::fs::remove_file(&path);
            let moov = top_level(&bytes).into_iter().find(|b| &b.0 == b"moov").unwrap();
            let moov = &bytes[moov.1 + 8..moov.1 + moov.2];
            let stbl = find(moov, &[b"trak", b"mdia", b"minf", b"stbl"]).unwrap();
            let (stts, ctts) = (find(stbl, &[b"stts"]).unwrap(), find(stbl, &[b"ctts"]).expect("offsets"));
            let elst = find(moov, &[b"trak", b"edts", b"elst"]).expect("an edit");
            let frame = 90_000 / 60;
            assert_eq!((be32(stts, 4), be32(stts, 8), be32(stts, 12)), (1, 12, frame), "one frame each, decoded in turn");
            let mut after = Vec::new();
            for e in 0..be32(ctts, 4) as usize {
                after.extend(std::iter::repeat_n(be32(ctts, 12 + e * 8), be32(ctts, 8 + e * 8) as usize));
            }
            let start = be32(elst, 12);
            let shown: Vec<u32> = after.iter().enumerate().map(|(k, a)| (k as u32 * frame + a - start) / frame).collect();
            assert_eq!(shown, order, "fragmented: {fragmented}");
        }
    }

    #[test]
    fn annexb_conversion() {
        let au = [0, 0, 0, 1, 9, 0xF0, 0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 1, 0x65, 7, 7, 7];
        let f = annexb_to_avcc(&au);
        assert!(f.idr);
        assert_eq!(f.config, Some(AvcConfig { sps: vec![0x67, 1, 2], pps: vec![0x68, 3] }));
        assert_eq!(f.data, [0, 0, 0, 4, 0x65, 7, 7, 7]);
        let p = annexb_to_avcc(&[0, 0, 1, 0x41, 9, 9, 0, 0, 1, 0x41, 8]);
        assert!(!p.idr && p.config.is_none());
        assert_eq!(p.data, [0, 0, 0, 3, 0x41, 9, 9, 0, 0, 0, 2, 0x41, 8]);
    }
}
