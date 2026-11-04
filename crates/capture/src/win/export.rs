//! Saving an edited clip's picture with the graphics card, for `media`'s
//! export where the picture is made again (a new size, rate, bitrate or
//! crop, or frame blending), as on Linux (`linux::export`). The frames are
//! decoded on the card by the decoder playback uses (`decode`) and stay
//! there: cropped and scaled as they are, in YUV, by Lanczos, as ffmpeg's
//! export scales (`Nv12Scaler`); or, to blend them or to make BT.601 the
//! BT.709 the file says it is, averaged as light (as live blending does),
//! back in YUV and scaled the same way, which is ffmpeg's order too. Then
//! they're encoded by the recording's encoder (the card's, through Media
//! Foundation) into our MP4 writer; or, for a file that has to fit a size
//! or where the card has no encoder, read back and encoded by x264 through
//! ffmpeg (the best in few bits). The sound is added after, by `media`, as
//! before.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST, D3D11_SRV_DIMENSION_BUFFER, D3D11_SRV_DIMENSION_TEXTURE2D, ID3DBlob};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::{Interface, PCSTR, s};

use super::d3d::Gpu;
use super::decode::{Decoder, Surface};
use super::h264::{self, Setup};
use crate::export::{Job, Timeline, lanczos};
use crate::mp4file::Layout;
use crate::mp4mux::Spec;
use crate::shutter::Shutter;
use crate::writer::Writer;

/// Make `job`'s picture (no sound) at `dest`, an MP4. `Ok(false)` where it
/// can't be made here (a file the card's decoder can't open, an odd size,
/// a card that can't draw into video frames): ffmpeg makes it then.
pub(crate) fn render_video(job: &Job, dest: &Path, progress: &dyn Fn(f32)) -> Result<bool> {
    let (w, h) = job.size;
    if job.out_fps == 0 || w < 2 || h < 2 || w % 2 != 0 || h % 2 != 0 || job.parts.is_empty() || job.fps <= 0.0 {
        return Ok(false);
    }
    let mut decoder = match Decoder::open_nv12(job.source) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("export: the graphics card's decoder can't open it ({e:#})");
            return Ok(false);
        }
    };
    // In whole U, V pairs, as H.264 keeps colour.
    let source = decoder.source_size();
    let crop = job.crop.unwrap_or([0, 0, source.0, source.1]);
    let inside = crop[0] + crop[2] <= source.0 && crop[1] + crop[3] <= source.1;
    if source.0 % 2 != 0 || source.1 % 2 != 0 || crop.iter().any(|v| v % 2 != 0) || crop[2] < 2 || crop[3] < 2 || !inside {
        return Ok(false);
    }
    // Only an H.264 MP4 read by its own index, whose frames' times are
    // exact: Matroska's and WebM's are whole milliseconds, which numbers a
    // frame now and then one early (33 ms is 0.99 of a frame at 30 fps),
    // where ffmpeg's export takes the nearest.
    if decoder.way() != "direct" {
        return Ok(false);
    }
    let gpu = decoder.gpu().clone();
    if !renders_nv12(&gpu) {
        eprintln!("export: this graphics card can't draw into video frames");
        return Ok(false);
    }
    eprintln!("export: decoding on the graphics card ({})", decoder.way());
    let started = Instant::now();
    decoder.set_fps(job.fps);
    let timeline = Timeline::new(job);
    if timeline.frames == 0 {
        bail!("nothing to save");
    }

    let shaders = Shaders::new(&gpu)?;
    if !scales_right(&gpu, &shaders) {
        return Ok(false);
    }
    let shutter = job.blend.map(|(rate, shape)| Shutter::new(rate / job.out_fps, shape));
    let bt601 = decoder.bt601();
    let mut maker = if shutter.is_none() && !bt601 {
        Maker::Scaled(Nv12Scaler::new(&gpu, &shaders, crop, job.size)?)
    } else {
        Maker::Drawn(Box::new(Blend::new(&gpu, &shaders, crop, bt601, shutter.map_or(1, |s| s.frames_open()), job.size)?))
    };
    let mut views = Views::default();

    let partial = dest.with_extension("partial.mp4");
    let mut encoder = Encoder::start(job, &gpu, &partial)?;

    // The source frames wanted, in order, each once: decoded ahead on a
    // thread of their own while the card works on the ones before.
    let wanted = timeline.source_frames(job, shutter.as_ref());
    let (tx, rx) = mpsc::sync_channel::<Result<Surface>>(6);
    let decoding = thread::Builder::new().name("export decode".into()).spawn(move || {
        super::system::com_init();
        for i in wanted {
            // Playback short of the card's video decoder (a clip being
            // watched meanwhile): it comes first.
            while crate::playback_short() {
                thread::sleep(Duration::from_millis(20));
            }
            let got = decoder.frame(i).and_then(|p| p.context("a frame the decoder doesn't have")?.gpu.context("a frame not on the graphics card"));
            let failed = got.is_err();
            if tx.send(got).is_err() || failed {
                break;
            }
        }
    })?;
    let made = (|| -> Result<()> {
        let mut next = || -> Result<(Surface, [ID3D11ShaderResourceView; 2])> {
            let frame = rx.recv().map_err(|_| anyhow!("the decoder stopped"))??;
            let planes = views.planes(&gpu, frame.texture())?;
            Ok((frame, planes))
        };
        let mut shown: Option<u64> = None;
        let mut version = 0u64;
        match (&shutter, &mut maker) {
            (None, _) => {
                for k in 0..timeline.frames {
                    let i = timeline.shown(job, k);
                    if shown != Some(i) {
                        let (frame, planes) = next()?;
                        match &mut maker {
                            Maker::Scaled(scaler) => scaler.put(&planes),
                            Maker::Drawn(blend) => {
                                blend.hold(frame, planes);
                                blend.add(0, 1.0);
                                blend.finish(0);
                            }
                        }
                        shown = Some(i);
                        version += 1;
                    }
                    encoder.encode(maker.scaler(), version, k as i64)?;
                    progress(k as f32 / timeline.frames as f32);
                }
            }
            (Some(s), Maker::Drawn(blend)) => {
                let last = timeline.frames as i64 - 1;
                let mut closed = -1i64;
                let mut m = s.first_look(0);
                while closed < last {
                    let (first, weights) = s.split(m);
                    if first > last {
                        break;
                    }
                    let i = timeline.seen(job, m);
                    if shown != Some(i) {
                        let (frame, planes) = next()?;
                        blend.hold(frame, planes);
                        shown = Some(i);
                    }
                    for (k, w) in [(first, weights[0]), (first + 1, weights[1])] {
                        if w > 0.0 && (0..=last).contains(&k) {
                            blend.add(k, w);
                        }
                    }
                    // The frames no later look counts toward are done.
                    let (after, _) = s.split(m + 1);
                    while closed < (after - 1).min(last) {
                        closed += 1;
                        blend.finish(closed);
                        version += 1;
                        encoder.encode(&blend.scaler, version, closed)?;
                        progress(closed as f32 / timeline.frames as f32);
                    }
                    m += 1;
                }
                while closed < last {
                    closed += 1;
                    blend.finish(closed);
                    version += 1;
                    encoder.encode(&blend.scaler, version, closed)?;
                }
            }
            (Some(_), Maker::Scaled(_)) => unreachable!("blending is drawn"),
        }
        Ok(())
    })();
    drop(rx);
    let _ = decoding.join();
    let finished = encoder.finish();
    let result = made.and(finished).and_then(|()| Ok(std::fs::rename(&partial, dest)?));
    match &result {
        Ok(()) => eprintln!("export: {} frames made in {:.1} s", timeline.frames, started.elapsed().as_secs_f64()),
        Err(_) => {
            let _ = std::fs::remove_file(&partial);
        }
    }
    result.map(|()| true)
}

