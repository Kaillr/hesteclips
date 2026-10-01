//! AAC encoding with Apple's AudioConverter (better than ffmpeg's built-in AAC
//! encoder at the same bitrate), producing compressed sample buffers that
//! AVAssetWriter passes through untouched.

use std::ffi::c_void;
use std::ptr::{self, NonNull};

use anyhow::{Result, bail};
use objc2_audio_toolbox::{
    AudioConverterDispose, AudioConverterFillComplexBuffer, AudioConverterGetProperty,
    AudioConverterGetPropertyInfo, AudioConverterNew, AudioConverterRef, AudioConverterSetProperty,
    kAudioConverterCompressionMagicCookie, kAudioConverterEncodeBitRate,
};
use objc2_core_audio_types::{
    AudioBuffer, AudioBufferList, AudioStreamBasicDescription, AudioStreamPacketDescription,
    kAudioFormatFlagIsFloat, kAudioFormatFlagIsPacked, kAudioFormatLinearPCM, kAudioFormatMPEG4AAC,
};
use objc2_core_foundation::CFRetained;
use objc2_core_media::{
    CMAudioFormatDescriptionCreate, CMAudioSampleBufferCreateReadyWithPacketDescriptions,
    CMBlockBuffer, CMFormatDescription, CMSampleBuffer, CMTime,
};

use crate::mixer::RATE;

/// Frames per AAC packet.
pub const AAC_FRAMES: usize = 1024;

/// One encoded AAC packet and the frame (48 kHz sample index) it starts at.
pub struct AacPacket {
    pub data: Vec<u8>,
    pub frame: i64,
}

/// Streaming stereo 48 kHz float → AAC-LC encoder.
pub struct AacEncoder {
    conv: AudioConverterRef,
    format: CFRetained<CMFormatDescription>,
    /// Interleaved stereo samples not yet handed to the converter.
    pending: Vec<f32>,
    /// Source frame of `pending[0]`, and of the next packet out.
    next_frame: i64,
}

// The converter is only used from one thread at a time (owned by the writer thread).
unsafe impl Send for AacEncoder {}

/// What the input callback reads from: exactly one packet's worth of PCM.
struct Feed {
    samples: *const f32,
    frames: u32,
    given: bool,
}

impl AacEncoder {
    pub fn new(bitrate: u32) -> Result<Self> {
        let mut input = pcm_format();
        let mut output = AudioStreamBasicDescription {
            mSampleRate: RATE as f64,
            mFormatID: kAudioFormatMPEG4AAC,
            mFormatFlags: 0,
            mBytesPerPacket: 0,
            mFramesPerPacket: AAC_FRAMES as u32,
            mBytesPerFrame: 0,
            mChannelsPerFrame: 2,
            mBitsPerChannel: 0,
            mReserved: 0,
        };
        let mut conv: AudioConverterRef = ptr::null_mut();
        let status = unsafe {
            AudioConverterNew(NonNull::from(&mut input), NonNull::from(&mut output), NonNull::from(&mut conv))
        };
        if status != 0 || conv.is_null() {
            bail!("couldn't create the AAC encoder (error {status})");
        }
        let mut br = bitrate;
        unsafe {
            AudioConverterSetProperty(conv, kAudioConverterEncodeBitRate, 4, NonNull::from(&mut br).cast());
        }
        // The magic cookie (AudioSpecificConfig) goes in the format description so
        // the file knows how to decode the packets.
        let cookie = unsafe {
            let mut size = 0u32;
            AudioConverterGetPropertyInfo(conv, kAudioConverterCompressionMagicCookie, &mut size, ptr::null_mut());
            let mut cookie = vec![0u8; size as usize];
            if size > 0 {
                AudioConverterGetProperty(
                    conv,
                    kAudioConverterCompressionMagicCookie,
                    NonNull::from(&mut size),
                    NonNull::new_unchecked(cookie.as_mut_ptr().cast()),
                );
            }
            cookie.truncate(size as usize);
            cookie
        };
        let mut desc: *const CMFormatDescription = ptr::null();
        let status = unsafe {
            CMAudioFormatDescriptionCreate(
                None,
                NonNull::from(&mut output),
                0,
                ptr::null(),
                cookie.len(),
                if cookie.is_empty() { ptr::null() } else { cookie.as_ptr().cast() },
                None,
                NonNull::from(&mut desc),
            )
        };
        let Some(desc) = NonNull::new(desc as *mut CMFormatDescription).filter(|_| status == 0) else {
            unsafe { AudioConverterDispose(conv) };
            bail!("couldn't describe the AAC format (error {status})");
        };
        Ok(Self { conv, format: unsafe { CFRetained::from_raw(desc) }, pending: Vec::new(), next_frame: 0 })
    }

    /// The format description every packet from this encoder uses.
    pub fn format(&self) -> &CMFormatDescription {
        &self.format
    }

