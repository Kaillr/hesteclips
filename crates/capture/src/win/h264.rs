//! H.264 encoding with Media Foundation, on its own thread.
//!
//! Hardware encoders (NVENC, AMF, Quick Sync behind their MFTs) take the NV12
//! textures straight from the GPU; they're asynchronous MFTs, driven by their
//! "need input" / "have output" events. A texture goes back to the pool when
//! the encoder releases its sample (tracked samples). The software encoder is a
//! plain synchronous MFT fed from a CPU copy of each frame.
//!
//! Output is Annex B; it's converted to AVCC for the MP4 writer here, with the
//! parameter sets attached to every keyframe so a clip can start on any of them.

use std::mem::ManuallyDrop;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use windows::Win32::Foundation::{E_NOTIMPL, VARIANT_TRUE};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_USAGE_STAGING, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Variant::{VARIANT, VT_BOOL, VT_UI4};
use windows::core::{Interface, implement};

use super::d3d::{Gpu, TexturePool};
use super::file::{EncodedFrame, VideoFrame};
use crate::EncodeSettings;
use crate::mp4mux::{AvcConfig, annexb_to_avcc};
use crate::writer::{Command, Media};

/// Frames waiting for the encoder before the pacer starts dropping them.
const QUEUE: usize = 4;

/// One frame to encode: pool slot `slot`, presented at `n / fps`.
pub(crate) struct Frame {
    pub slot: usize,
    pub n: i64,
}

/// The encoder thread.
pub(crate) struct Encoder {
    tx: Option<SyncSender<Frame>>,
    thread: Option<JoinHandle<()>>,
    pub pool: Arc<TexturePool>,
}