/// The card can draw into NV12 textures (each plane on its own) and read
/// them, as the export does.
fn renders_nv12(gpu: &Gpu) -> bool {
    let need = (D3D11_FORMAT_SUPPORT_TEXTURE2D.0 | D3D11_FORMAT_SUPPORT_RENDER_TARGET.0 | D3D11_FORMAT_SUPPORT_SHADER_LOAD.0) as u32;
    unsafe { gpu.device.CheckFormatSupport(DXGI_FORMAT_NV12) }.is_ok_and(|s| s & need == need)
}

/// Whether the card crops and scales video frames as it should: a small
/// one, made there and here (the same weights), comes out the same, to a
/// level. A driver can draw into a video frame's planes wrongly without
/// saying so. Tried once.
fn scales_right(gpu: &Gpu, shaders: &Shaders) -> bool {
    static RIGHT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *RIGHT.get_or_init(|| match check_scaling(gpu, shaders) {
        Ok(None) => true,
        Ok(Some(off)) => {
            eprintln!("export: the graphics card scaled a test frame wrongly (a level off by {off}): ffmpeg makes exports");
            false
        }
        Err(e) => {
            eprintln!("export: couldn't try the graphics card's scaling ({e:#}): ffmpeg makes exports");
            false
        }
    })
}

/// A 64×32 NV12 frame of noise, its middle cropped and made 40×18 on the
/// card and here: `None` if they're the same to a level, else how far off.
fn check_scaling(gpu: &Gpu, shaders: &Shaders) -> Result<Option<u8>> {
    let (size, crop, frame) = ((64u32, 32u32), [2u32, 4, 58, 26], (40u32, 18u32));
    let mut seed = 0x2545_f491u32;
    let source: Vec<u8> = (0..size.0 * size.1 * 3 / 2)
        .map(|_| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 24) as u8
        })
        .collect();
    let texture = gpu.texture(size.0, size.1, DXGI_FORMAT_NV12, D3D11_BIND_SHADER_RESOURCE.0 as u32, D3D11_USAGE_DEFAULT, 0)?;
    let staging = gpu.texture(size.0, size.1, DXGI_FORMAT_NV12, 0, D3D11_USAGE_STAGING, D3D11_CPU_ACCESS_WRITE.0 as u32)?;
    unsafe {
        let ctx = &gpu.context;
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        ctx.Map(&staging, 0, D3D11_MAP_WRITE, 0, Some(&mut mapped))?;
        let w = size.0 as usize;
        for (row, line) in source.chunks_exact(w).enumerate() {
            std::ptr::copy_nonoverlapping(line.as_ptr(), (mapped.pData as *mut u8).add(row * mapped.RowPitch as usize), w);
        }
        ctx.Unmap(&staging, 0);
        ctx.CopyResource(&texture, &staging);
    }
    let scaler = Nv12Scaler::new(gpu, shaders, crop, frame)?;
    scaler.put(&planes_read(gpu, &texture)?);
    let mut card = Vec::new();
    Readback::new(gpu, frame)?.read(&scaler, &mut card)?;

    // Here: each plane across into rows of the new width, then down.
    let scale = |get: &dyn Fn(i32, i32) -> f32, [x, y, w, h]: [u32; 4], (ow, oh): (u32, u32)| -> Vec<u8> {
        let ((fx, wx, tx), (fy, wy, ty)) = (lanczos(x, w, ow), lanczos(y, h, oh));
        let rows: Vec<f32> = (0..h as i32)
            .flat_map(|r| (0..ow as usize).map(move |i| (i, r)))
            .map(|(i, r)| (0..tx as usize).map(|t| get((fx[i] + t as i32).clamp(x as i32, (x + w - 1) as i32), y as i32 + r) * wx[i * tx as usize + t]).sum())
            .collect();
        (0..oh as usize)
            .flat_map(|j| (0..ow as usize).map(move |i| (i, j)))
            .map(|(i, j)| {
                let sum: f32 = (0..ty as usize).map(|t| rows[((fy[j] + t as i32).clamp(y as i32, (y + h - 1) as i32) - y as i32) as usize * ow as usize + i] * wy[j * ty as usize + t]).sum();
                sum.round().clamp(0.0, 255.0) as u8
            })
            .collect()
    };
    let w = size.0 as i32;
    let chroma_at = (size.0 * size.1) as i32;
    let half = |[x, y, cw, ch]: [u32; 4]| [x / 2, y / 2, cw / 2, ch / 2];
    let luma = scale(&|x, y| source[(y * w + x) as usize] as f32, crop, frame);
    let u = scale(&|x, y| source[(chroma_at + y * w + 2 * x) as usize] as f32, half(crop), (frame.0 / 2, frame.1 / 2));
    let v = scale(&|x, y| source[(chroma_at + y * w + 2 * x + 1) as usize] as f32, half(crop), (frame.0 / 2, frame.1 / 2));
    let here: Vec<u8> = luma.into_iter().chain(u.into_iter().zip(v).flat_map(|(u, v)| [u, v])).collect();
    anyhow::ensure!(card.len() == here.len(), "{} bytes read back, not {}", card.len(), here.len());
    let off = card.iter().zip(&here).map(|(a, b)| a.abs_diff(*b)).max().unwrap_or(0);
    Ok((off > 1).then_some(off))
}

