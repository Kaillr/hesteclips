//! Frames handed straight to the H.264 decoder, read from the file by our own
//! index (`mp4read`), instead of through Media Foundation's file reader.
//! Measured on a quiet machine (decode_bench `DIRECT=1`, 1440p60, pictures
//! converted to 640 px): a jump to a keyframe 15 ms instead of 45, halfway
//! between keyframes 33-66 ms instead of 57-110, reading in order the same
//! to 10% slower (750 fps against 850). Pictures are the same, bit for bit:
//! checked against ffmpeg, and against the reader on every library clip
//! (decode_bench `VERIFY=n`).
//!
//! The decoder is Media Foundation's H.264 decoder on the GPU (DXVA), the one
//! the reader uses too, in its normal mode: low-latency mode (each frame out
//! as soon as it's decoded) made jumps slower, 25-35 ms instead of 15-20, and
//! reading in order 600 fps instead of 850 (measured on a quiet machine).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::mem::ManuallyDrop;
use std::path::Path;

use anyhow::{Context, Result, bail};
use windows::Win32::Media::MediaFoundation::*;
use windows::core::Interface;

use super::mp4read::Index;

/// Media Foundation's time unit: 100 ns.
const UNITS: i64 = 10_000_000;
/// The decoder itself, owned by the thread of a [`Stream`].
struct Inner {
    mft: IMFTransform,
    index: Index,
    file: File,
    /// The next sample (decode order) to hand the decoder.
    next: usize,
    /// Told the decoder there's no more input (at the end of the file).
    drained: bool,
    /// Sample (decode order) of each keyframe, ascending.
    keys: Vec<usize>,
    /// A frame's length in 100 ns, for sample durations.
    frame_units: i64,
    /// Frames come out in a different order than they go in (B-frames).
    reordered: bool,
    /// The file read in blocks (frames are stored one after another): one
    /// read per few dozen frames, not per frame.
    block: Vec<u8>,
    block_at: u64,
}

/// Bytes read from the file at a time.
const BLOCK: usize = 4 << 20;

