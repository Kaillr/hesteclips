//! Saving an edited clip's picture with the graphics card, for `media`'s
//! export where the picture is made again (a new size, rate, bitrate or
//! crop, or frame blending). The frames are decoded in the app (`decode`:
//! on the card, there, into our device's memory; else on the CPU, and
//! put up on the card), there cropped and scaled as they are, in YUV
//! (`Nv12Scaler`); or, to blend them, turned into RGB and blended by the
//! recording's own frame code (`gpu_frame`: blended just as live). Then
//! they're encoded by the recording's encoders (NVIDIA's, or the card's
//! through Vulkan Video) into our MP4 writer; or, for a file that has to
//! fit a size, read back and encoded by x264 through ffmpeg (the best in
//! few bits). The sound is added after, by `media`, as before.
//!
//! ffmpeg's filters and x264 did all of it on the CPU: a minute of 1440p60
//! made 1080p took 49 s, and blending 22 s of 540 fps down to 60, 64 s.

use std::io::Write;
use std::path::Path;
use std::process::{Child, ChildStdin, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

use super::decode::{Card, Decoder, Nv12, Output};
use super::gpu::{self, Gpu};
use super::gpu_frame::{GpuPicture, Nv12Source, Nv12Target, nv12_len};
use super::nvenc::Nvenc;
use super::screen::{Frame, Order};
use super::vkenc::VkEncoder;
use crate::mp4file::Layout;
use crate::mp4mux::Spec;
use crate::shutter::Shutter;
use crate::writer::Writer;
use crate::export::{Job, Timeline, lanczos};
use crate::{Crop, Framing, ScreenPlacement};

/// Make `job`'s picture (no sound) at `dest`, an MP4. `Ok(false)` where it
/// can't be made here (no graphics card to work on, a file the decoder
/// can't open): ffmpeg makes it then.
pub(crate) fn render_video(job: &Job, dest: &Path, progress: &dyn Fn(f32)) -> Result<bool> {
    let Some(card) = gpu::gpu() else { return Ok(false) };
    if job.out_fps == 0 || job.size.0 < 2 || job.size.1 < 2 || job.parts.is_empty() || job.fps <= 0.0 {
        return Ok(false);
    }
    // On the card where it can (the CPU free: ~11 ms a 1440p frame
    // there, every core busy), into our device's memory, which the frames
    // are read from where they are.
    let on_card = Card::background().filter(|_| std::env::var_os("HESTECLIPS_CPU_DECODE").is_none()).and_then(|c| Decoder::open_card(job.source, &c).ok());
    let Some(mut decoder) = on_card.or_else(|| Decoder::open_threads(job.source, Output::Screen, decode_threads()).ok()) else { return Ok(false) };
    eprintln!("export: decoding with {}", decoder.way());
    decoder.set_fps(job.fps);
    let source = decoder.source_size();
    let timeline = Timeline::new(job);
    if timeline.frames == 0 {
        bail!("nothing to save");
    }

    // Cropped and scaled as decoded, in YUV, as ffmpeg's filters do: a crop
    // comes out exactly as it went in. In RGB, colours a screen can't show
    // came out clipped, and detailed pictures have plenty: in a fractal
    // flown over, a fifth of the pixels, losing 3 dB against ffmpeg. Through
    // RGB only to blend (in light, as live) or to make BT.601 the BT.709
    // the file says it is.
    let shutter = job.blend.map(|(rate, shape)| Shutter::new(rate / job.out_fps, shape));
    let mut maker = if shutter.is_none() && !decoder.bt601() {
        Maker::Scaled(Nv12Scaler::new(card.clone(), source, job.crop, job.size))
    } else {
        let place = ScreenPlacement {
            crop: job.crop.map(|[x, y, w, h]| Crop { w, h, at: Some((x, y)) }),
            ..ScreenPlacement::whole(Framing::Stretch)
        };
        let mut pic = GpuPicture::new(card.clone(), job.size.0, job.size.1, place);
        if let Some(s) = &shutter {
            pic.start_blending(s.frames_open());
        }
        Maker::Drawn { pic: Box::new(pic), upload: Upload::new(&card, source) }
    };

    let partial = dest.with_extension("partial.mp4");
    let mut encoder = Encoder::start(job, maker.source(), &partial)?;

    // The source frames wanted, in order, each once: decoded ahead on a
    // thread of their own while the card works on the ones before.
    let wanted = timeline.source_frames(job, shutter.as_ref());
    let (tx, rx) = mpsc::sync_channel::<Result<(u64, Nv12, bool)>>(6);
    let on_card = decoder.pool_size().is_some();
    let decoding = {
        let wanted = wanted.clone();
        thread::Builder::new().name("export decode".into()).spawn(move || {
            for i in wanted {
                // Playback short of the card's video decoder (a clip being
                // watched meanwhile): it comes first.
                while on_card && crate::playback_short() {
                    thread::sleep(Duration::from_millis(20));
                }
                let got = decoder.frame(i).and_then(|p| {
                    let p = p.context("a frame the decoder doesn't have")?;
                    let s = p.gpu.context("a frame not in NV12")?;
                    Ok((i, s.nv12, s.bt601))
                });
                let failed = got.is_err();
                if tx.send(got).is_err() || failed {
                    break;
                }
            }
        })?
    };
    let made = (|| -> Result<()> {
        let next = || -> Result<(Nv12, bool)> {
            let (_, nv12, bt601) = rx.recv().map_err(|_| anyhow!("the decoder stopped"))??;
            Ok((nv12, bt601))
        };
        let mut shown: Option<u64> = None;
        let mut version = 0u64;
        // Frames on the card put up since the last frame was encoded: their
        // buffers go back to the decoder once that's done (an encode waits
        // for everything the card was given before it).
        let mut held: Vec<Nv12> = Vec::new();
        match (&shutter, &mut maker) {
            (None, _) => {
                for k in 0..timeline.frames {
                    let i = timeline.shown(job, k);
                    if shown != Some(i) {
                        let (nv12, bt601) = next()?;
                        match &mut maker {
                            Maker::Scaled(scaler) => scaler.put(&nv12),
                            Maker::Drawn { pic, upload } => {
                                let frame = upload.put(&card, &nv12, bt601);
                                ensure_drawn(pic.draw(&frame, i))?;
                            }
                        }
                        held.push(nv12);
                        shown = Some(i);
                        version += 1;
                    }
                    encoder.encode(maker.source(), version, k as i64)?;
                    held.clear();
                    progress(k as f32 / timeline.frames as f32);
                }
            }
            (Some(s), Maker::Drawn { pic, upload }) => {
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
                        let (nv12, bt601) = next()?;
                        pic.hold(&upload.put(&card, &nv12, bt601), i);
                        held.push(nv12);
                        shown = Some(i);
                    }
                    for (k, w) in [(first, weights[0]), (first + 1, weights[1])] {
                        if w > 0.0 && (0..=last).contains(&k) {
                            pic.add(k, w);
                        }
                    }
                    // The frames no later look counts toward are done.
                    let (after, _) = s.split(m + 1);
                    while closed < (after - 1).min(last) {
                        closed += 1;
                        ensure_drawn(pic.finish(closed))?;
                        version += 1;
                        encoder.encode(&**pic, version, closed)?;
                        held.clear();
                        progress(closed as f32 / timeline.frames as f32);
                    }
                    m += 1;
                }
                while closed < last {
                    closed += 1;
                    ensure_drawn(pic.finish(closed))?;
                    version += 1;
                    encoder.encode(&**pic, version, closed)?;
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
    if result.is_err() {
        let _ = std::fs::remove_file(&partial);
    }
    result.map(|()| true)
}

fn ensure_drawn(drawn: bool) -> Result<()> {
    if drawn { Ok(()) } else { bail!("a frame couldn't be made on the graphics card") }
}

/// Threads to decode on: most of the CPU, the export being what's asked for.
fn decode_threads() -> std::ffi::c_int {
    thread::available_parallelism().map_or(4, |n| n.get()).clamp(2, 12) as std::ffi::c_int
}

/// How the frames are made on the card.
enum Maker {
    /// Cropped and scaled as decoded.
    Scaled(Nv12Scaler),
    /// Drawn in RGB by the recording's frame code.
    Drawn { pic: Box<GpuPicture>, upload: Upload },
}

impl Maker {
    /// Where the encoder takes the frame made from.
    fn source(&self) -> &dyn Nv12Source {
        match self {
            Self::Scaled(s) => s,
            Self::Drawn { pic, .. } => &**pic,
        }
    }
}

/// A buffer on the card for a decoded frame: NV12 at `(w, h)` (the U, V
/// pairs half that each way, rounded up), as the decoder hands it out.
fn decoded_buffer(card: &Gpu, (w, h): (u32, u32)) -> wgpu::Buffer {
    let bytes = (w as u64 * h as u64 + 2 * w.div_ceil(2) as u64 * h.div_ceil(2) as u64).next_multiple_of(4);
    card.device.create_buffer(&wgpu::BufferDescriptor { label: Some("decoded nv12"), size: bytes, usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false })
}

/// Decoded frames cropped and scaled on the card as they are, NV12 to the
/// encoder's NV12, each plane on its own: by Lanczos (3 lobes, as ffmpeg's
/// export scaled: sharper than averaging the area each pixel covers, which
/// kept 8% less of a detailed picture's finest detail), across into rows
/// of the new width (once a frame), then down into the encoder's picture.
/// The same size is an exact copy.
struct Nv12Scaler {
    gpu: Arc<Gpu>,
    across: wgpu::ComputePipeline,
    down: wgpu::ComputePipeline,
    /// A frame decoded on the CPU, put up, and the rows made the new width.
    decoded: wgpu::Buffer,
    rows: wgpu::Buffer,
    /// Each output sample's first source sample and its weights, for the
    /// four ways (luma across, chroma across, luma down, chroma down).
    firsts: wgpu::Buffer,
    weights: wgpu::Buffer,
    /// Everything but the encoder's picture (`Scale`, its size and pitch 0),
    /// for frames as the CPU decodes them.
    layout: [u32; 28],
    across_bind: wgpu::BindGroup,
    /// Frames on the card ([`Nv12::Card`], their rows further apart): what
    /// they're read across with.
    card_uniforms: OnceLock<wgpu::Buffer>,
    /// How many of `rows` there are.
    row_values: u32,
}

const SCALE_NV12_SHADER: &str = r#"
struct Scale {
    // The part of the source used (x, y, w, h, in its pixels: even).
    crop: vec4<u32>,
    // The source's size, and the frame's.
    source: vec2<u32>,
    frame: vec2<u32>,
    // The encoder's picture: its size (the frame's, or more: the edges
    // repeated past it) and bytes from one row to the next (the width, or
    // more: aligned rows).
    size: vec2<u32>,
    pitch: u32,
    _pad: u32,
    // Weights a sample, and where each way's start in `firsts` and
    // `weights`: luma across, chroma across, luma down, chroma down.
    taps: vec4<u32>,
    first_at: vec4<u32>,
    weight_at: vec4<u32>,
    // The decoded frame's rows: bytes from one luma row to the next, where
    // the U, V pairs' rows start, and bytes from one of those to the next.
    frame_rows: vec4<u32>,
}

@group(0) @binding(0) var<storage, read> decoded: array<u32>;
@group(0) @binding(1) var<storage, read_write> rows: array<f32>;
@group(0) @binding(2) var<uniform> s: Scale;
@group(0) @binding(3) var<storage, read> firsts: array<i32>;
@group(0) @binding(4) var<storage, read> weights: array<f32>;
@group(0) @binding(5) var<storage, read_write> nv12: array<u32>;

fn decoded_byte(i: u32) -> f32 {
    return f32((decoded[i / 4u] >> ((i % 4u) * 8u)) & 0xFFu);
}

// Output sample `i` one way (`way`: 0 luma across, 1 chroma across, 2 luma
// down, 3 chroma down): its first source sample, from `lo` to `hi` (the
// edges repeated past them).
fn tap(way: u32, i: u32, t: u32, lo: u32, hi: u32) -> u32 {
    return u32(clamp(firsts[s.first_at[way] + i] + i32(t), i32(lo), i32(hi)));
}

fn weight(way: u32, i: u32, t: u32) -> f32 {
    return weights[s.weight_at[way] + i * s.taps[way] + t];
}

// Across: each of the crop's rows (luma, then the U, V pairs' rows) made the
// frame's width, a value a thread.
@compute @workgroup_size(64)
fn across(@builtin(global_invocation_id) id: vec3<u32>) {
    let i = id.x + id.y * 65535u * 64u;
    let fw = s.frame.x;
    let luma = s.crop.w * fw;
    if (i >= luma + s.crop.w / 2u * fw) {
        return;
    }
    var sum = 0.0;
    if (i < luma) {
        let x = i % fw;
        let row = (s.crop.y + i / fw) * s.frame_rows.x;
        for (var t = 0u; t < s.taps[0]; t += 1u) {
            sum += decoded_byte(row + tap(0u, x, t, s.crop.x, s.crop.x + s.crop.z - 1u)) * weight(0u, x, t);
        }
    } else {
        let k = i - luma;
        let x = (k % fw) / 2u;
        let row = s.frame_rows.y + (s.crop.y / 2u + k / fw) * s.frame_rows.z + k % 2u;
        let lo = s.crop.x / 2u;
        for (var t = 0u; t < s.taps[1]; t += 1u) {
            sum += decoded_byte(row + 2u * tap(1u, x, t, lo, lo + s.crop.z / 2u - 1u)) * weight(1u, x, t);
        }
    }
    rows[i] = sum;
}

// One byte of the encoder's picture: the luma rows, then the U, V pairs'
// rows at half the size each way, each down from the rows made `across`.
// A row's padding past the width is 0.
fn byte_at(b: u32) -> u32 {
    let w = s.size.x;
    let h = s.size.y;
    let p = s.pitch;
    let fw = s.frame.x;
    var sum = 0.0;
    if (b < p * h) {
        let x = b % p;
        if (x >= w) {
            return 0u;
        }
        let at = min(vec2<u32>(x, b / p), s.frame - 1u);
        for (var t = 0u; t < s.taps[2]; t += 1u) {
            let r = tap(2u, at.y, t, s.crop.y, s.crop.y + s.crop.w - 1u) - s.crop.y;
            sum += rows[r * fw + at.x] * weight(2u, at.y, t);
        }
    } else {
        let k = b - p * h;
        let x = k % p;
        if (k >= p * h / 2u || x >= w) {
            return 0u;
        }
        let at = min(vec2<u32>(x / 2u, k / p), s.frame / 2u - 1u);
        let lo = s.crop.y / 2u;
        for (var t = 0u; t < s.taps[3]; t += 1u) {
            let r = tap(3u, at.y, t, lo, lo + s.crop.w / 2u - 1u) - lo;
            sum += rows[s.crop.w * fw + r * fw + at.x * 2u + x % 2u] * weight(3u, at.y, t);
        }
    }
    return u32(clamp(round(sum), 0.0, 255.0));
}

// Down: four bytes of the picture a thread.
@compute @workgroup_size(64)
fn down(@builtin(global_invocation_id) id: vec3<u32>) {
    let i = id.x + id.y * 65535u * 64u;
    if (i >= arrayLength(&nv12)) {
        return;
    }
    let b = i * 4u;
    nv12[i] = byte_at(b) | (byte_at(b + 1u) << 8u) | (byte_at(b + 2u) << 16u) | (byte_at(b + 3u) << 24u);
}
"#;

impl Nv12Scaler {
    /// For frames `size` big, `crop` of them used (or all), made `frame` big.
    fn new(gpu: Arc<Gpu>, size: (u32, u32), crop: Option<[u32; 4]>, frame: (u32, u32)) -> Self {
        let device = &gpu.device;
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("scale nv12"), source: wgpu::ShaderSource::Wgsl(SCALE_NV12_SHADER.into()) });
        let pipeline = |entry: &str| device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor { label: Some(entry), layout: None, module: &module, entry_point: Some(entry), compilation_options: Default::default(), cache: None });
        let (across, down) = (pipeline("across"), pipeline("down"));
        let [cx, cy, cw, ch] = crop.unwrap_or([0, 0, size.0, size.1]);
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
        let storage = |label, bytes: &[u8], usage| {
            let buffer = device.create_buffer(&wgpu::BufferDescriptor { label: Some(label), size: (bytes.len() as u64).next_multiple_of(4).max(4), usage: usage | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
            gpu.queue.write_buffer(&buffer, 0, bytes);
            buffer
        };
        let firsts = storage("lanczos firsts", &firsts.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>(), wgpu::BufferUsages::STORAGE);
        let weights = storage("lanczos weights", &weights.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>(), wgpu::BufferUsages::STORAGE);
        let mut layout = [0u32; 28];
        layout[..12].copy_from_slice(&[cx, cy, cw, ch, size.0, size.1, fw, fh, 0, 0, 0, 0]);
        layout[12..16].copy_from_slice(&taps);
        layout[16..20].copy_from_slice(&first_at);
        layout[20..24].copy_from_slice(&weight_at);
        // Rows as the CPU decodes them: tight, the pairs after the luma.
        layout[24..28].copy_from_slice(&[size.0, size.0 * size.1, size.0.div_ceil(2) * 2, 0]);
        let decoded = decoded_buffer(&gpu, size);
        let row_values = ch * fw + ch / 2 * fw;
        let rows = device.create_buffer(&wgpu::BufferDescriptor { label: Some("rows across"), size: row_values as u64 * 4, usage: wgpu::BufferUsages::STORAGE, mapped_at_creation: false });
        let uniforms = storage("scale nv12", &layout.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>(), wgpu::BufferUsages::UNIFORM);
        let across_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("across"),
            layout: &across.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: decoded.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: rows.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: uniforms.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 3, resource: firsts.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 4, resource: weights.as_entire_binding() },
            ],
        });
        Self { gpu, across, down, decoded, rows, firsts, weights, layout, across_bind, card_uniforms: OnceLock::new(), row_values }
    }

    /// The next frame, `nv12` as decoded, made the new width: read where it
    /// is on the card, or put up first.
    fn put(&self, nv12: &Nv12) {
        let card_bind;
        let bind = match nv12 {
            Nv12::Memory(nv12) => {
                let len = nv12.len().min(self.decoded.size() as usize) / 4 * 4;
                self.gpu.queue.write_buffer(&self.decoded, 0, &nv12[..len]);
                &self.across_bind
            }
            Nv12::Card(f) => {
                let uniforms = self.card_uniforms.get_or_init(|| {
                    let mut layout = self.layout;
                    layout[24..28].copy_from_slice(&[f.pitch(), f.chroma_at() as u32, f.pitch(), 0]);
                    let buffer = self.gpu.device.create_buffer(&wgpu::BufferDescriptor { label: Some("scale card frames"), size: 112, usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
                    self.gpu.queue.write_buffer(&buffer, 0, &layout.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>());
                    buffer
                });
                card_bind = self.gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("across a card frame"),
                    layout: &self.across.get_bind_group_layout(0),
                    entries: &[
                        wgpu::BindGroupEntry { binding: 0, resource: f.buffer().as_entire_binding() },
                        wgpu::BindGroupEntry { binding: 1, resource: self.rows.as_entire_binding() },
                        wgpu::BindGroupEntry { binding: 2, resource: uniforms.as_entire_binding() },
                        wgpu::BindGroupEntry { binding: 3, resource: self.firsts.as_entire_binding() },
                        wgpu::BindGroupEntry { binding: 4, resource: self.weights.as_entire_binding() },
                    ],
                });
                &card_bind
            }
        };
        let groups = self.row_values.div_ceil(64);
        let mut enc = self.gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("across") });
        {
            let mut pass = enc.begin_compute_pass(&Default::default());
            pass.set_pipeline(&self.across);
            pass.set_bind_group(0, bind, &[]);
            pass.dispatch_workgroups(groups.min(65535), groups.div_ceil(65535), 1);
        }
        self.gpu.queue.submit([enc.finish()]);
    }
}