/// How the frames are made on the card.
enum Maker {
    /// Cropped and scaled as decoded.
    Scaled(Nv12Scaler),
    /// Averaged as light, then scaled.
    Drawn(Box<Blend>),
}

impl Maker {
    /// What makes the frame the encoder takes.
    fn scaler(&self) -> &Nv12Scaler {
        match self {
            Self::Scaled(s) => s,
            Self::Drawn(b) => &b.scaler,
        }
    }
}

/// The export's shaders, over NV12 textures read and drawn a plane at a
/// time: the luma (R8), and the U, V pairs (R8G8, half the size each way).
const SHADER: &str = r"
cbuffer Scale : register(b0) {
    // The part of the source used: x, y, width, height (even).
    int4 crop;
    // Weights a sample, and where each way's start in `firsts` and
    // `weights`: luma across, chroma across, luma down, chroma down.
    uint4 taps;
    uint4 first_at;
    uint4 weight_at;
};
cbuffer Look : register(b1) {
    // The crop's top left, and whether the source is BT.601 (else BT.709).
    int2 at;
    uint bt601;
    uint pad;
};
Texture2D<float> luma : register(t0);
Texture2D<float2> chroma : register(t1);
Buffer<int> firsts : register(t2);
Buffer<float> weights : register(t3);
// The crop's rows made the frame's width (values 0-255, unrounded).
Texture2D<float> rows_y : register(t4);
Texture2D<float2> rows_c : register(t5);
// A blended frame, as light.
Texture2D<float4> light : register(t6);

float4 vs(uint id : SV_VertexID) : SV_Position {
    float2 corner = float2((id << 1) & 2, id & 2);
    return float4(corner * float2(2, -2) + float2(-1, 1), 0, 1);
}

// Output sample `i` one way, tap `t`: its source sample, from `lo` to `hi`
// (the edges repeated past them).
int tap(uint way, uint i, uint t, int lo, int hi) {
    return clamp(firsts[first_at[way] + i] + (int)t, lo, hi);
}
float weight(uint way, uint i, uint t) {
    return weights[weight_at[way] + i * taps[way] + t];
}

// Across: each of the crop's rows made the frame's width.
float across_y(float4 pos : SV_Position) : SV_Target {
    uint2 p = uint2(pos.xy);
    float sum = 0;
    [loop] for (uint t = 0; t < taps.x; t++) {
        sum += luma.Load(int3(tap(0, p.x, t, crop.x, crop.x + crop.z - 1), crop.y + (int)p.y, 0)) * weight(0, p.x, t);
    }
    return sum * 255;
}
float2 across_c(float4 pos : SV_Position) : SV_Target {
    uint2 p = uint2(pos.xy);
    int lo = crop.x / 2;
    float2 sum = 0;
    [loop] for (uint t = 0; t < taps.y; t++) {
        sum += chroma.Load(int3(tap(1, p.x, t, lo, lo + crop.z / 2 - 1), crop.y / 2 + (int)p.y, 0)) * weight(1, p.x, t);
    }
    return sum * 255;
}

// Down: those rows made the frame's height, into its planes.
float down_y(float4 pos : SV_Position) : SV_Target {
    uint2 p = uint2(pos.xy);
    float sum = 0;
    [loop] for (uint t = 0; t < taps.z; t++) {
        sum += rows_y.Load(int3(p.x, tap(2, p.y, t, crop.y, crop.y + crop.w - 1) - crop.y, 0)) * weight(2, p.y, t);
    }
    return clamp(round(sum), 0, 255) / 255;
}
float2 down_c(float4 pos : SV_Position) : SV_Target {
    uint2 p = uint2(pos.xy);
    int lo = crop.y / 2;
    float2 sum = 0;
    [loop] for (uint t = 0; t < taps.w; t++) {
        sum += rows_c.Load(int3(p.x, tap(3, p.y, t, lo, lo + crop.w / 2 - 1) - lo, 0)) * weight(3, p.y, t);
    }
    return clamp(round(sum), 0, 255) / 255;
}