    /// Feed interleaved stereo samples; returns the packets that are complete.
    pub fn push(&mut self, samples: &[f32]) -> Vec<AacPacket> {
        self.pending.extend_from_slice(samples);
        let mut out = Vec::new();
        while self.pending.len() >= AAC_FRAMES * 2 {
            if let Some(p) = self.encode_one() {
                out.push(p);
            }
            self.pending.drain(..AAC_FRAMES * 2);
        }
        out
    }

    fn encode_one(&mut self) -> Option<AacPacket> {
        let mut feed = Feed { samples: self.pending.as_ptr(), frames: AAC_FRAMES as u32, given: false };
        let mut buf = vec![0u8; 2048];
        let mut list = AudioBufferList {
            mNumberBuffers: 1,
            mBuffers: [AudioBuffer { mNumberChannels: 2, mDataByteSize: buf.len() as u32, mData: buf.as_mut_ptr().cast() }],
        };
        let mut packets = 1u32;
        let mut desc = AudioStreamPacketDescription { mStartOffset: 0, mVariableFramesInPacket: 0, mDataByteSize: 0 };
        let status = unsafe {
            AudioConverterFillComplexBuffer(
                self.conv,
                Some(input_proc),
                (&raw mut feed).cast(),
                NonNull::from(&mut packets),
                NonNull::from(&mut list),
                &mut desc,
            )
        };
        // The encoder has a fixed delay: the first call may return no packet yet.
        // Packets are numbered by output order, so timing stays contiguous.
        if status != 0 || packets == 0 {
            return None;
        }
        buf.truncate(list.mBuffers[0].mDataByteSize as usize);
        let frame = self.next_frame;
        self.next_frame += AAC_FRAMES as i64;
        Some(AacPacket { data: buf, frame })
    }
}

impl Drop for AacEncoder {
    fn drop(&mut self) {
        unsafe { AudioConverterDispose(self.conv) };
    }
}

/// AudioConverter pulls input through this: hand over the one packet of PCM once,
/// then report "no more for now".
unsafe extern "C-unwind" fn input_proc(
    _conv: AudioConverterRef,
    io_packets: NonNull<u32>,
    io_data: NonNull<AudioBufferList>,
    _desc: *mut *mut AudioStreamPacketDescription,
    user: *mut c_void,
) -> i32 {
    let feed = unsafe { &mut *(user as *mut Feed) };
    let list = unsafe { &mut *io_data.as_ptr() };
    if feed.given {
        unsafe { *io_packets.as_ptr() = 0 };
        // Non-zero tells the converter "no data right now" without ending the stream.
        return 1;
    }
    feed.given = true;
    list.mBuffers[0].mData = feed.samples as *mut c_void;
    list.mBuffers[0].mDataByteSize = feed.frames * 8;
    list.mBuffers[0].mNumberChannels = 2;
    unsafe { *io_packets.as_ptr() = feed.frames };
    0
}

fn pcm_format() -> AudioStreamBasicDescription {
    AudioStreamBasicDescription {
        mSampleRate: RATE as f64,
        mFormatID: kAudioFormatLinearPCM,
        mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
        mBytesPerPacket: 8,
        mFramesPerPacket: 1,
        mBytesPerFrame: 8,
        mChannelsPerFrame: 2,
        mBitsPerChannel: 32,
        mReserved: 0,
    }
}

/// A sample buffer holding one AAC packet at `frame` (48 kHz) on the capture
/// timeline, for AVAssetWriter passthrough.
pub fn packet_sample(packet: &AacPacket, format: &CMFormatDescription) -> Option<CFRetained<CMSampleBuffer>> {
    unsafe {
        let len = packet.data.len();
        let mut block: *mut CMBlockBuffer = ptr::null_mut();
        let status = CMBlockBuffer::create_with_memory_block(
            None,
            ptr::null_mut(),
            len,
            None,
            ptr::null(),
            0,
            len,
            0,
            NonNull::from(&mut block),
        );
        let block = CFRetained::from_raw(NonNull::new(block).filter(|_| status == 0)?);
        CMBlockBuffer::replace_data_bytes(NonNull::new_unchecked(packet.data.as_ptr() as *mut c_void), &block, 0, len);
        let desc = AudioStreamPacketDescription { mStartOffset: 0, mVariableFramesInPacket: 0, mDataByteSize: len as u32 };
        let mut sample: *mut CMSampleBuffer = ptr::null_mut();
        let status = CMAudioSampleBufferCreateReadyWithPacketDescriptions(
            None,
            &block,
            format,
            1,
            CMTime::new(packet.frame, RATE as i32),
            &desc,
            NonNull::from(&mut sample),
        );
        NonNull::new(sample).filter(|_| status == 0).map(|s| CFRetained::from_raw(s))
    }
}