impl Encoder {
    pub(crate) fn start(gpu: &Gpu, width: u32, height: u32, s: &EncodeSettings, out: Sender<Command>) -> Result<Self> {
        let pool = TexturePool::new(gpu, width, height, 8)?;
        let (tx, rx) = mpsc::sync_channel::<Frame>(QUEUE);
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
        let (gpu2, pool2, s2) = (gpu.clone(), pool.clone(), s.clone());
        let thread = thread::spawn(move || {
            super::system::com_init();
            let mft = match open(&gpu2, width, height, &s2) {
                Ok(m) => m,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            let _ = ready_tx.send(Ok(()));
            let mut run = Run { gpu: gpu2, pool: pool2, fps: s2.fps, out, config: None, width, height, staging: None };
            if let Err(e) = run.run(mft, rx) {
                eprintln!("video encoder stopped: {e:#}");
            }
        });
        ready_rx.recv_timeout(Duration::from_secs(10)).map_err(|_| anyhow!("the video encoder didn't start"))??;
        Ok(Self { tx: Some(tx), thread: Some(thread), pool })
    }

    /// Queue slot `slot` as frame `n`. If the encoder is behind, the frame is
    /// dropped (its slot freed) rather than stalling capture.
    pub(crate) fn encode(&self, slot: usize, n: i64) {
        let Some(tx) = &self.tx else { return };
        match tx.try_send(Frame { slot, n }) {
            Ok(()) => {}
            Err(TrySendError::Full(f)) | Err(TrySendError::Disconnected(f)) => self.pool.give(f.slot),
        }
    }

    /// Encode what's queued, flush the encoder and wait for it.
    pub(crate) fn finish(mut self) {
        drop(self.tx.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// An opened and configured encoder MFT.
enum Mft {
    /// Hardware: event-driven, takes GPU textures.
    Async { mft: IMFTransform, events: IMFMediaEventGenerator, _manager: IMFDXGIDeviceManager },
    /// Software: called directly, takes frames in memory.
    Sync { mft: IMFTransform },
}

impl Mft {
    fn mft(&self) -> &IMFTransform {
        match self {
            Mft::Async { mft, .. } | Mft::Sync { mft } => mft,
        }
    }
}

/// The best encoder that accepts our settings: a hardware one on our GPU if
/// asked for (falling back to software if none works), else software.
fn open(gpu: &Gpu, width: u32, height: u32, s: &EncodeSettings) -> Result<Mft> {
    let mut errors = Vec::new();
    if s.use_hardware {
        for activate in enum_encoders(true, gpu.adapter_luid()) {
            match configure_hardware(&activate, gpu, width, height, s) {
                Ok(m) => return Ok(m),
                Err(e) => errors.push(format!("{}: {e:#}", friendly_name(&activate))),
            }
        }
        if !errors.is_empty() {
            eprintln!("hardware H.264 encoders failed, using software: {}", errors.join("; "));
        }
    }
    for activate in enum_encoders(false, None) {
        let result = unsafe { activate.ActivateObject::<IMFTransform>() }
            .map_err(anyhow::Error::from)
            .and_then(|mft| configure(&mft, width, height, s).map(|()| Mft::Sync { mft }));
        match result {
            Ok(m) => return Ok(m),
            Err(e) => errors.push(format!("{}: {e:#}", friendly_name(&activate))),
        }
    }
    bail!("no H.264 encoder works on this PC ({})", errors.join("; "))
}

/// H.264 encoder MFTs taking NV12, best first.
fn enum_encoders(hardware: bool, luid: Option<u64>) -> Vec<IMFActivate> {
    let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
    let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
    let flags = if hardware {
        MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER
    } else {
        MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_LOCALMFT | MFT_ENUM_FLAG_SORTANDFILTER
    };
    let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    unsafe {
        // On multi-GPU machines, only the encoders on the GPU we capture with.
        let by_luid = luid.and_then(|luid| {
            let mut attrs = None;
            MFCreateAttributes(&mut attrs, 1).ok()?;
            let attrs = attrs?;
            attrs.SetBlob(&MFT_ENUM_ADAPTER_LUID, &luid.to_le_bytes()).ok()?;
            MFTEnum2(MFT_CATEGORY_VIDEO_ENCODER, flags, Some(&input), Some(&output), &attrs, &mut list, &mut count).ok()
        });
        if by_luid.is_none() && MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, flags, Some(&input), Some(&output), &mut list, &mut count).is_err() {
            return Vec::new();
        }
        take_activates(list, count)
    }
}

/// Take ownership of an MFT enumeration result.
pub(crate) unsafe fn take_activates(list: *mut Option<IMFActivate>, count: u32) -> Vec<IMFActivate> {
    if list.is_null() {
        return Vec::new();
    }
    let out = (0..count as usize).filter_map(|i| unsafe { std::ptr::read(list.add(i)) }).collect();
    unsafe { CoTaskMemFree(Some(list as *const _)) };
    out
}

fn friendly_name(activate: &IMFActivate) -> String {
    let mut buf = [0u16; 256];
    let mut len = 0u32;
    unsafe { activate.GetString(&MFT_FRIENDLY_NAME_Attribute, &mut buf, Some(&mut len)) }
        .map(|()| String::from_utf16_lossy(&buf[..len as usize]))
        .unwrap_or_else(|_| "encoder".into())
}

fn configure_hardware(activate: &IMFActivate, gpu: &Gpu, width: u32, height: u32, s: &EncodeSettings) -> Result<Mft> {
    unsafe {
        let mft: IMFTransform = activate.ActivateObject()?;
        let attrs = mft.GetAttributes()?;
        if attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) == 0 {
            bail!("not an asynchronous encoder");
        }
        attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
        let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
        let mut token = 0u32;
        let mut manager = None;
        MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
        let manager = manager.context("no device manager")?;
        manager.ResetDevice(&gpu.device, token)?;
        mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)?;
        configure(&mft, width, height, s)?;
        let events: IMFMediaEventGenerator = mft.cast()?;
        Ok(Mft::Async { mft, events, _manager: manager })
    }
}

/// Rate control, GOP and latency, then the media types, then start streaming.
fn configure(mft: &IMFTransform, width: u32, height: u32, s: &EncodeSettings) -> Result<()> {
    let bitrate = s.video_bitrate_kbps.saturating_mul(1000);
    let gop = s.fps * s.keyframe_interval_secs.max(1);
    unsafe {
        if let Ok(api) = mft.cast::<ICodecAPI>() {
            // Each is best effort: encoders support different subsets.
            let set = |key: &windows::core::GUID, v: VARIANT| {
                let _ = api.SetValue(key, &v);
            };
            if api.SetValue(&CODECAPI_AVEncCommonRateControlMode, &variant_u32(eAVEncCommonRateControlMode_PeakConstrainedVBR.0 as u32)).is_err() {
                set(&CODECAPI_AVEncCommonRateControlMode, variant_u32(eAVEncCommonRateControlMode_UnconstrainedVBR.0 as u32));
            }
            set(&CODECAPI_AVEncCommonMeanBitRate, variant_u32(bitrate));
            set(&CODECAPI_AVEncCommonMaxBitRate, variant_u32(bitrate.saturating_mul(3) / 2));
            set(&CODECAPI_AVEncMPVGOPSize, variant_u32(gop));
            set(&CODECAPI_AVEncMPVDefaultBPictureCount, variant_u32(0));
            set(&CODECAPI_AVLowLatencyMode, variant_bool(true));
        }

        let out = MFCreateMediaType()?;
        out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        out.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
        out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
        video_type_common(&out, width, height, s.fps)?;
        mft.SetOutputType(0, &out, 0).context("encoder rejected the output format")?;

        let input = MFCreateMediaType()?;
        input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        video_type_common(&input, width, height, s.fps)?;
        mft.SetInputType(0, &input, 0).context("encoder rejected the input format")?;

        mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
        mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
    }
    Ok(())
}

fn video_type_common(t: &IMFMediaType, width: u32, height: u32, fps: u32) -> windows::core::Result<()> {
    unsafe {
        t.SetUINT64(&MF_MT_FRAME_SIZE, (width as u64) << 32 | height as u64)?;
        t.SetUINT64(&MF_MT_FRAME_RATE, (fps as u64) << 32 | 1)?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, 1 << 32 | 1)?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        t.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
        t.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
        t.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
        t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
    }
    Ok(())
}