// sRGB colour to light (what a screen gives off, which adds up linearly),
// and back: as live blending (`quad`).
float3 to_light(float3 c) { return c <= 0.04045 ? c / 12.92 : pow((c + 0.055) / 1.055, 2.4); }
float3 from_light(float3 c) { c = saturate(c); return c <= 0.0031308 ? c * 12.92 : 1.055 * pow(c, 1.0 / 2.4) - 0.055; }

// A look: the source's picture in the crop, as light. Studio-range Y'CbCr
// to R'G'B', by the source's matrix.
float4 look(float4 pos : SV_Position) : SV_Target {
    int2 p = int2(pos.xy) + at;
    float y = (luma.Load(int3(p, 0)) * 255 - 16) / 219;
    float2 c = (chroma.Load(int3(p / 2, 0)) * 255 - 128) / 224;
    float3 rgb = bt601
        ? float3(y + 1.402 * c.y, y - 0.344136 * c.x - 0.714136 * c.y, y + 1.772 * c.x)
        : float3(y + 1.5748 * c.y, y - 0.187324 * c.x - 0.468124 * c.y, y + 1.8556 * c.x);
    return float4(to_light(saturate(rgb)), 1);
}

// A blended frame back in BT.709's studio-range Y'CbCr: the luma, and the
// U, V pairs of each 2x2 pixels' colour.
static const float3 KR_KG_KB = float3(0.2126, 0.7152, 0.0722);
float to_y(float4 pos : SV_Position) : SV_Target {
    float3 c = from_light(light.Load(int3(int2(pos.xy), 0)).rgb);
    return round(16 + 219 * dot(c, KR_KG_KB)) / 255;
}
float2 to_c(float4 pos : SV_Position) : SV_Target {
    int2 p = int2(pos.xy) * 2;
    float3 c = (from_light(light.Load(int3(p, 0)).rgb) + from_light(light.Load(int3(p + int2(1, 0), 0)).rgb)
        + from_light(light.Load(int3(p + int2(0, 1), 0)).rgb) + from_light(light.Load(int3(p + int2(1, 1), 0)).rgb)) / 4;
    float y = dot(c, KR_KG_KB);
    return round(128 + 224 * float2((c.b - y) / 1.8556, (c.r - y) / 1.5748)) / 255;
}
";

/// The compiled shaders, and the blend that mixes a look into an average.
#[derive(Clone)]
struct Shaders {
    vs: ID3D11VertexShader,
    across_y: ID3D11PixelShader,
    across_c: ID3D11PixelShader,
    down_y: ID3D11PixelShader,
    down_c: ID3D11PixelShader,
    look: ID3D11PixelShader,
    to_y: ID3D11PixelShader,
    to_c: ID3D11PixelShader,
    /// Mixes by a constant (the blend factor).
    mix: ID3D11BlendState,
}

fn compile(entry: PCSTR, target: PCSTR) -> Result<ID3DBlob> {
    let mut code = None;
    let mut errors = None;
    let result = unsafe { D3DCompile(SHADER.as_ptr().cast(), SHADER.len(), s!("export"), None, None, entry, target, 0, 0, &mut code, Some(&mut errors)) };
    if let Err(e) = result {
        let msg = errors.map(|b: ID3DBlob| String::from_utf8_lossy(bytes(&b)).into_owned()).unwrap_or_default();
        bail!("shader: {e} {msg}");
    }
    code.context("no shader code")
}

fn bytes(blob: &ID3DBlob) -> &[u8] {
    unsafe { std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize()) }
}

impl Shaders {
    fn new(gpu: &Gpu) -> Result<Self> {
        let dev = &gpu.device;
        let pixel = |entry: PCSTR| -> Result<ID3D11PixelShader> {
            let code = compile(entry, s!("ps_4_0"))?;
            let mut ps = None;
            unsafe { dev.CreatePixelShader(bytes(&code), None, Some(&mut ps)) }?;
            ps.context("no pixel shader")
        };
        let vs_code = compile(s!("vs"), s!("vs_4_0"))?;
        let mut vs = None;
        unsafe { dev.CreateVertexShader(bytes(&vs_code), None, Some(&mut vs)) }?;
        let mut desc = D3D11_BLEND_DESC::default();
        desc.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC {
            BlendEnable: true.into(),
            SrcBlend: D3D11_BLEND_BLEND_FACTOR,
            DestBlend: D3D11_BLEND_INV_BLEND_FACTOR,
            BlendOp: D3D11_BLEND_OP_ADD,
            SrcBlendAlpha: D3D11_BLEND_ONE,
            DestBlendAlpha: D3D11_BLEND_ZERO,
            BlendOpAlpha: D3D11_BLEND_OP_ADD,
            RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
        };
        let mut mix = None;
        unsafe { dev.CreateBlendState(&desc, Some(&mut mix)) }?;
        Ok(Self {
            vs: vs.context("no vertex shader")?,
            across_y: pixel(s!("across_y"))?,
            across_c: pixel(s!("across_c"))?,
            down_y: pixel(s!("down_y"))?,
            down_c: pixel(s!("down_c"))?,
            look: pixel(s!("look"))?,
            to_y: pixel(s!("to_y"))?,
            to_c: pixel(s!("to_c"))?,
            mix: mix.context("no blend state")?,
        })
    }

