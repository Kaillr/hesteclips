//! Hear a microphone live ("Listen" on the Sources page), to check it works
//! and how it sounds.
//!
//! The mic goes straight to the default output, not through the mixer (which
//! runs deliberately behind, for the meters and the recording): its own input
//! stream, a short queue, and an output stream, both asking for 5 ms buffers.
//! The queue is kept short — anything past [`MAX_QUEUE`] is dropped — so the
//! delay can't build up. The source's volume and mute apply, so you hear
//! what goes into clips.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::mixer::Channel;

/// Buffers asked of both devices (they may give more): 5 ms.
const BUFFER_SECS: f32 = 0.005;
/// Most sound waiting to be played before the oldest is dropped (a guess: a
/// little over the two buffers, so a late callback doesn't click).
const MAX_QUEUE: f32 = 0.02;

/// The input's error handler.
#[cfg(not(target_os = "linux"))]
fn input_errors(device: &str) -> impl FnMut(cpal::Error) + Send + 'static {
    crate::sources::quiet_xruns(format!("{device} (Listen)"))
}

#[cfg(target_os = "linux")]
fn input_errors(_: &str) -> impl FnMut(cpal::Error) + Send + 'static {
    |e| eprintln!("mic monitor (input): {e}")
}

/// A microphone being played live; stops when dropped.
pub struct MicMonitor {
    _input: cpal::Stream,
    _output: cpal::Stream,
    /// Mic to ear, in µs, as the devices report their timing (0 until known).
    latency_us: Arc<AtomicU32>,
}

impl MicMonitor {
    /// Play the input device named `device` (a name, not "default") live, at
    /// `channel`'s volume.
    pub fn start(device: &str, channel: Arc<Channel>) -> Result<Self> {
        let host = cpal::default_host();
        let input = host
            .input_devices()?
            .find(|d| d.description().is_ok_and(|desc| desc.name() == device))
            .context("the microphone isn't connected")?;
        let output = host.default_output_device().context("no audio output device")?;
        let in_config = input.default_input_config()?;
        let out_config = output.default_output_config()?;
        let (in_rate, out_rate) = (in_config.sample_rate() as f32, out_config.sample_rate() as f32);
        let in_channels = in_config.channels() as usize;
        let out_channels = out_config.channels() as usize;
        let small = |range: &cpal::SupportedBufferSize, rate: f32| match range {
            cpal::SupportedBufferSize::Range { min, max } => cpal::BufferSize::Fixed(((rate * BUFFER_SECS) as u32).clamp(*min, *max)),
            cpal::SupportedBufferSize::Unknown => cpal::BufferSize::Default,
        };
        let mut in_stream_config = in_config.config();
        in_stream_config.buffer_size = small(in_config.buffer_size(), in_rate);
        let mut out_stream_config = out_config.config();
        out_stream_config.buffer_size = small(out_config.buffer_size(), out_rate);

        // Mono, at the mic's rate.
        let queue: Arc<Mutex<VecDeque<f32>>> = Default::default();
        let max_queue = (in_rate * MAX_QUEUE) as usize;
        let latency_us = Arc::new(AtomicU32::new(0));
        // The input's buffer: sound is that old when it's handed over.
        let in_buffer = match in_stream_config.buffer_size {
            cpal::BufferSize::Fixed(n) => n as f32 / in_rate,
            cpal::BufferSize::Default => 0.01,
        };

        let q = queue.clone();
        let in_stream = input.build_input_stream::<f32, _, _>(
            in_stream_config,
            move |data, _| {
                let mut q = q.lock().unwrap();
                q.extend(data.chunks(in_channels).map(|f| f.iter().sum::<f32>() / in_channels as f32));
                let excess = q.len().saturating_sub(max_queue);
                q.drain(..excess);
            },
            // Windows flags the first packets after a start as a gap (not lost
            // sound): only real dropouts are reported, and at most once a minute.
            input_errors(device),
            None,
        )?;

        // Mic frames per output frame; the position between two mic frames.
        let step = in_rate / out_rate;
        let mut pos = 0.0f32;
        let mut last = 0.0f32;
        let (q, lat) = (queue, latency_us.clone());
        let out_stream = output.build_output_stream::<f32, _, _>(
            out_stream_config,
            move |out: &mut [f32], info| {
                let gain = channel.gain();
                let mut q = q.lock().unwrap();
                let queued = q.len() as f32 / in_rate;
                for frame in out.chunks_mut(out_channels) {
                    // Linear steps through the mic's samples, at the output's rate.
                    pos += step;
                    while pos >= 1.0 {
                        if let Some(s) = q.pop_front() {
                            last = s;
                        }
                        pos -= 1.0;
                    }
                    let next = q.front().copied().unwrap_or(last);
                    let s = (last + (next - last) * pos) * gain;
                    frame.fill(s);
                }
                drop(q);
                // The mic's buffer, waiting in the queue, then until this
                // buffer is heard.
                let ts = info.timestamp();
                let ahead = ts.playback.duration_since(ts.callback).as_secs_f32();
                lat.store(((in_buffer + queued + ahead) * 1e6) as u32, Ordering::Relaxed);
            },
            |e| eprintln!("mic monitor (output): {e}"),
            None,
        )?;
        in_stream.play()?;
        out_stream.play()?;
        Ok(Self { _input: in_stream, _output: out_stream, latency_us })
    }

    /// About how long from the mic to your ears: its buffer, the queue, and
    /// the output's lead (the devices' own hardware delay comes on top).
    pub fn latency_ms(&self) -> Option<f32> {
        let us = self.latency_us.load(Ordering::Relaxed);
        (us > 0).then(|| us as f32 / 1000.0)
    }
}
