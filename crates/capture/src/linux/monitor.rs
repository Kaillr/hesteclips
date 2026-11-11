//! "Listen" on Linux: a microphone played live, through PipeWire (what
//! records it), as `crate::monitor` does elsewhere. Its devices are PipeWire
//! nodes, which the audio library used elsewhere doesn't know by name.
//!
//! A capture stream on the mic and a playback stream to the default output,
//! both asking for 5 ms at a time, with a short queue between them; the
//! same processing as there (Mono, noise removal, the source's volume).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Result, anyhow};
use pipewire as pw;
use pw::spa;

use crate::mixer::{Channel, RATE};

/// The quantum asked of both streams: 5 ms.
const QUANTUM: u32 = RATE / 200;
/// Most sound waiting to be played before the oldest is dropped.
const MAX_QUEUE: usize = (RATE / 50) as usize;

/// A microphone being played live; stops when dropped.
pub struct MicMonitor {
    stop: pw::channel::Sender<()>,
    thread: Option<JoinHandle<()>>,
    latency_us: Arc<AtomicU32>,
}

impl MicMonitor {
    /// Play the PipeWire input node named `device` live, at `channel`'s volume.
    pub fn start(device: &str, channel: Arc<Channel>) -> Result<Self> {
        let latency_us = Arc::new(AtomicU32::new(0));
        let (ready_tx, ready_rx) = mpsc::channel::<Result<pw::channel::Sender<()>>>();
        let (device, lat) = (device.to_owned(), latency_us.clone());
        let thread = thread::Builder::new().name("mic-monitor".into()).spawn(move || {
            if let Err(e) = run(&device, channel, lat, &ready_tx) {
                let _ = ready_tx.send(Err(e));
            }
        })?;
        let stop = ready_rx.recv_timeout(Duration::from_secs(5)).map_err(|_| anyhow!("PipeWire didn't answer — is it running?"))??;
        Ok(Self { stop, thread: Some(thread), latency_us })
    }

    /// About how long from the mic to your ears: the two streams' quanta and
    /// the queue (the devices' own delay comes on top).
    pub fn latency_ms(&self) -> Option<f32> {
        let us = self.latency_us.load(Ordering::Relaxed);
        (us > 0).then(|| us as f32 / 1000.0)
    }
}

impl Drop for MicMonitor {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run(device: &str, channel: Arc<Channel>, latency_us: Arc<AtomicU32>, ready: &mpsc::Sender<Result<pw::channel::Sender<()>>>) -> Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None).map_err(|e| anyhow!("can't connect to PipeWire: {e}"))?;
    let queue: Arc<Mutex<VecDeque<f32>>> = Default::default();
    let props = |category: &str| {
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => category,
            *pw::keys::MEDIA_ROLE => "Production",
            *pw::keys::APP_NAME => "HesteClips",
            *pw::keys::NODE_DESCRIPTION => "HesteClips: Listen",
            *pw::keys::NODE_LATENCY => format!("{QUANTUM}/{RATE}"),
        }
    };

    // The mic, as it's recorded: Mono and noise removal (a frame, 10 ms, later).
    let mut input_props = props("Capture");
    input_props.insert("target.object", device);
    let input = pw::stream::StreamRc::new(core.clone(), "hesteclips-listen-in", input_props)?;
    let mut sides = crate::mixer::Mono::new(channel.mono_flag());
    let mut denoise = crate::mixer::Denoise::new(channel.denoise_mode());
    let mut frames = Vec::new();
    let mut mono = Vec::new();
    let q = queue.clone();
    let _input_listener = input
        .add_local_listener_with_user_data(())
        .process(move |stream, _| {
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            let Some(data) = buffer.datas_mut().first_mut() else { return };
            let (offset, size) = (data.chunk().offset() as usize, data.chunk().size() as usize);
            let Some(bytes) = data.data().and_then(|b| b.get(offset..offset + size)) else { return };
            frames.clear();
            frames.extend(bytes.chunks_exact(8).map(|f| [f32::from_le_bytes([f[0], f[1], f[2], f[3]]), f32::from_le_bytes([f[4], f[5], f[6], f[7]])]));
            sides.process(&mut frames);
            mono.clear();
            mono.extend(frames.iter().map(|f| (f[0] + f[1]) * 0.5));
            let out = denoise.process_mono(&mono);
            let mut q = q.lock().unwrap();
            q.extend(out);
            let excess = q.len().saturating_sub(MAX_QUEUE);
            q.drain(..excess);
        })
        .register()?;

    // To the default output, at the source's volume.
    let output = pw::stream::StreamRc::new(core.clone(), "hesteclips-listen-out", props("Playback"))?;
    let mut last = 0.0f32;
    let mut smooth = 0.0f32;
    let q = queue.clone();
    let _output_listener = output
        .add_local_listener_with_user_data(())
        .process(move |stream, _| {
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            let requested = buffer.requested() as usize;
            let Some(data) = buffer.datas_mut().first_mut() else { return };
            let Some(bytes) = data.data() else { return };
            let room = bytes.len() / 8;
            let n = if requested > 0 { requested.min(room) } else { room.min(QUANTUM as usize) };
            let gain = channel.gain();
            let mut q = q.lock().unwrap();
            let queued = q.len();
            for frame in bytes[..n * 8].chunks_exact_mut(8) {
                if let Some(s) = q.pop_front() {
                    last = s;
                }
                let s = (last * gain).to_le_bytes();
                frame[..4].copy_from_slice(&s);
                frame[4..].copy_from_slice(&s);
            }
            drop(q);
            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = 8;
            *chunk.size_mut() = (n * 8) as u32;
            // Its quantum in, the queue, its quantum out, and the noise
            // removal's frame; smoothed, and only moved by whole milliseconds.
            let now = (2 * QUANTUM as usize + queued + crate::mixer::Denoise::FRAME) as f32 / RATE as f32;
            smooth = if smooth == 0.0 { now } else { smooth + (now - smooth) * 0.02 };
            let shown = latency_us.load(Ordering::Relaxed) as f32 / 1e6;
            if (smooth - shown).abs() >= 0.001 {
                latency_us.store(((smooth * 1000.0).round() * 1000.0) as u32, Ordering::Relaxed);
            }
        })
        .register()?;

    let pod = super::audio::stereo_format()?;
    let flags = pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::RT_PROCESS;
    let mut params = [spa::pod::Pod::from_bytes(&pod).ok_or_else(|| anyhow!("bad audio format"))?];
    input.connect(spa::utils::Direction::Input, None, flags, &mut params)?;
    let mut params = [spa::pod::Pod::from_bytes(&pod).ok_or_else(|| anyhow!("bad audio format"))?];
    output.connect(spa::utils::Direction::Output, None, flags, &mut params)?;

    let (stop_tx, stop_rx) = pw::channel::channel::<()>();
    let quit = mainloop.downgrade();
    let _stop = stop_rx.attach(mainloop.loop_(), move |()| {
        if let Some(l) = quit.upgrade() {
            l.quit();
        }
    });
    let _ = ready.send(Ok(stop_tx));
    mainloop.run();
    let _ = input.disconnect();
    let _ = output.disconnect();
    Ok(())
}
