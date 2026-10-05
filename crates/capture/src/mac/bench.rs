//! Measurements for the "webcam on its own track" plan (examples
//! `webcam_track_bench`): what a save-time render costs, and what a second
//! encoder costs while capturing. Built from the recorder's own pieces
//! (decoder, compositor, encoder, writer), so the numbers are the app's.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use objc2_core_foundation::{CFBoolean, CFType};
use objc2_video_toolbox::{VTSessionSetProperty, kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality, kVTCompressionPropertyKey_RealTime};

use super::camera::CameraLayer;
use super::decode::{Decoder, Output};
use super::gpu::{FramePool, Gpu};
use super::video::{Frames, Picture};
use crate::avwriter::SendFormat;
use crate::sck::{Encoder, host_now, probe_video_format};
use crate::webcam::{Placement, Webcam};
use crate::writer::{Command, Layout, Media, Writer};
use crate::{EncodeSettings, VideoSource};

fn settings(fps: u32, kbps: u32) -> EncodeSettings {
    EncodeSettings {
        output_dir: std::env::temp_dir(),
        container_ext: "mp4".into(),
        fps,
        video_bitrate_kbps: kbps,
        target_height: None,
        keyframe_interval_secs: 2,
        use_hardware: true,
        replay_seconds: 60,
        video: VideoSource::Screen { id: String::new() },
        away_screen: None,
        webcam: None,
        sources: Vec::new(),
    }
}

fn video_writer(out: &Path, w: usize, h: usize, s: &EncodeSettings) -> Result<Writer> {
    let layout = Layout { video_format: SendFormat(probe_video_format(w, h, s)?), audio_formats: Vec::new(), audio_titles: Vec::new(), comment: String::new() };
    Ok(Writer::record(out.to_path_buf(), layout, s.fps))
}

/// How to run the save-time render.
#[derive(Debug, Clone, Copy)]
pub enum RenderMode {
    /// Decode the screen only.
    DecodeOnly,
    /// Decode both and composite, no encode.
    Compose,
    /// Decode, composite, encode to a file (the real thing).
    Full,
}

/// Render `screen` with `camera` drawn over it (bottom right, a quarter wide)
/// into `out`, as a save would. `offline`: the encoder told it's not real
/// time (and to favour speed). Prints and returns the wall time.
pub fn save_render(screen: &Path, camera: Option<&Path>, out: &Path, mode: RenderMode, kbps: u32, offline: bool, frames: Option<u64>) -> Result<Duration> {
    let t = Instant::now();
    let mut dec = Decoder::open(screen, Output::Screen)?;
    dec.set_fps(60.0);
    let (w, h) = (dec.width as usize, dec.height as usize);
    let mut cam = camera.map(|c| Decoder::open(c, Output::Screen)).transpose()?;
    if let Some(c) = &mut cam {
        c.set_fps(30.0);
    }
    let gpu = Gpu::new()?;
    let pool = FramePool::new(w, h)?;
    let s = settings(60, kbps);
    let (encoder, writer) = if matches!(mode, RenderMode::Full) {
        let writer = video_writer(out, w, h, &s)?;
        let enc = Encoder::new(w, h, &s, writer.tx.clone())?;
        if offline {
            unsafe {
                VTSessionSetProperty(&enc.session, kVTCompressionPropertyKey_RealTime, Some(CFBoolean::new(false) as &CFType));
                VTSessionSetProperty(&enc.session, kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality, Some(CFBoolean::new(true) as &CFType));
            }
        }
        (Some(enc), Some(writer))
    } else {
        (None, None)
    };
    let placement = cam.as_ref().map(|c| {
        let (cw, ch) = c.source_size();
        Placement::default_for(cw as f32 / ch as f32, w as f32 / h as f32)
    });
    let open = t.elapsed();
    let t = Instant::now();
    let (mut n, mut compose_time) = (0u64, Duration::ZERO);
    let mut cam_frame = None;
    let mut cam_index = u64::MAX;
    while frames.is_none_or(|f| n < f) {
        let Some(p) = dec.next()? else { break };
        let surface = p.gpu.context("no GPU frame")?;
        if matches!(mode, RenderMode::DecodeOnly) {
            n += 1;
            continue;
        }
        // The camera frame showing at this time (30 fps under 60).
        if let Some(c) = &mut cam {
            let want = p.index / 2;
            if want != cam_index {
                if let Some(f) = c.frame(want)? {
                    cam_frame = f.gpu;
                }
                cam_index = want;
            }
        }
        let buf = pool.take()?;
        let ct = Instant::now();
        gpu.compose(surface.buffer(), cam_frame.as_ref().map(|c| c.buffer()), placement.unwrap_or_else(Placement::hidden), &buf)?;
        compose_time += ct.elapsed();
        if let Some(e) = &encoder {
            e.encode(&buf, p.index as i64, 60);
        }
        n += 1;
    }
    if let Some(e) = encoder {
        e.finish();
    }
    if let Some(w) = writer {
        w.join()?;
    }
    let total = t.elapsed();
    println!(
        "{mode:?}{}{}: {n} frames {w}x{h} in {:.2} s ({:.0} fps, {:.1}x real time); open {:.0} ms; compose {:.2} ms/frame",
        if camera.is_some() { " +camera" } else { "" },
        if offline { " offline" } else { "" },
        total.as_secs_f64(),
        n as f64 / total.as_secs_f64(),
        n as f64 / 60.0 / total.as_secs_f64(),
        open.as_secs_f64() * 1000.0,
        compose_time.as_secs_f64() * 1000.0 / n.max(1) as f64,
    );
    Ok(total)
}