    /// One draw over all of `target` (`size` pixels) by `ps`, reading
    /// `reads` (from t0) and `constants` (from b0). `mix`: drawn as that
    /// much of the picture, the rest what's there.
    #[allow(clippy::too_many_arguments)]
    fn pass(&self, gpu: &Gpu, target: &ID3D11RenderTargetView, size: (u32, u32), ps: &ID3D11PixelShader, reads: &[Option<ID3D11ShaderResourceView>], constants: &[Option<ID3D11Buffer>], mix: Option<f32>) {
        let _lock = gpu.exclusive();
        let ctx = &gpu.context;
        unsafe {
            ctx.OMSetRenderTargets(Some(&[Some(target.clone())]), None);
            ctx.RSSetViewports(Some(&[D3D11_VIEWPORT { TopLeftX: 0.0, TopLeftY: 0.0, Width: size.0 as f32, Height: size.1 as f32, MinDepth: 0.0, MaxDepth: 1.0 }]));
            ctx.RSSetState(None);
            match mix {
                Some(w) if w < 1.0 => ctx.OMSetBlendState(Some(&self.mix), Some(&[w; 4]), 0xffff_ffff),
                _ => ctx.OMSetBlendState(None, None, 0xffff_ffff),
            }
            ctx.OMSetDepthStencilState(None, 0);
            ctx.IASetInputLayout(None);
            ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            ctx.VSSetShader(&self.vs, None);
            ctx.PSSetShader(ps, None);
            ctx.PSSetConstantBuffers(0, Some(constants));
            ctx.PSSetShaderResources(0, Some(reads));
            ctx.Draw(3, 0);
            // Leave nothing bound that the next pass draws into.
            ctx.OMSetRenderTargets(None, None);
            ctx.PSSetShaderResources(0, Some(&vec![None; reads.len()]));
        }
    }
}

/// Views of an NV12 texture's planes, to read: the luma (R8), and the U, V
/// pairs (R8G8).
fn planes_read(gpu: &Gpu, texture: &ID3D11Texture2D) -> Result<[ID3D11ShaderResourceView; 2]> {
    let view = |format: DXGI_FORMAT| -> Result<ID3D11ShaderResourceView> {
        let desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
            Format: format,
            ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_SRV { MostDetailedMip: 0, MipLevels: 1 } },
        };
        let mut v = None;
        unsafe { gpu.device.CreateShaderResourceView(texture, Some(&desc), Some(&mut v)) }.context("can't read a video frame's plane")?;
        v.context("no view")
    };
    Ok([view(DXGI_FORMAT_R8_UNORM)?, view(DXGI_FORMAT_R8G8_UNORM)?])
}

/// The same, to draw into.
fn planes_drawn(gpu: &Gpu, texture: &ID3D11Texture2D) -> Result<[ID3D11RenderTargetView; 2]> {
    let view = |format: DXGI_FORMAT| -> Result<ID3D11RenderTargetView> {
        let desc = D3D11_RENDER_TARGET_VIEW_DESC {
            Format: format,
            ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_RTV { MipSlice: 0 } },
        };
        let mut v = None;
        unsafe { gpu.device.CreateRenderTargetView(texture, Some(&desc), Some(&mut v)) }.context("can't draw into a video frame's plane")?;
        v.context("no view")
    };
    Ok([view(DXGI_FORMAT_R8_UNORM)?, view(DXGI_FORMAT_R8G8_UNORM)?])
}

/// Views of the decoder's textures (a fixed set), made once each.
#[derive(Default)]
struct Views(HashMap<usize, [ID3D11ShaderResourceView; 2]>);

impl Views {
    fn planes(&mut self, gpu: &Gpu, texture: &ID3D11Texture2D) -> Result<[ID3D11ShaderResourceView; 2]> {
        let key = texture.as_raw() as usize;
        if let Some(v) = self.0.get(&key) {
            return Ok(v.clone());
        }
        let v = planes_read(gpu, texture)?;
        self.0.insert(key, v.clone());
        Ok(v)
    }
}

/// A texture to draw into and read from: `format`, `size`.
fn surface(gpu: &Gpu, size: (u32, u32), format: DXGI_FORMAT) -> Result<(ID3D11RenderTargetView, ID3D11ShaderResourceView)> {
    let texture = gpu.texture(size.0, size.1, format, (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32, D3D11_USAGE_DEFAULT, 0)?;
    Ok((super::quad::target_view(gpu, &texture)?, super::quad::shader_view(gpu, &texture)?))
}

/// An unchanging buffer for the shaders: constants, or (`format`) values
/// they read one by one.
fn buffer(gpu: &Gpu, data: &[u8], format: Option<DXGI_FORMAT>) -> Result<(ID3D11Buffer, Option<ID3D11ShaderResourceView>)> {
    let bind = if format.is_some() { D3D11_BIND_SHADER_RESOURCE } else { D3D11_BIND_CONSTANT_BUFFER };
    let desc = D3D11_BUFFER_DESC { ByteWidth: data.len().next_multiple_of(16) as u32, Usage: D3D11_USAGE_IMMUTABLE, BindFlags: bind.0 as u32, ..Default::default() };
    let mut padded = data.to_vec();
    padded.resize(desc.ByteWidth as usize, 0);
    let init = D3D11_SUBRESOURCE_DATA { pSysMem: padded.as_ptr().cast(), SysMemPitch: 0, SysMemSlicePitch: 0 };
    let mut b = None;
    unsafe { gpu.device.CreateBuffer(&desc, Some(&init), Some(&mut b)) }?;
    let b = b.context("no buffer")?;
    let view = match format {
        None => None,
        Some(format) => {
            let desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
                Format: format,
                ViewDimension: D3D11_SRV_DIMENSION_BUFFER,
                Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                    Buffer: D3D11_BUFFER_SRV { Anonymous1: D3D11_BUFFER_SRV_0 { FirstElement: 0 }, Anonymous2: D3D11_BUFFER_SRV_1 { NumElements: (data.len() / 4) as u32 } },
                },
            };
            let mut v = None;
            unsafe { gpu.device.CreateShaderResourceView(&b, Some(&desc), Some(&mut v)) }?;
            Some(v.context("no view")?)
        }
    };
    Ok((b, view))
}