impl Nv12Source for Nv12Scaler {
    fn gpu(&self) -> &Arc<Gpu> {
        &self.gpu
    }

    fn nv12_target(&self, buffer: wgpu::Buffer, pitch: u32, size: (u32, u32), picture: Option<wgpu::Texture>) -> Nv12Target {
        let device = &self.gpu.device;
        let mut layout = self.layout;
        layout[8..11].copy_from_slice(&[size.0, size.1, pitch]);
        let uniforms = device.create_buffer(&wgpu::BufferDescriptor { label: Some("scale nv12"), size: 112, usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
        self.gpu.queue.write_buffer(&uniforms, 0, &layout.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>());
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("down"),
            layout: &self.down.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry { binding: 1, resource: self.rows.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: uniforms.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 3, resource: self.firsts.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 4, resource: self.weights.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 5, resource: buffer.as_entire_binding() },
            ],
        });
        Nv12Target::new(buffer, bind, pitch, size, picture)
    }

    fn nv12_to(&self, target: &Nv12Target) {
        target.fill(&self.gpu, &self.down);
    }
}

/// Frames read back from the card (for x264), made into a buffer of our
/// own as for an encoder of the card's.
struct Readback {
    target: Nv12Target,
    made: wgpu::Buffer,
    read: wgpu::Buffer,
    len: usize,
}