/// How to run the live capture test.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LiveMode {
    /// The screen only, one encoder.
    Screen,
    /// The webcam composited over the screen, one encoder (today's).
    Composited,
    /// Screen and webcam each to their own encoder (the plan).
    Separate,
}

/// What one encoder did: frames out, how late they came out of the encoder
/// (output time minus when the frame was due), and how late the pacer was.
#[derive(Default)]
struct Stats {
    frames: u64,
    latency_ms: Vec<f64>,
}

/// Forward encoded frames from `rx` to the writer, noting how long after
/// its due time (t0 + pts) each came out.
fn spawn_meter(rx: mpsc::Receiver<Command>, writer: mpsc::Sender<Command>, t0: Arc<Mutex<f64>>, stats: Arc<Mutex<Stats>>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        while let Ok(cmd) = rx.recv() {
            if let Command::Media(Media::Video { pts, .. }) = &cmd {
                let late = host_now() - *t0.lock().unwrap() - pts;
                let mut s = stats.lock().unwrap();
                s.frames += 1;
                s.latency_ms.push(late * 1000.0);
            }
            let _ = writer.send(cmd);
        }
    })
}

/// Encode frames from `next` every 1/fps for `secs`, as the recorder's pacer
/// does. Returns how many were late by more than a frame when encoded.
fn pace(fps: u32, secs: f64, t0: f64, enc: &Encoder, mut next: impl FnMut() -> Option<Arc<super::video::Buffer>>) -> u64 {
    let mut n: i64 = 0;
    let mut late = 0;
    let end = (secs * fps as f64) as i64;
    while n < end {
        let due = t0 + n as f64 / fps as f64;
        let wait = due - host_now();
        if wait > 0.0 {
            thread::sleep(Duration::from_secs_f64(wait));
        } else if -wait > 1.0 / fps as f64 {
            late += 1;
        }
        if let Some(f) = next() {
            let t = Instant::now();
            let took = enc.encode(&f.0, n, fps);
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            if n % 120 == 0 && std::env::var("TRACE").is_ok() {
                println!("    frame {n}: encode call {ms:.1} ms, taken {took}, in flight {}", enc.in_flight());
            }
        }
        n += 1;
    }
    late
}

