//! AAC encoding with the Media Foundation AAC encoder (raw AAC-LC packets; the
//! MP4 writer describes the stream itself).

use std::mem::ManuallyDrop;

use anyhow::{Context, Result, bail};
use windows::Win32::Media::MediaFoundation::*;

use super::h264::take_activates;
use crate::mixer::RATE;
use crate::writer::AacPacket;

/// Frames per AAC packet.
const AAC_FRAMES: i64 = 1024;

/// Streaming stereo 48 kHz float → AAC-LC encoder.
pub(crate) struct AacEncoder {
    mft: IMFTransform,
    /// Frame of the next packet out; packets are numbered by output order, so
    /// timing stays contiguous across the encoder's fixed delay.
    next_frame: i64,
    /// Frames handed to the encoder so far (input timestamps).
    fed: i64,
    out_size: u32,
}

// The encoder is only used from one thread at a time (its AAC thread).
unsafe impl Send for AacEncoder {}

impl AacEncoder {
    pub(crate) fn new(bitrate: u32) -> Result<Self> {
        super::system::com_init();
        super::system::mf_startup()?;
        let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Audio, guidSubtype: MFAudioFormat_PCM };
        let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Audio, guidSubtype: MFAudioFormat_AAC };
        let mut list = std::ptr::null_mut();
        let mut count = 0u32;
        let activates = unsafe {
            MFTEnumEx(
                MFT_CATEGORY_AUDIO_ENCODER,
                MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_LOCALMFT | MFT_ENUM_FLAG_SORTANDFILTER,
                Some(&input),
                Some(&output),
                &mut list,
                &mut count,
            )
            .context("no AAC encoder")?;
            take_activates(list, count)
        };
        let activate = activates.first().context("no AAC encoder on this PC")?;
        let mft: IMFTransform = unsafe { activate.ActivateObject()? };

        // The encoder only offers 96, 128, 160 and 192 kbit/s.
        let bytes_per_sec = [12_000u32, 16_000, 20_000, 24_000]
            .into_iter()
            .min_by_key(|b| (*b as i64 - bitrate as i64 / 8).abs())
            .unwrap();
        unsafe {
            let out = MFCreateMediaType()?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            out.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
            out.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            out.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, RATE)?;
            out.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)?;
            out.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, bytes_per_sec)?;
            out.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)?; // raw packets
            out.SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, 0x29)?; // AAC-LC

            let pcm = MFCreateMediaType()?;
            pcm.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            pcm.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
            pcm.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            pcm.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, RATE)?;
            pcm.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)?;
            pcm.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 4)?;
            pcm.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, RATE * 4)?;

            // The encoder wants its input type first.
            mft.SetInputType(0, &pcm, 0).context("AAC encoder rejected the input format")?;
            mft.SetOutputType(0, &out, 0).context("AAC encoder rejected the output format")?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            let out_size = mft.GetOutputStreamInfo(0)?.cbSize.max(1536);
            Ok(Self { mft, next_frame: 0, fed: 0, out_size })
        }
    }

    /// Feed interleaved stereo samples; returns the packets that are complete.
    pub(crate) fn push(&mut self, samples: &[f32]) -> Vec<AacPacket> {
        let mut out = Vec::new();
        if let Err(e) = self.feed(samples, &mut out) {
            eprintln!("AAC encoder: {e:#}");
        }
        out
    }

    fn feed(&mut self, samples: &[f32], out: &mut Vec<AacPacket>) -> Result<()> {
        let frames = (samples.len() / 2) as i64;
        if frames == 0 {
            return Ok(());
        }
        unsafe {
            let bytes = (frames * 4) as u32;
            let buffer = MFCreateMemoryBuffer(bytes)?;
            let mut dst = std::ptr::null_mut();
            buffer.Lock(&mut dst, None, None)?;
            let dst = std::slice::from_raw_parts_mut(dst as *mut i16, samples.len());
            for (d, s) in dst.iter_mut().zip(samples) {
                *d = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
            }
            buffer.Unlock()?;
            buffer.SetCurrentLength(bytes)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(self.fed * 10_000_000 / RATE as i64)?;
            sample.SetSampleDuration(frames * 10_000_000 / RATE as i64)?;
            self.fed += frames;
            self.mft.ProcessInput(0, &sample, 0)?;
            while let Some(data) = self.output()? {
                out.push(AacPacket { data, frame: self.next_frame });
                self.next_frame += AAC_FRAMES;
            }
        }
        Ok(())
    }

    /// One encoded packet, or None when the encoder needs more input.
    fn output(&mut self) -> Result<Option<Vec<u8>>> {
        unsafe {
            let sample = MFCreateSample()?;
            sample.AddBuffer(&MFCreateMemoryBuffer(self.out_size)?)?;
            let mut buf = [MFT_OUTPUT_DATA_BUFFER { dwStreamID: 0, pSample: ManuallyDrop::new(Some(sample)), dwStatus: 0, pEvents: ManuallyDrop::new(None) }];
            let mut status = 0u32;
            let result = self.mft.ProcessOutput(0, &mut buf, &mut status);
            let sample = ManuallyDrop::take(&mut buf[0].pSample);
            drop(ManuallyDrop::take(&mut buf[0].pEvents));
            match result {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
                Err(e) => bail!(e),
            }
            let Some(sample) = sample else { return Ok(None) };
            let buffer = sample.ConvertToContiguousBuffer()?;
            let mut ptr = std::ptr::null_mut();
            let mut len = 0u32;
            buffer.Lock(&mut ptr, None, Some(&mut len))?;
            let data = std::slice::from_raw_parts(ptr, len as usize).to_vec();
            buffer.Unlock()?;
            Ok(Some(data))
        }
    }
}