impl Readback {
    fn new(src: &dyn Nv12Source, (w, h): (u32, u32)) -> Self {
        let device = &src.gpu().device;
        let len = nv12_len(w, h);
        let bytes = len.next_multiple_of(4) as u64;
        let made = device.create_buffer(&wgpu::BufferDescriptor { label: Some("nv12 made"), size: bytes, usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC, mapped_at_creation: false });
        let read = device.create_buffer(&wgpu::BufferDescriptor { label: Some("nv12 read"), size: bytes, usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false });
        Self { target: src.nv12_target(made.clone(), w, (w, h), None), made, read, len }
    }

    /// The frame `src` has made, as NV12 into `out`.
    fn read(&self, src: &dyn Nv12Source, out: &mut Vec<u8>) {
        src.nv12_to(&self.target);
        let card = src.gpu();
        let mut enc = card.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("nv12 read") });
        enc.copy_buffer_to_buffer(&self.made, 0, &self.read, 0, self.made.size());
        card.queue.submit([enc.finish()]);
        let slice = self.read.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        let _ = card.device.poll(wgpu::PollType::wait_indefinitely());
        out.clear();
        if let Ok(data) = slice.get_mapped_range() {
            out.extend_from_slice(&data[..self.len]);
        }
        self.read.unmap();
    }
}