fn le_bytes<T: Copy, const N: usize>(values: &[T], to: impl Fn(T) -> [u8; N]) -> Vec<u8> {
    values.iter().flat_map(|&v| to(v)).collect()
}

/// NV12 frames cropped and scaled on the card as they are, each plane on
/// its own, by Lanczos (3 lobes, as ffmpeg's export scaled: Linux's
/// `Nv12Scaler`, the same weights): across into rows of the new width
/// (once a frame), then down into the encoder's picture. The same size is
/// an exact copy.
struct Nv12Scaler {
    gpu: Gpu,
    shaders: Shaders,
    /// The crop and each way's weights (see the shader's `Scale`).
    constants: ID3D11Buffer,
    firsts: ID3D11ShaderResourceView,
    weights: ID3D11ShaderResourceView,
    /// The crop's rows made the new width: the luma's, and the U, V
    /// pairs'.
    rows: [(ID3D11RenderTargetView, ID3D11ShaderResourceView); 2],
    frame: (u32, u32),
    crop_height: u32,
}

impl Nv12Scaler {
    /// For frames `crop` of which is used (left, top, width, height: even),
    /// made `frame` big.
    fn new(gpu: &Gpu, shaders: &Shaders, crop: [u32; 4], frame: (u32, u32)) -> Result<Self> {
        let [cx, cy, cw, ch] = crop;
        let (fw, fh) = frame;
        // Luma across, chroma across, luma down, chroma down.
        let ways = [lanczos(cx, cw, fw), lanczos(cx / 2, cw / 2, fw / 2), lanczos(cy, ch, fh), lanczos(cy / 2, ch / 2, fh / 2)];
        let (mut firsts, mut weights) = (Vec::new(), Vec::new());
        let (mut taps, mut first_at, mut weight_at) = ([0u32; 4], [0u32; 4], [0u32; 4]);
        for (k, (f, w, t)) in ways.iter().enumerate() {
            (taps[k], first_at[k], weight_at[k]) = (*t, firsts.len() as u32, weights.len() as u32);
            firsts.extend_from_slice(f);
            weights.extend_from_slice(w);
        }
        let mut layout = [0u32; 16];
        layout[..4].copy_from_slice(&crop);
        layout[4..8].copy_from_slice(&taps);
        layout[8..12].copy_from_slice(&first_at);
        layout[12..16].copy_from_slice(&weight_at);
        let (constants, _) = buffer(gpu, &le_bytes(&layout, u32::to_le_bytes), None)?;
        let (_, firsts) = buffer(gpu, &le_bytes(&firsts, i32::to_le_bytes), Some(DXGI_FORMAT_R32_SINT))?;
        let (_, weights) = buffer(gpu, &le_bytes(&weights, f32::to_le_bytes), Some(DXGI_FORMAT_R32_FLOAT))?;
        let rows = [surface(gpu, (fw, ch), DXGI_FORMAT_R32_FLOAT)?, surface(gpu, (fw / 2, ch / 2), DXGI_FORMAT_R32G32_FLOAT)?];
        Ok(Self {
            gpu: gpu.clone(),
            shaders: shaders.clone(),
            constants,
            firsts: firsts.context("no view")?,
            weights: weights.context("no view")?,
            rows,
            frame,
            crop_height: ch,
        })
    }

    /// The next frame (its planes) made the new width.
    fn put(&self, planes: &[ID3D11ShaderResourceView; 2]) {
        let (fw, ch) = (self.frame.0, self.crop_height);
        let (sh, constants) = (&self.shaders, [Some(self.constants.clone())]);
        let lookups = [Some(self.firsts.clone()), Some(self.weights.clone())];
        sh.pass(&self.gpu, &self.rows[0].0, (fw, ch), &sh.across_y, &[Some(planes[0].clone()), None, lookups[0].clone(), lookups[1].clone()], &constants, None);
        sh.pass(&self.gpu, &self.rows[1].0, (fw / 2, ch / 2), &sh.across_c, &[None, Some(planes[1].clone()), lookups[0].clone(), lookups[1].clone()], &constants, None);
    }

    /// That frame made the new height too, into `target`'s planes.
    fn to(&self, target: &[ID3D11RenderTargetView; 2]) {
        let (fw, fh) = self.frame;
        let (sh, constants) = (&self.shaders, [Some(self.constants.clone())]);
        let lookups = [Some(self.firsts.clone()), Some(self.weights.clone())];
        sh.pass(&self.gpu, &target[0], (fw, fh), &sh.down_y, &[None, None, lookups[0].clone(), lookups[1].clone(), Some(self.rows[0].1.clone()), None], &constants, None);
        sh.pass(&self.gpu, &target[1], (fw / 2, fh / 2), &sh.down_c, &[None, None, lookups[0].clone(), lookups[1].clone(), None, Some(self.rows[1].1.clone())], &constants, None);
        unsafe { self.gpu.context.Flush() };
    }
}