fn percentiles(v: &mut [f64]) -> String {
    if v.is_empty() {
        return "-".into();
    }
    v.sort_by(f64::total_cmp);
    let at = |p: f64| v[((v.len() - 1) as f64 * p) as usize];
    format!("median {:.1} ms, p99 {:.1}, max {:.1}", at(0.5), at(0.99), at(1.0))
}

/// CPU seconds this process has used.
fn cpu_secs() -> f64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    let t = |tv: libc::timeval| tv.tv_sec as f64 + tv.tv_usec as f64 / 1e6;
    t(ru.ru_utime) + t(ru.ru_stime)
}

/// GPU busy % (ioreg) and the encoder service's CPU %, sampled every half
/// second until `stop`.
fn spawn_sampler(stop: Arc<AtomicBool>) -> thread::JoinHandle<(Vec<f64>, Vec<f64>)> {
    thread::spawn(move || {
        let (mut gpu, mut vt) = (Vec::new(), Vec::new());
        while !stop.load(Ordering::Relaxed) {
            if let Ok(o) = std::process::Command::new("ioreg").args(["-r", "-d", "1", "-w", "0", "-c", "IOAccelerator"]).output() {
                let s = String::from_utf8_lossy(&o.stdout);
                if let Some(v) = s.split("\"Device Utilization %\"=").nth(1).and_then(|r| r.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()) {
                    gpu.push(v);
                }
            }
            if let Ok(o) = std::process::Command::new("ps").args(["-A", "-o", "%cpu=,comm="]).output() {
                let s = String::from_utf8_lossy(&o.stdout);
                let sum: f64 = s.lines().filter(|l| l.contains("VTEncoderXPCService")).filter_map(|l| l.split_whitespace().next()?.parse::<f64>().ok()).sum();
                vt.push(sum);
            }
            thread::sleep(Duration::from_millis(500));
        }
        (gpu, vt)
    })
}