/// Puts decoded NV12 frames up on the card as RGB, into two textures in
/// turn (blending still reads the one before while the next goes up).
struct Upload {
    to_rgb: wgpu::ComputePipeline,
    nv12: wgpu::Buffer,
    uniforms: wgpu::Buffer,
    textures: [wgpu::Texture; 2],
    next: usize,
    size: (u32, u32),
}

const TO_RGB_SHADER: &str = r#"
struct Size {
    width: u32,
    height: u32,
    bt601: u32,
    // Bytes from one luma row to the next, where the U, V pairs' rows
    // start, and bytes from one of those to the next.
    pitch: u32,
    chroma_at: u32,
    chroma_pitch: u32,
    _pad: vec2<u32>,
}

@group(0) @binding(0) var<storage, read> nv12: array<u32>;
@group(0) @binding(1) var picture: texture_storage_2d<rgba8unorm, write>;
@group(0) @binding(2) var<uniform> s: Size;

fn byte_at(i: u32) -> f32 {
    return f32((nv12[i / 4u] >> ((i % 4u) * 8u)) & 0xFFu);
}

// Studio-range NV12 (the luma, then Cb, Cr pairs at half size each way) to
// RGB, as the decoder's own conversion (`decode::nv12_to_rgba`).
@compute @workgroup_size(8, 8)
fn to_rgb(@builtin(global_invocation_id) id: vec3<u32>) {
    if (id.x >= s.width || id.y >= s.height) {
        return;
    }
    let l = 1.164 * (byte_at(id.y * s.pitch + id.x) - 16.0);
    let c = s.chroma_at + (id.y / 2u) * s.chroma_pitch + (id.x / 2u) * 2u;
    let cb = byte_at(c) - 128.0;
    let cr = byte_at(c + 1u) - 128.0;
    var k = vec4<f32>(1.793, 0.213, 0.533, 2.112);
    if (s.bt601 != 0u) {
        k = vec4<f32>(1.596, 0.392, 0.813, 2.017);
    }
    let rgb = vec3<f32>(l + k.x * cr, l - k.y * cb - k.z * cr, l + k.w * cb);
    textureStore(picture, vec2<i32>(id.xy), vec4<f32>(clamp(round(rgb) / 255.0, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0));
}
"#;

impl Upload {
    fn new(card: &Gpu, (w, h): (u32, u32)) -> Self {
        let device = &card.device;
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("to rgb"), source: wgpu::ShaderSource::Wgsl(TO_RGB_SHADER.into()) });
        let to_rgb = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor { label: Some("to rgb"), layout: None, module: &module, entry_point: Some("to_rgb"), compilation_options: Default::default(), cache: None });
        let nv12 = decoded_buffer(card, (w, h));
        let uniforms = device.create_buffer(&wgpu::BufferDescriptor { label: Some("to rgb"), size: 32, usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
        let texture = || {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some("decoded frame"),
                size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            })
        };
        Self { to_rgb, nv12, uniforms, textures: [texture(), texture()], next: 0, size: (w, h) }
    }

    /// The frame `nv12` as RGB on the card: read where it is on the card, or
    /// put up first.
    fn put(&mut self, card: &Gpu, nv12: &Nv12, bt601: bool) -> Frame {
        let texture = self.textures[self.next].clone();
        self.next = 1 - self.next;
        let (w, h) = self.size;
        let (source, rows) = match nv12 {
            Nv12::Memory(nv12) => {
                let len = (nv12.len().min(self.nv12.size() as usize)) / 4 * 4;
                card.queue.write_buffer(&self.nv12, 0, &nv12[..len]);
                (&self.nv12, [w, w * h, w.div_ceil(2) * 2])
            }
            Nv12::Card(f) => (f.buffer(), [f.pitch(), f.chroma_at() as u32, f.pitch()]),
        };
        let sizes: Vec<u8> = [w, h, u32::from(bt601), rows[0], rows[1], rows[2], 0, 0].iter().flat_map(|v| v.to_le_bytes()).collect();
        card.queue.write_buffer(&self.uniforms, 0, &sizes);
        let view = texture.create_view(&Default::default());
        let bind = card.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("to rgb"),
            layout: &self.to_rgb.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: source.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&view) },
                wgpu::BindGroupEntry { binding: 2, resource: self.uniforms.as_entire_binding() },
            ],
        });
        let mut enc = card.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("to rgb") });
        {
            let mut pass = enc.begin_compute_pass(&Default::default());
            pass.set_pipeline(&self.to_rgb);
            pass.set_bind_group(0, &bind, &[]);
            pass.dispatch_workgroups(self.size.0.div_ceil(8), self.size.1.div_ceil(8), 1);
        }
        card.queue.submit([enc.finish()]);
        Frame { width: self.size.0, height: self.size.1, stride: 0, order: Order::Rgb, data: Vec::new(), texture: Some(texture) }
    }
}