fn variant_u32(v: u32) -> VARIANT {
    let mut var = VARIANT::default();
    unsafe {
        let inner = &mut *var.Anonymous.Anonymous;
        inner.vt = VT_UI4;
        inner.Anonymous.ulVal = v;
    }
    var
}

fn variant_bool(v: bool) -> VARIANT {
    let mut var = VARIANT::default();
    unsafe {
        let inner = &mut *var.Anonymous.Anonymous;
        inner.vt = VT_BOOL;
        inner.Anonymous.boolVal = if v { VARIANT_TRUE } else { windows::Win32::Foundation::VARIANT_FALSE };
    }
    var
}

// ---------------------------------------------------------------------------
// The encoding loop
// ---------------------------------------------------------------------------

struct Run {
    gpu: Gpu,
    pool: Arc<TexturePool>,
    fps: u32,
    out: Sender<Command>,
    /// The latest parameter sets, attached to every keyframe.
    config: Option<AvcConfig>,
    width: u32,
    height: u32,
    /// Software path: CPU-readable copy of a frame.
    staging: Option<ID3D11Texture2D>,
}

impl Run {
    fn run(&mut self, mft: Mft, rx: Receiver<Frame>) -> Result<()> {
        // Encoders that don't repeat SPS/PPS in the stream still describe them in
        // the output type.
        self.config = sequence_header(mft.mft());
        match &mft {
            Mft::Async { mft, events, .. } => self.run_async(mft, events, rx),
            Mft::Sync { mft } => self.run_sync(mft, rx),
        }
    }

