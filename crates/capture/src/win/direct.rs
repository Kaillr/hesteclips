//! Frames handed straight to the H.264 decoder, read from the file by our own
//! index (`mp4read`), instead of through Media Foundation's file reader. A
//! jump to a keyframe takes ~5 ms this way, ~45 through the reader (whose seek
//! is most of it); halfway between keyframes about half as long. Pictures are
//! the same, bit for bit (checked against ffmpeg and the reader:
//! decode_bench `VERIFY=1`).
//!
//! The decoder is Media Foundation's H.264 decoder on the GPU (DXVA), the one
//! the reader uses too. Without reordered frames (no B-frames, as recorded) it
//! runs in low-latency mode: each frame comes out as soon as it's decoded.

use std::collections::VecDeque;
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
/// Frames decoded ahead while reading in order, so the GPU decodes the next
/// while the caller converts this one, as the file reader does. Whether it
/// helps is still to be measured on a quiet machine (with a game running,
/// in-order reading was 1.6-2× slower than the reader's either way).
const AHEAD: usize = 3;

pub(crate) struct Stream {
    mft: IMFTransform,
    pub(crate) index: Index,
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
    /// Decoded, not yet handed out.
    ahead: VecDeque<(i64, IMFSample)>,
    /// Reads since the last seek: from a few on, reading is in order.
    streak: u32,
}

impl Stream {
    /// The decoder for `path`'s video, on the GPU of `manager`.
    pub(crate) fn open(path: &Path, manager: &IMFDXGIDeviceManager) -> Result<Self> {
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
            if !reordered {
                attrs.SetUINT32(&MF_LOW_LATENCY, 1)?;
            }
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
            Ok(Self { mft, file: File::open(path)?, index, next: 0, drained: false, keys, frame_units, reordered, ahead: VecDeque::new(), streak: 0 })
        }
    }

    /// Frames per second, from the frame times (the caller usually knows better).
    pub(crate) fn fps(&self) -> f64 {
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

    /// Whether reaching time `t` from time `from` (the next frame's) is
    /// cheaper decoding on than starting again from `t`'s keyframe: unless
    /// `t` is past the next keyframe, decoding on wins (a restart costs a few
    /// ms, a frame ~1.5).
    pub(crate) fn decode_on(&self, from: i64, t: i64) -> bool {
        from <= t && self.time_of(self.key_for(t)) <= from + 3 * self.frame_units
    }

    /// Start again from the keyframe at or before time `t` (100 ns).
    pub(crate) fn seek(&mut self, t: i64) -> Result<()> {
        unsafe { self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)? };
        if self.drained {
            unsafe { self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)? };
            self.drained = false;
        }
        self.next = self.key_for(t);
        self.ahead.clear();
        self.streak = 0;
        Ok(())
    }

    /// The time of the frame the next read returns, when that's known
    /// without decoding it (frames not reordered); `i64::MAX` at the end.
    pub(crate) fn upcoming(&self) -> Option<i64> {
        if let Some((t, _)) = self.ahead.front() {
            return Some(*t);
        }
        if self.reordered {
            return None;
        }
        Some(if self.next < self.index.samples.len() { self.time_of(self.next) } else { i64::MAX })
    }

    /// The next decoded frame in presentation order: (time in 100 ns, sample
    /// holding its texture). `None` at the end.
    pub(crate) fn read(&mut self) -> Result<Option<(i64, IMFSample)>> {
        self.streak += 1;
        if self.streak >= 3 {
            while self.ahead.len() < AHEAD {
                match self.decode_one()? {
                    Some(f) => self.ahead.push_back(f),
                    None => break,
                }
            }
        }
        match self.ahead.pop_front() {
            Some(f) => Ok(Some(f)),
            None => self.decode_one(),
        }
    }

    fn decode_one(&mut self) -> Result<Option<(i64, IMFSample)>> {
        loop {
            if let Some(out) = self.output()? {
                return Ok(Some((unsafe { out.GetSampleTime()? }, out)));
            }
            if self.next < self.index.samples.len() {
                if self.feed(self.next)? {
                    self.next += 1;
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
        let mut data = vec![0u8; s.size as usize];
        self.file.seek(SeekFrom::Start(s.offset))?;
        self.file.read_exact(&mut data)?;
        let annexb = annexb(&self.index, &data, s.key);
        unsafe {
            let input = MFCreateSample()?;
            let buffer = MFCreateMemoryBuffer(annexb.len() as u32)?;
            let mut ptr = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None)?;
            std::ptr::copy_nonoverlapping(annexb.as_ptr(), ptr, annexb.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(annexb.len() as u32)?;
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

/// The decoder's input: NAL units with start codes, the SPS/PPS first on a keyframe.
fn annexb(index: &Index, avcc: &[u8], key: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(avcc.len() + 64);
    if key {
        for p in index.sps.iter().chain(&index.pps) {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(p);
        }
    }
    let n = index.nal_length;
    let mut at = 0;
    while at + n <= avcc.len() {
        let len = avcc[at..at + n].iter().fold(0usize, |a, &b| a << 8 | b as usize);
        at += n;
        let end = (at + len).min(avcc.len());
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&avcc[at..end]);
        at = end;
    }
    out
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