impl Inner {
    /// The decoder for `path`'s video, on the GPU of `manager`.
    fn open(path: &Path, manager: &IMFDXGIDeviceManager) -> Result<Self> {
        let index = Index::read(path)?;
        if index.samples.is_empty() {
            bail!("no video frames");
        }
        let reordered = index.samples.windows(2).any(|w| w[1].pts < w[0].pts);
        unsafe {
            let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
            let mut list = std::ptr::null_mut();
            let mut count = 0u32;
            let flags = MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_LOCALMFT | MFT_ENUM_FLAG_SORTANDFILTER;
            MFTEnumEx(MFT_CATEGORY_VIDEO_DECODER, flags, Some(&input), None, &mut list, &mut count)?;
            let found = super::h264::take_activates(list, count);
            let mft: IMFTransform = found.first().context("no H.264 decoder")?.ActivateObject()?;
            let attrs = mft.GetAttributes()?;
            if attrs.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) == 0 {
                bail!("the H.264 decoder can't use the graphics card");
            }
            mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)?;
            let t = MFCreateMediaType()?;
            t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            t.SetUINT64(&MF_MT_FRAME_SIZE, (index.width as u64) << 32 | index.height as u64)?;
            t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            mft.SetInputType(0, &t, 0).context("the decoder rejected the video")?;
            set_nv12(&mft)?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            let keys = index.keyframes();
            if keys.first() != Some(&0) {
                bail!("the video doesn't start with a keyframe");
            }
            let frame_units = (index.samples.len() > 1)
                .then(|| (index.samples[1].pts - index.samples[0].pts).abs() * UNITS / index.timescale.max(1) as i64)
                .filter(|&d| d > 0)
                .unwrap_or(UNITS / 60);
            Ok(Self { mft, file: File::open(path)?, index, next: 0, drained: false, keys, frame_units, reordered, block: Vec::new(), block_at: 0 })
        }
    }

    /// Frames per second, from the frame times (the caller usually knows better).
    fn fps(&self) -> f64 {
        UNITS as f64 / self.frame_units as f64
    }

    fn time_of(&self, sample: usize) -> i64 {
        self.index.samples[sample].pts * UNITS / self.index.timescale.max(1) as i64
    }

    /// The keyframe (sample) to start from for time `t` (100 ns): the last one
    /// at or before it.
    fn key_for(&self, t: i64) -> usize {
        let k = self.keys.partition_point(|&k| self.time_of(k) <= t);
        self.keys[k.saturating_sub(1)]
    }

    /// Start again from the keyframe at or before time `t` (100 ns).
    fn seek(&mut self, t: i64) -> Result<()> {
        unsafe { self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)? };
        if self.drained {
            unsafe { self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)? };
            self.drained = false;
        }
        self.next = self.key_for(t);
        Ok(())
    }

    /// The next decoded frame in presentation order: (time in 100 ns, sample
    /// holding its texture). `None` at the end.
    fn read(&mut self) -> Result<Option<(i64, IMFSample)>> {
        // A decoder that takes frames without ever giving one back (out of
        // its own picture buffers): an error, not a hang.
        let mut fed = 0;
        loop {
            if let Some(out) = self.output()? {
                let t = unsafe { out.GetSampleTime()? };
                return Ok(Some((t, out)));
            }
            if self.next < self.index.samples.len() {
                if fed > 64 {
                    bail!("the decoder stopped giving frames back");
                }
                if self.feed(self.next)? {
                    self.next += 1;
                    fed += 1;
                } else {
                    bail!("the decoder won't take a frame and has none to give");
                }
            } else if !self.drained {
                unsafe { self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)? };
                self.drained = true;
            } else {
                return Ok(None);
            }
        }
    }

    /// Hand sample `i` to the decoder; false if it wants its output taken first.
    fn feed(&mut self, i: usize) -> Result<bool> {
        let s = self.index.samples[i];
        let (start, end) = (s.offset, s.offset + s.size as u64);
        if start < self.block_at || end > self.block_at + self.block.len() as u64 {
            let len = (s.size as usize).max(BLOCK);
            let file_len = self.file.metadata()?.len();
            let len = len.min((file_len.saturating_sub(start)) as usize).max(s.size as usize);
            self.block.resize(len, 0);
            self.file.seek(SeekFrom::Start(start))?;
            self.file.read_exact(&mut self.block)?;
            self.block_at = start;
        }
        let at = (start - self.block_at) as usize;
        let data = &self.block[at..at + s.size as usize];
        // Start codes in place of the length prefixes, plus 4 bytes per
        // parameter set on a keyframe: written straight into the decoder's buffer.
        let extra: usize = if s.key { self.index.sps.iter().chain(&self.index.pps).map(|p| p.len() + 4).sum() } else { 0 };
        let room = data.len() + extra + 4 * (data.len() / (self.index.nal_length + 1) + 1);
        unsafe {
            let input = MFCreateSample()?;
            let buffer = MFCreateMemoryBuffer(room as u32)?;
            let mut ptr = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None)?;
            let out = std::slice::from_raw_parts_mut(ptr, room);
            let written = annexb_into(&self.index, data, s.key, out);
            buffer.Unlock()?;
            buffer.SetCurrentLength(written as u32)?;
            input.AddBuffer(&buffer)?;
            input.SetSampleTime(self.time_of(i))?;
            input.SetSampleDuration(self.frame_units)?;
            if s.key {
                input.SetUINT32(&MFSampleExtension_CleanPoint, 1)?;
            }
            match self.mft.ProcessInput(0, &input, 0) {
                Ok(()) => Ok(true),
                Err(e) if e.code() == MF_E_NOTACCEPTING => Ok(false),
                Err(e) => Err(e.into()),
            }
        }
    }

    fn output(&self) -> Result<Option<IMFSample>> {
        unsafe {
            loop {
                let info = self.mft.GetOutputStreamInfo(0)?;
                let provides = info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0) as u32 != 0;
                if !provides {
                    bail!("the decoder doesn't hand out GPU frames");
                }
                let mut buf = [MFT_OUTPUT_DATA_BUFFER { dwStreamID: 0, pSample: ManuallyDrop::new(None), dwStatus: 0, pEvents: ManuallyDrop::new(None) }];
                let mut status = 0u32;
                let result = self.mft.ProcessOutput(0, &mut buf, &mut status);
                let sample = ManuallyDrop::take(&mut buf[0].pSample);
                drop(ManuallyDrop::take(&mut buf[0].pEvents));
                match result {
                    Ok(()) => return Ok(sample),
                    Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
                    Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => set_nv12(&self.mft)?,
                    Err(e) => return Err(e.into()),
                }
            }
        }
    }
}