    fn run_async(&mut self, mft: &IMFTransform, events: &IMFMediaEventGenerator, rx: Receiver<Frame>) -> Result<()> {
        let mut draining: Option<Instant> = None;
        // Inputs the encoder asked for that we haven't given yet.
        let mut wanted = 0usize;
        loop {
            match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => {
                    let kind = MF_EVENT_TYPE(unsafe { event.GetType()? } as i32);
                    if kind == METransformNeedInput {
                        wanted += 1;
                    } else if kind == METransformHaveOutput {
                        self.process_output(mft)?;
                    } else if kind == METransformDrainComplete {
                        return Ok(());
                    }
                }
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => {
                    if let Some(since) = draining {
                        if since.elapsed() > Duration::from_secs(3) {
                            bail!("the encoder didn't finish");
                        }
                    }
                    if wanted == 0 || draining.is_some() {
                        thread::sleep(Duration::from_millis(1));
                    }
                }
                Err(e) => return Err(e.into()),
            }
            if wanted > 0 && draining.is_none() {
                match rx.recv_timeout(Duration::from_millis(2)) {
                    Ok(frame) => {
                        wanted -= 1;
                        self.input_hardware(mft, frame)?;
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        unsafe {
                            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)?;
                            mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)?;
                        }
                        draining = Some(Instant::now());
                    }
                }
            }
        }
    }

    fn run_sync(&mut self, mft: &IMFTransform, rx: Receiver<Frame>) -> Result<()> {
        for frame in rx {
            let sample = self.readback(frame.slot, frame.n);
            self.pool.give(frame.slot);
            unsafe { mft.ProcessInput(0, &sample?, 0)? };
            while self.process_output(mft)? {}
        }
        unsafe {
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)?;
            mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)?;
        }
        while self.process_output(mft)? {}
        Ok(())
    }

    /// Hand the encoder a GPU texture. The slot returns to the pool when the
    /// encoder lets go of the sample.
    fn input_hardware(&mut self, mft: &IMFTransform, frame: Frame) -> Result<()> {
        let result = (|| -> Result<()> {
            unsafe {
                let tracked = MFCreateTrackedSample()?;
                let sample: IMFSample = tracked.cast()?;
                let tex = &self.pool.textures[frame.slot];
                let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, tex, 0, false)?;
                if let Ok(b2) = buffer.cast::<IMF2DBuffer>() {
                    buffer.SetCurrentLength(b2.GetContiguousLength()?)?;
                }
                sample.AddBuffer(&buffer)?;
                self.stamp(&sample, frame.n)?;
                let release: IMFAsyncCallback = Release { pool: self.pool.clone(), slot: frame.slot }.into();
                tracked.SetAllocator(&release, None)?;
                mft.ProcessInput(0, &sample, 0)?;
            }
            Ok(())
        })();
        if result.is_err() {
            self.pool.give(frame.slot);
        }
        result
    }

    /// Copy a frame to memory for the software encoder.
    fn readback(&mut self, slot: usize, n: i64) -> Result<IMFSample> {
        let (w, h) = (self.width as usize, self.height as usize);
        if self.staging.is_none() {
            self.staging = Some(self.gpu.texture(self.width, self.height, DXGI_FORMAT_NV12, 0, D3D11_USAGE_STAGING, D3D11_CPU_ACCESS_READ.0 as u32)?);
        }
        let staging = self.staging.as_ref().unwrap();
        let size = w * h * 3 / 2;
        unsafe {
            self.gpu.context.CopyResource(staging, &self.pool.textures[slot]);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.gpu.context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            let buffer = MFCreateMemoryBuffer(size as u32)?;
            let mut dst = std::ptr::null_mut();
            buffer.Lock(&mut dst, None, None)?;
            let pitch = mapped.RowPitch as usize;
            let src = mapped.pData as *const u8;
            // Y plane, then the interleaved UV plane (half height), which follows
            // the Y plane's rows in the mapped texture.
            for row in 0..h + h / 2 {
                std::ptr::copy_nonoverlapping(src.add(row * pitch), dst.add(row * w), w);
            }
            buffer.Unlock()?;
            self.gpu.context.Unmap(staging, 0);
            buffer.SetCurrentLength(size as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            self.stamp(&sample, n)?;
            Ok(sample)
        }
    }

    fn stamp(&self, sample: &IMFSample, n: i64) -> windows::core::Result<()> {
        let fps = self.fps.max(1) as i64;
        unsafe {
            sample.SetSampleTime(n * 10_000_000 / fps)?;
            sample.SetSampleDuration(10_000_000 / fps)
        }
    }

    /// Pull one encoded frame out and send it to the writer. Returns false when
    /// the encoder needs more input first.
    fn process_output(&mut self, mft: &IMFTransform) -> Result<bool> {
        unsafe {
            let info = mft.GetOutputStreamInfo(0)?;
            let provides = info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0) as u32 != 0;
            let own = if provides {
                None
            } else {
                let s = MFCreateSample()?;
                s.AddBuffer(&MFCreateMemoryBuffer(info.cbSize.max(self.width * self.height))?)?;
                Some(s)
            };
            let mut buf = [MFT_OUTPUT_DATA_BUFFER { dwStreamID: 0, pSample: ManuallyDrop::new(own), dwStatus: 0, pEvents: ManuallyDrop::new(None) }];
            let mut status = 0u32;
            let result = mft.ProcessOutput(0, &mut buf, &mut status);
            let sample = ManuallyDrop::take(&mut buf[0].pSample);
            drop(ManuallyDrop::take(&mut buf[0].pEvents));
            match result {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(false),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    let t = mft.GetOutputAvailableType(0, 0)?;
                    mft.SetOutputType(0, &t, 0)?;
                    if let Some(c) = sequence_header(mft) {
                        self.config = Some(c);
                    }
                    return Ok(true);
                }
                Err(e) => return Err(e.into()),
            }
            let Some(sample) = sample else { return Ok(true) };
            let n = ((sample.GetSampleTime().unwrap_or(0) as f64) * self.fps as f64 / 1e7).round() as i64;
            let clean = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
            let buffer = sample.ConvertToContiguousBuffer()?;
            let mut ptr = std::ptr::null_mut();
            let mut len = 0u32;
            buffer.Lock(&mut ptr, None, Some(&mut len))?;
            let frame = annexb_to_avcc(std::slice::from_raw_parts(ptr, len as usize));
            buffer.Unlock()?;
            if frame.data.is_empty() {
                return Ok(true);
            }
            if let Some(c) = frame.config {
                self.config = Some(c);
            }
            let key = frame.idr || clean;
            let config = if key { self.config.clone() } else { None };
            let media = Media::Video {
                frame: VideoFrame(Arc::new(EncodedFrame { data: Arc::from(frame.data), config })),
                pts: n as f64 / self.fps as f64,
                key,
            };
            if self.out.send(Command::Media(media)).is_err() {
                bail!("the file writer stopped");
            }
            Ok(true)
        }
    }
}

/// SPS/PPS from the output type's sequence header, if the encoder sets one.
fn sequence_header(mft: &IMFTransform) -> Option<AvcConfig> {
    unsafe {
        let t = mft.GetOutputCurrentType(0).ok()?;
        let size = t.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER).ok()?;
        let mut blob = vec![0u8; size as usize];
        t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut blob, None).ok()?;
        annexb_to_avcc(&blob).config
    }
}

/// Returns a texture to the pool when the encoder releases the sample using it.
#[implement(IMFAsyncCallback)]
struct Release {
    pool: Arc<TexturePool>,
    slot: usize,
}

impl IMFAsyncCallback_Impl for Release_Impl {
    fn GetParameters(&self, _flags: *mut u32, _queue: *mut u32) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn Invoke(&self, _result: windows::core::Ref<IMFAsyncResult>) -> windows::core::Result<()> {
        self.pool.give(self.slot);
        Ok(())
    }
}