/// Frames blended as live blending does (`FrameBlend`): each the average of
/// the light of the looks in it, weighed as the shutter says, in 16-bit
/// float (faint ghosts and dark colours keep their precision); then back
/// in BT.709's YUV at the crop's size, and scaled from there. One look a
/// frame is the picture itself (a BT.601 one made BT.709).
struct Blend {
    gpu: Gpu,
    shaders: Shaders,
    /// Where the looks read the source (see the shader's `Look`).
    look: ID3D11Buffer,
    /// The averages being made: one per frame open at once, by its number.
    light: Vec<(ID3D11RenderTargetView, ID3D11ShaderResourceView)>,
    /// Per average: the looks' weight in it so far, and the weight of looks
    /// at the picture that's up not added yet (added in one draw when
    /// another comes up or the frame is done).
    weight: [f32; 2],
    pending: [f32; 2],
    /// The picture that's up, and its planes.
    held: Option<(Surface, [ID3D11ShaderResourceView; 2])>,
    /// A finished frame at the crop's size, in YUV: drawn into, and read.
    mid: [ID3D11RenderTargetView; 2],
    mid_planes: [ID3D11ShaderResourceView; 2],
    size: (u32, u32),
    /// From there to the frame's size.
    scaler: Nv12Scaler,
}

impl Blend {
    /// For frames `crop` of which is used (BT.601 ones, or BT.709), `open`
    /// of them made at once, made `frame` big.
    fn new(gpu: &Gpu, shaders: &Shaders, crop: [u32; 4], bt601: bool, open: usize, frame: (u32, u32)) -> Result<Self> {
        let size = (crop[2], crop[3]);
        let (look, _) = buffer(gpu, &le_bytes(&[crop[0], crop[1], u32::from(bt601), 0], u32::to_le_bytes), None)?;
        let light = (0..open.clamp(1, 2)).map(|_| surface(gpu, size, DXGI_FORMAT_R16G16B16A16_FLOAT)).collect::<Result<Vec<_>>>()?;
        let bind = (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32;
        let mid = gpu.texture(size.0, size.1, DXGI_FORMAT_NV12, bind, D3D11_USAGE_DEFAULT, 0)?;
        Ok(Self {
            gpu: gpu.clone(),
            shaders: shaders.clone(),
            look,
            light,
            weight: [0.0; 2],
            pending: [0.0; 2],
            held: None,
            mid: planes_drawn(gpu, &mid)?,
            mid_planes: planes_read(gpu, &mid)?,
            size,
            scaler: Nv12Scaler::new(gpu, shaders, [0, 0, size.0, size.1], frame)?,
        })
    }

    /// Which average frame `k` is made in.
    fn slot(&self, k: i64) -> usize {
        k.rem_euclid(self.light.len() as i64) as usize
    }

    /// `frame` is the picture that's up now.
    fn hold(&mut self, frame: Surface, planes: [ID3D11ShaderResourceView; 2]) {
        self.flush();
        self.held = Some((frame, planes));
    }

    /// Add a look at the picture that's up to frame `k`'s average, by `weight`.
    fn add(&mut self, k: i64, weight: f32) {
        let i = self.slot(k);
        self.pending[i] += weight;
    }

    /// Mix the looks at the picture that's up into the averages.
    fn flush(&mut self) {
        let Some((_, planes)) = &self.held else { return };
        let reads = [Some(planes[0].clone()), Some(planes[1].clone())];
        for i in 0..self.light.len() {
            let weight = std::mem::take(&mut self.pending[i]);
            if weight == 0.0 {
                continue;
            }
            self.weight[i] += weight;
            // Mixed in by its share of the total so far: the average.
            let sh = &self.shaders;
            sh.pass(&self.gpu, &self.light[i].0, self.size, &sh.look, &reads, &[None, Some(self.look.clone())], Some(weight / self.weight[i]));
        }
    }

    /// Frame `k` is done: back in YUV, made the frame's width, ready for
    /// the encoder; its average starts empty for the frame after next.
    fn finish(&mut self, k: i64) {
        self.flush();
        let i = self.slot(k);
        let sh = &self.shaders;
        let light = [None, None, None, None, None, None, Some(self.light[i].1.clone())];
        sh.pass(&self.gpu, &self.mid[0], self.size, &sh.to_y, &light, &[None], None);
        sh.pass(&self.gpu, &self.mid[1], (self.size.0 / 2, self.size.1 / 2), &sh.to_c, &light, &[None], None);
        self.weight[i] = 0.0;
        self.scaler.put(&self.mid_planes);
    }
}

/// The ffmpeg to run: one shipped next to the app wins over the one on
/// `PATH` (as the editor does it), without a console window.
fn ffmpeg() -> Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let bundled = std::env::current_exe().ok().and_then(|exe| Some(exe.parent()?.join("ffmpeg.exe"))).filter(|p| p.is_file());
    let mut cmd = Command::new(bundled.unwrap_or_else(|| PathBuf::from("ffmpeg")));
    cmd.args(["-hide_banner", "-nostdin", "-loglevel", "error"]).creation_flags(CREATE_NO_WINDOW);
    cmd
}

/// Frames read back from the card, for x264: made into a texture of our
/// own, copied to one the CPU can read.
struct Readback {
    gpu: Gpu,
    planes: [ID3D11RenderTargetView; 2],
    texture: ID3D11Texture2D,
    staging: ID3D11Texture2D,
    size: (u32, u32),
}

impl Readback {
    fn new(gpu: &Gpu, (w, h): (u32, u32)) -> Result<Self> {
        let texture = gpu.texture(w, h, DXGI_FORMAT_NV12, D3D11_BIND_RENDER_TARGET.0 as u32, D3D11_USAGE_DEFAULT, 0)?;
        let staging = gpu.texture(w, h, DXGI_FORMAT_NV12, 0, D3D11_USAGE_STAGING, D3D11_CPU_ACCESS_READ.0 as u32)?;
        Ok(Self { gpu: gpu.clone(), planes: planes_drawn(gpu, &texture)?, texture, staging, size: (w, h) })
    }