/// A decoded frame crossing from the decoder's thread (Media Foundation
/// objects are free-threaded: the app runs in the multithreaded apartment).
struct Frame(i64, IMFSample);
unsafe impl Send for Frame {}

struct SendManager(IMFDXGIDeviceManager);
unsafe impl Send for SendManager {}

enum Cmd {
    Seek(i64),
    Read,
}

/// The direct decoder, decoding on its own thread one frame ahead of the
/// reader: the next frame decodes while the caller converts this one, as
/// Media Foundation's file reader does (reading in order: 720 fps without
/// it, 750-780 with, the reader 850).
/// Only one frame ahead: holding more ran the decoder out of its own
/// picture buffers.
pub(crate) struct Stream {
    pub(crate) index: Index,
    keys: Vec<usize>,
    frame_units: i64,
    reordered: bool,
    fps: f64,
    cmd: std::sync::mpsc::Sender<Cmd>,
    frames: std::sync::mpsc::Receiver<Result<Option<Frame>>>,
    /// Time of the last frame handed out.
    last: Option<i64>,
}

impl Stream {
    /// The decoder for `path`'s video, on the GPU of `manager`.
    pub(crate) fn open(path: &Path, manager: &IMFDXGIDeviceManager) -> Result<Self> {
        let (cmd, cmds) = std::sync::mpsc::channel::<Cmd>();
        let (frames_tx, frames) = std::sync::mpsc::channel::<Result<Option<Frame>>>();
        let (opened_tx, opened) = std::sync::mpsc::channel::<Result<(Index, Vec<usize>, i64, bool, f64)>>();
        let (path, manager) = (path.to_path_buf(), SendManager(manager.clone()));
        std::thread::Builder::new().name("direct decode".into()).spawn(move || {
            super::system::com_init();
            let manager = manager;
            let mut inner = match Inner::open(&path, &manager.0) {
                Ok(i) => {
                    let _ = opened_tx.send(Ok((i.index.clone(), i.keys.clone(), i.frame_units, i.reordered, i.fps())));
                    i
                }
                Err(e) => {
                    let _ = opened_tx.send(Err(e));
                    return;
                }
            };
            let mut ahead: Option<Result<Option<(i64, IMFSample)>>> = None;
            let mut failed: Option<anyhow::Error> = None;
            while let Ok(c) = cmds.recv() {
                match c {
                    Cmd::Seek(t) => {
                        ahead = None;
                        if let Err(e) = inner.seek(t) {
                            failed = Some(e);
                        }
                    }
                    Cmd::Read => {
                        let r = match failed.take() {
                            Some(e) => Err(e),
                            None => ahead.take().unwrap_or_else(|| inner.read()),
                        };
                        let more = matches!(r, Ok(Some(_)));
                        if frames_tx.send(r.map(|o| o.map(|(t, s)| Frame(t, s)))).is_err() {
                            return;
                        }
                        // The next one while the caller converts this one.
                        if more {
                            ahead = Some(inner.read());
                        }
                    }
                }
            }
        })?;
        let (index, keys, frame_units, reordered, fps) = opened.recv().context("the decoder thread stopped")??;
        Ok(Self { index, keys, frame_units, reordered, fps, cmd, frames, last: None })
    }