/// Record the main display (and the webcam, as `mode` says) for `secs` into
/// files in `dir`, and report what it cost.
pub fn live(mode: LiveMode, secs: f64, camera: &str, dir: &Path, kbps: u32, cam_kbps: u32, target_height: Option<u32>) -> Result<()> {
    let source = VideoSource::Screen { id: String::new() };
    let (content, display, (w, h)) = super::video::plan(&source, target_height)?;
    let fps = 60;
    let s = settings(fps, kbps);
    let webcam = (mode != LiveMode::Screen).then(|| {
        let placement = Arc::new(Mutex::new(Placement::default_for(16.0 / 9.0, w as f32 / h as f32)));
        Webcam { device: camera.to_string(), format: None, placement }
    });
    // The camera opens first (as it does in the app, kept open by the Sources page).
    if let Some(wc) = &webcam {
        super::camera::keep_open(Some((wc.device.clone(), None)));
        let deadline = Instant::now() + Duration::from_secs(5);
        while super::camera::frame_size().is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
        }
    }
    let picture = Picture::start(&source, &content, &display, (w, h), fps, None)?;
    let composite = if mode == LiveMode::Composited { webcam.as_ref() } else { None };
    let mut frames = Frames::new(picture.latest.clone(), composite, w, h, 0)?;

    let t0 = Arc::new(Mutex::new(0.0));
    let screen_stats = Arc::new(Mutex::new(Stats::default()));
    let screen_out = dir.join(format!("live_{mode:?}_screen.mp4"));
    let screen_writer = video_writer(&screen_out, w, h, &s)?;
    let (stx, srx) = mpsc::channel();
    let screen_meter = spawn_meter(srx, screen_writer.tx.clone(), t0.clone(), screen_stats.clone());
    let screen_enc = Encoder::new(w, h, &s, stx)?;

    let cam_parts = if mode == LiveMode::Separate {
        let (cw, ch) = super::camera::frame_size().context("the camera isn't delivering")?;
        let cs = settings(30, cam_kbps);
        let out = dir.join("live_Separate_camera.mp4");
        let writer = video_writer(&out, cw, ch, &cs)?;
        let stats = Arc::new(Mutex::new(Stats::default()));
        let (tx, rx) = mpsc::channel();
        let meter = spawn_meter(rx, writer.tx.clone(), t0.clone(), stats.clone());
        let enc = Encoder::new(cw, ch, &cs, tx)?;
        Some((enc, writer, meter, stats, out, (cw, ch)))
    } else {
        None
    };

    let stop = Arc::new(AtomicBool::new(false));
    let sampler = spawn_sampler(stop.clone());
    let cpu0 = cpu_secs();
    let start = host_now();
    *t0.lock().unwrap() = start;
    let wall = Instant::now();

    // The camera's own pacer at 30 fps, on its own thread.
    let cam_thread = cam_parts.as_ref().map(|_| {
        let wc = webcam.clone().unwrap();
        let (tx, rx) = mpsc::channel::<u64>();
        let enc_ptr = &cam_parts.as_ref().unwrap().0 as *const Encoder as usize;
        let h = thread::spawn(move || {
            let enc = unsafe { &*(enc_ptr as *const Encoder) };
            let mut layer = CameraLayer::new(&wc);
            let late = pace(30, secs, start, enc, || layer.latest().map(|f| Arc::new(super::video::Buffer(f.buffer.clone()))));
            let _ = tx.send(late);
        });
        (h, rx)
    });
    let screen_late = pace(fps, secs, start, &screen_enc, || frames.next());
    let cam_late = cam_thread.map(|(h, rx)| {
        let _ = h.join();
        rx.recv().unwrap_or(0)
    });
    let elapsed = wall.elapsed().as_secs_f64();
    let cpu = cpu_secs() - cpu0;
    stop.store(true, Ordering::Relaxed);
    let (mut gpu, mut vt) = sampler.join().unwrap();

    screen_enc.finish();
    drop(frames);
    picture.stop();
    let _ = screen_meter.join();
    screen_writer.join()?;
    let cam_report = cam_parts.map(|(enc, writer, meter, stats, out, size)| {
        enc.finish();
        let _ = meter.join();
        let r = writer.join();
        (stats, out, size, r)
    });
    super::camera::keep_open(None);

    let avg = |v: &[f64]| if v.is_empty() { 0.0 } else { v.iter().sum::<f64>() / v.len() as f64 };
    let mut ss = screen_stats.lock().unwrap();
    let ss = &mut *ss;
    println!("== {mode:?}, {secs:.0} s, screen {w}x{h}@{fps} {kbps} kbps ==");
    println!("  screen: {} frames out of {} due, {screen_late} paced late; encoder latency {}", ss.frames, (secs * fps as f64) as u64, percentiles(&mut ss.latency_ms));
    if let Some((stats, out, (cw, ch), r)) = cam_report {
        r?;
        let mut cs = stats.lock().unwrap();
        let cs = &mut *cs;
        println!(
            "  camera: {cw}x{ch}@30 {cam_kbps} kbps, {} frames out of {} due, {} paced late; encoder latency {}; {}",
            cs.frames,
            (secs * 30.0) as u64,
            cam_late.unwrap_or(0),
            percentiles(&mut cs.latency_ms),
            out.display()
        );
    }
    gpu.sort_by(f64::total_cmp);
    vt.sort_by(f64::total_cmp);
    println!(
        "  our CPU {:.0}% of one core; VT encoder service CPU avg {:.0}%; GPU busy avg {:.0}% (max {:.0}); {}",
        cpu / elapsed * 100.0,
        avg(&vt),
        avg(&gpu),
        gpu.last().copied().unwrap_or(0.0),
        screen_out.display()
    );
    Ok(())
}