    /// The frame `scaler` has, as NV12 into `out`.
    fn read(&self, scaler: &Nv12Scaler, out: &mut Vec<u8>) -> Result<()> {
        scaler.to(&self.planes);
        let (w, h) = (self.size.0 as usize, self.size.1 as usize);
        out.clear();
        unsafe {
            let ctx = &self.gpu.context;
            ctx.CopyResource(&self.staging, &self.texture);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            ctx.Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            let src = mapped.pData as *const u8;
            // The luma's rows, then the U, V pairs' (half as many), which
            // follow them in the mapped texture.
            for row in 0..h + h / 2 {
                out.extend_from_slice(std::slice::from_raw_parts(src.add(row * mapped.RowPitch as usize), w));
            }
            ctx.Unmap(&self.staging, 0);
        }
        Ok(())
    }
}

/// Where the frames go: the card's encoder into our MP4 writer, or x264
/// (through ffmpeg) writing the file itself.
enum Encoder {
    Card { encoder: h264::Encoder, writer: Writer, targets: Vec<[ID3D11RenderTargetView; 2]> },
    X264 { child: Child, stdin: ChildStdin, readback: Readback, buf: Vec<u8>, holds: Option<u64> },
}

impl Encoder {
    fn start(job: &Job, gpu: &Gpu, partial: &Path) -> Result<Self> {
        let (w, h) = job.size;
        // `HASTECLIP_ENCODER=x264` (or anything but `mf`) leaves the card's
        // encoder out.
        let card = std::env::var("HASTECLIP_ENCODER").ok().is_none_or(|o| o == "mf");
        if !job.fit && card {
            let spec = Spec { width: w, height: h, fps: job.out_fps, audio_titles: Vec::new(), audio_bitrate: 0, comment: String::new(), mov: false, reorder: 0 };
            let writer = Writer::record(partial.to_path_buf(), Layout { spec }, job.out_fps);
            let setup = Setup { fps: job.out_fps, kbps: job.kbps.max(100), hardware: true, export: true, stream: None };
            let started = h264::Encoder::start_with(gpu, w, h, setup, writer.tx.clone()).and_then(|encoder| {
                let targets = encoder.pool.textures.iter().map(|t| planes_drawn(gpu, t)).collect::<Result<Vec<_>>>()?;
                Ok((encoder, targets))
            });
            match started {
                Ok((encoder, targets)) => {
                    eprintln!("export: encoding with {} ({w}x{h} at {} fps, {} kb/s)", encoder.name, job.out_fps, job.kbps);
                    return Ok(Self::Card { encoder, writer, targets });
                }
                Err(e) => eprintln!("export: the graphics card's encoder didn't start: {e:#}"),
            }
        }
        // x264: the best in few bits (a size to fit), or where the card
        // has no encoder. B-frames and all, ffmpeg writing the file.
        let rate = format!("{}k", job.kbps);
        let mut cmd = ffmpeg();
        cmd.args(["-y", "-f", "rawvideo", "-pix_fmt", "nv12", "-video_size", &format!("{w}x{h}"), "-framerate", &job.out_fps.to_string()])
            .args(["-color_range", "tv", "-colorspace", "bt709", "-color_primaries", "bt709", "-color_trc", "bt709", "-i", "pipe:0"])
            .args(["-c:v", "libx264", "-preset", "medium", "-b:v", &rate, "-profile:v", "high", "-pix_fmt", "yuv420p"]);
        if job.fit {
            cmd.args(["-maxrate", &rate, "-bufsize", &format!("{}k", job.kbps * 2)]);
        }
        cmd.args(["-color_range", "tv", "-colorspace", "bt709", "-color_primaries", "bt709", "-color_trc", "bt709", "-an", "-f", "mp4"]).arg(partial);
        let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().context("couldn't run ffmpeg for x264")?;
        let stdin = child.stdin.take().context("no pipe to ffmpeg")?;
        eprintln!("export: encoding with x264 ({w}x{h} at {} fps, {} kb/s)", job.out_fps, job.kbps);
        Ok(Self::X264 { child, stdin, readback: Readback::new(gpu, (w, h))?, buf: Vec::new(), holds: None })
    }

    /// Encode the frame `scaler` has (picture number `version`) as frame
    /// `n`, waiting while the encoder is busy: an export skips nothing.
    fn encode(&mut self, scaler: &Nv12Scaler, version: u64, n: i64) -> Result<()> {
        match self {
            Self::Card { encoder, targets, .. } => {
                // A texture of the encoder's, once it lets go of one.
                let since = Instant::now();
                let slot = loop {
                    if let Some(s) = encoder.pool.take() {
                        break s;
                    }
                    if encoder.failed() || since.elapsed() > Duration::from_secs(10) {
                        bail!("the graphics card's encoder stopped");
                    }
                    thread::sleep(Duration::from_micros(200));
                };
                scaler.to(&targets[slot]);
                if !encoder.encode_waiting(slot, n) {
                    bail!("the graphics card's encoder stopped");
                }
            }
            Self::X264 { stdin, readback, buf, holds, .. } => {
                if *holds != Some(version) {
                    readback.read(scaler, buf)?;
                    *holds = Some(version);
                }
                stdin.write_all(buf).context("x264 stopped")?;
            }
        }
        Ok(())
    }

    /// Encode what's in flight and finish the file.
    fn finish(self) -> Result<()> {
        match self {
            Self::Card { encoder, writer, .. } => {
                let finished = encoder.finish();
                writer.join()?;
                if !finished {
                    bail!("the graphics card's encoder stopped");
                }
            }
            Self::X264 { child, stdin, .. } => {
                drop(stdin);
                let out = child.wait_with_output()?;
                if !out.status.success() {
                    let why = String::from_utf8_lossy(&out.stderr);
                    bail!("x264: {}", why.lines().last().unwrap_or("failed"));
                }
            }
        }
        Ok(())
    }
}