    /// Frames per second, from the frame times (the caller usually knows better).
    pub(crate) fn fps(&self) -> f64 {
        self.fps
    }

    fn time_of(&self, sample: usize) -> i64 {
        self.index.samples[sample].pts * UNITS / self.index.timescale.max(1) as i64
    }

    fn key_for(&self, t: i64) -> usize {
        let k = self.keys.partition_point(|&k| self.time_of(k) <= t);
        self.keys[k.saturating_sub(1)]
    }

    /// Whether reaching time `t` from time `from` (the next frame's) is
    /// cheaper decoding on than starting again from `t`'s keyframe: unless
    /// `t` is past the next keyframe, decoding on wins.
    pub(crate) fn decode_on(&self, from: i64, t: i64) -> bool {
        from <= t && self.time_of(self.key_for(t)) <= from + 3 * self.frame_units
    }

    /// Start again from the keyframe at or before time `t` (100 ns).
    pub(crate) fn seek(&mut self, t: i64) -> Result<()> {
        self.last = None;
        self.cmd.send(Cmd::Seek(t)).ok().context("the decoder thread stopped")
    }

    /// The next decoded frame in presentation order: (time in 100 ns, sample
    /// holding its texture). `None` at the end.
    pub(crate) fn read(&mut self) -> Result<Option<(i64, IMFSample)>> {
        self.cmd.send(Cmd::Read).ok().context("the decoder thread stopped")?;
        let got = self.frames.recv().context("the decoder thread stopped")??;
        self.last = got.as_ref().map(|f| f.0);
        Ok(got.map(|Frame(t, s)| (t, s)))
    }

    /// The time of the frame the next read returns, when that's known
    /// without decoding it (frames not reordered: the one after the last
    /// handed out, by the file's index); `i64::MAX` at the end.
    pub(crate) fn upcoming(&self) -> Option<i64> {
        if self.reordered {
            return None;
        }
        let last = self.last?;
        let next = self.index.samples.partition_point(|s| s.pts * UNITS / self.index.timescale.max(1) as i64 <= last);
        Some(if next < self.index.samples.len() { self.time_of(next) } else { i64::MAX })
    }
}

/// The decoder's input into `out`: NAL units with start codes, the SPS/PPS
/// first on a keyframe. Returns the bytes written.
fn annexb_into(index: &Index, avcc: &[u8], key: bool, out: &mut [u8]) -> usize {
    let mut w = 0;
    let mut put = |bytes: &[u8], w: &mut usize| {
        let end = (*w + bytes.len()).min(out.len());
        out[*w..end].copy_from_slice(&bytes[..end - *w]);
        *w = end;
    };
    if key {
        for p in index.sps.iter().chain(&index.pps) {
            put(&[0, 0, 0, 1], &mut w);
            put(p, &mut w);
        }
    }
    let n = index.nal_length;
    let mut at = 0;
    while at + n <= avcc.len() {
        let len = avcc[at..at + n].iter().fold(0usize, |a, &b| a << 8 | b as usize);
        at += n;
        let end = (at + len).min(avcc.len());
        put(&[0, 0, 0, 1], &mut w);
        put(&avcc[at..end], &mut w);
        at = end;
    }
    w
}

fn set_nv12(mft: &IMFTransform) -> Result<()> {
    unsafe {
        let mut i = 0;
        while let Ok(t) = mft.GetOutputAvailableType(0, i) {
            if t.GetGUID(&MF_MT_SUBTYPE)? == MFVideoFormat_NV12 {
                mft.SetOutputType(0, &t, 0)?;
                return Ok(());
            }
            i += 1;
        }
    }
    bail!("the decoder can't give NV12")
}
