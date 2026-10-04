//! Prototype: frames handed straight to the H.264 decoder, read from the
//! file by our own index (`mp4read`), skipping Media Foundation's file reader
//! and its seek. Being measured against `decode::Decoder` (decode_bench
//! `DIRECT=1`) before anything uses it.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::mem::ManuallyDrop;
use std::path::Path;

use anyhow::{Context, Result, bail};
use windows::Win32::Media::MediaFoundation::*;
use windows::core::Interface;

use super::d3d::Gpu;
use super::mp4read::Index;
use super::system::{com_init, mf_startup};

pub struct Direct {
    gpu: Gpu,
    _manager: IMFDXGIDeviceManager,
    mft: IMFTransform,
    pub index: Index,
    file: File,
    /// The decoder works on the GPU (DXVA), not in software.
    pub on_gpu: bool,
}

unsafe impl Send for Direct {}

impl Direct {
    pub fn open(path: &Path, low_latency: bool) -> Result<Self> {
        com_init();
        mf_startup()?;
        let index = Index::read(path)?;
        let gpu = Gpu::new()?;
        unsafe {
            let mut token = 0u32;
            let mut manager = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
            let manager = manager.context("no device manager")?;
            manager.ResetDevice(&gpu.device, token)?;

            let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
            let mut list = std::ptr::null_mut();
            let mut count = 0u32;
            let flags = MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_LOCALMFT | MFT_ENUM_FLAG_SORTANDFILTER;
            MFTEnumEx(MFT_CATEGORY_VIDEO_DECODER, flags, Some(&input), None, &mut list, &mut count)?;
            let found = super::h264::take_activates(list, count);
            let activate = found.first().context("no H.264 decoder")?;
            let mft: IMFTransform = activate.ActivateObject()?;
            let attrs = mft.GetAttributes()?;
            let on_gpu = attrs.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) != 0;
            if on_gpu {
                mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)?;
            }
            if low_latency {
                attrs.SetUINT32(&MF_LOW_LATENCY, 1)?;
            }
            let t = MFCreateMediaType()?;
            t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            t.SetUINT64(&MF_MT_FRAME_SIZE, (index.width as u64) << 32 | index.height as u64)?;
            t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            mft.SetInputType(0, &t, 0).context("the decoder rejected the input")?;
            set_nv12(&mft)?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            Ok(Self { gpu, _manager: manager, mft, file: File::open(path)?, index, on_gpu })
        }
    }

    /// Decode the keyframe `sample` (an index into `index.samples`) on its own,
    /// waiting until the GPU has finished it.
    pub fn keyframe(&mut self, sample: usize) -> Result<IMFSample> {
        let s = self.index.samples[sample];
        let mut data = vec![0u8; s.size as usize];
        self.file.seek(SeekFrom::Start(s.offset))?;
        self.file.read_exact(&mut data)?;
        let annexb = self.annexb(&data, true);
        unsafe {
            self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)?;
            let input = MFCreateSample()?;
            let buffer = MFCreateMemoryBuffer(annexb.len() as u32)?;
            let mut ptr = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None)?;
            std::ptr::copy_nonoverlapping(annexb.as_ptr(), ptr, annexb.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(annexb.len() as u32)?;
            input.AddBuffer(&buffer)?;
            input.SetSampleTime(s.pts * 10_000_000 / self.index.timescale as i64)?;
            input.SetSampleDuration(10_000_000 / 60)?;
            input.SetUINT32(&MFSampleExtension_CleanPoint, 1)?;
            self.mft.ProcessInput(0, &input, 0)?;
            let mut drained = false;
            loop {
                if let Some(out) = self.output()? {
                    self.gpu.wait_idle()?;
                    return Ok(out);
                }
                if drained {
                    bail!("the decoder gave nothing back");
                }
                self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)?;
                drained = true;
            }
        }
    }

    /// Decode sample `target` (decode order): from the keyframe before it,
    /// every frame up to it, keeping the last. Waits until the GPU is done.
    pub fn frame(&mut self, target: usize) -> Result<IMFSample> {
        let key = (0..=target).rev().find(|&i| self.index.samples[i].key).context("no keyframe before it")?;
        unsafe { self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)? };
        // One read for the whole stretch: our files keep a GOP's frames together.
        let (first, last) = (self.index.samples[key], self.index.samples[target]);
        let contiguous = last.offset >= first.offset;
        let span = if contiguous { (last.offset + last.size as u64 - first.offset) as usize } else { 0 };
        let mut block = vec![0u8; span];
        if contiguous {
            self.file.seek(SeekFrom::Start(first.offset))?;
            self.file.read_exact(&mut block)?;
        }
        let mut out = None;
        for i in key..=target {
            let s = self.index.samples[i];
            let data = if contiguous && s.offset >= first.offset && ((s.offset - first.offset) as usize + s.size as usize) <= span {
                let at = (s.offset - first.offset) as usize;
                block[at..at + s.size as usize].to_vec()
            } else {
                let mut d = vec![0u8; s.size as usize];
                self.file.seek(SeekFrom::Start(s.offset))?;
                self.file.read_exact(&mut d)?;
                d
            };
            let annexb = self.annexb(&data, i == key);
            unsafe {
                let input = MFCreateSample()?;
                let buffer = MFCreateMemoryBuffer(annexb.len() as u32)?;
                let mut ptr = std::ptr::null_mut();
                buffer.Lock(&mut ptr, None, None)?;
                std::ptr::copy_nonoverlapping(annexb.as_ptr(), ptr, annexb.len());
                buffer.Unlock()?;
                buffer.SetCurrentLength(annexb.len() as u32)?;
                input.AddBuffer(&buffer)?;
                input.SetSampleTime(s.pts * 10_000_000 / self.index.timescale as i64)?;
                input.SetSampleDuration(10_000_000 / 60)?;
                loop {
                    match self.mft.ProcessInput(0, &input, 0) {
                        Ok(()) => break,
                        Err(e) if e.code() == MF_E_NOTACCEPTING => {
                            if let Some(o) = self.output()? {
                                out = Some(o);
                            }
                        }
                        Err(e) => return Err(e.into()),
                    }
                }
                while let Some(o) = self.output()? {
                    out = Some(o);
                }
            }
        }
        if out.is_none() {
            unsafe { self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)? };
            while let Some(o) = self.output()? {
                out = Some(o);
            }
        }
        self.gpu.wait_idle()?;
        out.context("the decoder gave nothing back")
    }

    /// The decoded picture's NV12 bytes (rows packed, cropped to the video).
    pub fn read_nv12(&self, sample: &IMFSample) -> Result<Vec<u8>> {
        use windows::Win32::Graphics::Direct3D11::*;
        unsafe {
            let buffer = sample.GetBufferByIndex(0)?;
            let dxgi: IMFDXGIBuffer = buffer.cast()?;
            let mut texture: Option<ID3D11Texture2D> = None;
            dxgi.GetResource(&ID3D11Texture2D::IID, &mut texture as *mut _ as *mut _)?;
            let texture = texture.context("no texture")?;
            let slice = dxgi.GetSubresourceIndex()?;
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            texture.GetDesc(&mut desc);
            let staging_desc = D3D11_TEXTURE2D_DESC {
                ArraySize: 1,
                MipLevels: 1,
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
                ..desc
            };
            let mut staging = None;
            self.gpu.device.CreateTexture2D(&staging_desc, None, Some(&mut staging))?;
            let staging = staging.context("no staging texture")?;
            self.gpu.context.CopySubresourceRegion(&staging, 0, 0, 0, 0, &texture, slice, None);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.gpu.context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            let (w, h) = (self.index.width as usize, self.index.height as usize);
            let pitch = mapped.RowPitch as usize;
            let base = mapped.pData as *const u8;
            let mut out = Vec::with_capacity(w * h * 3 / 2);
            for y in 0..h {
                out.extend_from_slice(std::slice::from_raw_parts(base.add(y * pitch), w));
            }
            // The chroma plane starts after the texture's full (maybe padded) height.
            let uv = base.add(desc.Height as usize * pitch);
            for y in 0..h / 2 {
                out.extend_from_slice(std::slice::from_raw_parts(uv.add(y * pitch), w));
            }
            self.gpu.context.Unmap(&staging, 0);
            Ok(out)
        }
    }

    /// The decoder's input: NAL units with start codes, the SPS/PPS first on a keyframe.
    fn annexb(&self, avcc: &[u8], key: bool) -> Vec<u8> {
        let mut out = Vec::with_capacity(avcc.len() + 64);
        if key {
            for p in self.index.sps.iter().chain(&self.index.pps) {
                out.extend_from_slice(&[0, 0, 0, 1]);
                out.extend_from_slice(p);
            }
        }
        let n = self.index.nal_length;
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

    fn output(&self) -> Result<Option<IMFSample>> {
        unsafe {
            loop {
                let info = self.mft.GetOutputStreamInfo(0)?;
                let provides = info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0) as u32 != 0;
                let own = if provides {
                    None
                } else {
                    let s = MFCreateSample()?;
                    s.AddBuffer(&MFCreateMemoryBuffer(info.cbSize.max(1))?)?;
                    Some(s)
                };
                let mut buf = [MFT_OUTPUT_DATA_BUFFER { dwStreamID: 0, pSample: ManuallyDrop::new(own), dwStatus: 0, pEvents: ManuallyDrop::new(None) }];
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