/// Where the frames go: an encoder of the card's into our MP4 writer, or
/// x264 (through ffmpeg) writing the file itself.
enum Encoder {
    Nvenc(Nvenc, Writer),
    Vulkan(VkEncoder, Writer),
    X264 { child: Child, stdin: ChildStdin, readback: Readback, buf: Vec<u8>, holds: Option<u64> },
}

impl Encoder {
    fn start(job: &Job, src: &dyn Nv12Source, partial: &Path) -> Result<Self> {
        let (w, h) = job.size;
        if !job.fit {
            // NVIDIA's encoder makes B-frames (up to 3, so frames are shown up
            // to 3 after they're decoded); Vulkan Video's here doesn't.
            let writer = |reorder: u32| {
                let spec = Spec { width: w, height: h, fps: job.out_fps, audio_titles: Vec::new(), audio_bitrate: 0, comment: String::new(), mov: false, reorder };
                Writer::record(partial.to_path_buf(), Layout { spec }, job.out_fps)
            };
            let only = std::env::var("HESTECLIPS_ENCODER").ok();
            let wanted = |name: &str| only.as_deref().is_none_or(|o| o == name);
            if wanted("nvenc") {
                let wr = writer(3);
                match Nvenc::start(w, h, job.out_fps, job.kbps, wr.tx.clone(), Some(src), true) {
                    Ok(Some(e)) => return Ok(Self::Nvenc(e, wr)),
                    Ok(None) => {}
                    Err(e) => eprintln!("export: NVIDIA's encoder didn't start: {e:#}"),
                }
            }
            if wanted("vulkan") {
                let wr = writer(0);
                match VkEncoder::start(w, h, job.out_fps, job.kbps, wr.tx.clone(), src, true) {
                    Ok(Some(e)) => return Ok(Self::Vulkan(e, wr)),
                    Ok(None) => {}
                    Err(e) => eprintln!("export: the card's encoder (Vulkan Video) didn't start: {e:#}"),
                }
            }
        }
        // x264: the best in few bits (a size to fit), or where the card
        // has no encoder. B-frames and all, ffmpeg writing the file.
        let rate = format!("{}k", job.kbps);
        let mut cmd = super::ffmpeg::ffmpeg();
        cmd.args(["-y", "-f", "rawvideo", "-pix_fmt", "nv12", "-video_size", &format!("{w}x{h}"), "-framerate", &job.out_fps.to_string()])
            .args(["-color_range", "tv", "-colorspace", "bt709", "-color_primaries", "bt709", "-color_trc", "bt709", "-i", "pipe:0"])
            .args(["-c:v", "libx264", "-preset", "medium", "-b:v", &rate, "-profile:v", "high", "-pix_fmt", "yuv420p"]);
        if job.fit {
            cmd.args(["-maxrate", &rate, "-bufsize", &format!("{}k", job.kbps * 2)]);
        }
        cmd.args(["-color_range", "tv", "-colorspace", "bt709", "-color_primaries", "bt709", "-color_trc", "bt709", "-an", "-f", "mp4"]).arg(partial);
        let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().context("couldn't run ffmpeg for x264")?;
        let stdin = child.stdin.take().context("no pipe to ffmpeg")?;
        Ok(Self::X264 { child, stdin, readback: Readback::new(src, (w, h)), buf: Vec::new(), holds: None })
    }

    /// Encode the frame `src` has made (picture number `version`) as frame
    /// `n`, waiting while the encoder is busy: an export skips nothing.
    fn encode(&mut self, src: &dyn Nv12Source, version: u64, n: i64) -> Result<()> {
        match self {
            Self::Nvenc(e, _) => {
                while !e.encode_gpu(src, version, n) {
                    if e.failed() {
                        bail!("NVIDIA's encoder stopped");
                    }
                    thread::sleep(Duration::from_micros(200));
                }
            }
            Self::Vulkan(e, _) => {
                while !e.encode_gpu(src, version, n) {
                    if e.failed() {
                        bail!("the card's encoder stopped");
                    }
                    thread::sleep(Duration::from_micros(200));
                }
            }
            Self::X264 { stdin, readback, buf, holds, .. } => {
                if *holds != Some(version) {
                    readback.read(src, buf);
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
            Self::Nvenc(e, w) => {
                let failed = e.failed();
                e.finish();
                w.join()?;
                if failed {
                    bail!("NVIDIA's encoder stopped");
                }
            }
            Self::Vulkan(e, w) => {
                let failed = e.failed();
                e.finish();
                w.join()?;
                if failed {
                    bail!("the card's encoder stopped");
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
