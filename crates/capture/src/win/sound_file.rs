//! A video file's sound, read in order a block at a time (Media Foundation's
//! source reader, decoding to float samples at the file's own rate and
//! channels; the mixer's feed converts them), for a video layer to play
//! along with its picture (`layers::VideoFeed`). Never all of it at once: a
//! long video's sound would fill memory.

use std::path::Path;

use anyhow::{Context, Result};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::core::{GUID, HSTRING};

use super::system::{com_init, mf_startup};

const STREAM: u32 = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;
/// Media Foundation's time unit: 100 ns.
const UNITS: f64 = 10_000_000.0;

pub(crate) struct SoundReader {
    reader: IMFSourceReader,
    pub rate: u32,
    pub channels: u32,
    /// Its first block's time, which its times count from (so its first
    /// sound goes with the video's first picture).
    first: Option<i64>,
}

// The reader is used from one thread at a time (the video layer's).
unsafe impl Send for SoundReader {}

impl SoundReader {
    /// Open `path`'s first sound: an error if it has none, or it can't be
    /// decoded.
    pub(crate) fn open(path: &Path) -> Result<Self> {
        com_init();
        mf_startup()?;
        unsafe {
            let mut attrs = None;
            MFCreateAttributes(&mut attrs, 1)?;
            let attrs = attrs.context("no attributes")?;
            let reader = MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), &attrs).context("couldn't open the file")?;
            reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
            reader.SetStreamSelection(STREAM, true).context("it has no sound")?;
            let want = MFCreateMediaType()?;
            want.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            want.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_Float)?;
            reader.SetCurrentMediaType(STREAM, None, &want).context("its sound can't be decoded")?;
            let now = reader.GetCurrentMediaType(STREAM)?;
            let rate = now.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND)?;
            let channels = now.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS)?;
            anyhow::ensure!(rate > 0 && channels > 0, "its sound has no rate or channels");
            Ok(Self { reader, rate, channels, first: None })
        }
    }

    /// The next block of sound: when it starts (seconds after the first),
    /// and its samples, interleaved. `None` at its end.
    pub(crate) fn next(&mut self) -> Result<Option<(f64, Vec<f32>)>> {
        loop {
            let mut flags = 0u32;
            let mut time = 0i64;
            let mut sample = None;
            unsafe { self.reader.ReadSample(STREAM, 0, None, Some(&mut flags), Some(&mut time), Some(&mut sample))? };
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                return Ok(None);
            }
            let Some(sample) = sample else { continue };
            let first = *self.first.get_or_insert(time);
            let samples = unsafe {
                let buffer = sample.ConvertToContiguousBuffer()?;
                let mut ptr = std::ptr::null_mut();
                let mut len = 0u32;
                buffer.Lock(&mut ptr, None, Some(&mut len))?;
                let out = std::slice::from_raw_parts(ptr.cast::<f32>(), len as usize / 4).to_vec();
                buffer.Unlock()?;
                out
            };
            return Ok(Some(((time - first).max(0) as f64 / UNITS, samples)));
        }
    }

    /// Back to its start.
    pub(crate) fn rewind(&mut self) -> Result<()> {
        let pos = PROPVARIANT::from(0i64);
        unsafe { self.reader.SetCurrentPosition(&GUID::zeroed(), &pos)? };
        Ok(())
    }
}
