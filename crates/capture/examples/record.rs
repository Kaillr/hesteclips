//! Record with this platform's backend to `<temp>/hc`, printing live meters.
//!
//! Args are sources: `mic:<device>`, `desktop`, `desktop-excl` (desktop minus app
//! sources), `app:<bundle id or exe>`. Append `@mix`, `@track` to limit where it
//! goes (default both). Env: REPLAY=1, SECS=n, EXT=mov, HEIGHT=n (0 = native),
//! FPS=n, SOFTWARE=1, SCREEN=<id>, APP=<exe>[,<exe>…] (record those apps' windows,
//! following focus).
use capture::sources::{AudioSource, SourceKind};
use capture::{EncodeSettings, Mode, mixer::LiveAudio};

fn main() -> anyhow::Result<()> {
    let sources: Vec<AudioSource> = std::env::args()
        .skip(1)
        .enumerate()
        .map(|(i, arg)| {
            let (spec, route) = arg.split_once('@').map_or((arg.as_str(), ""), |(a, b)| (a, b));
            let kind = match spec.split_once(':') {
                Some(("mic", d)) => SourceKind::Microphone { device: d.into() },
                Some(("app", b)) => SourceKind::App { bundle_id: b.into() },
                _ if spec == "desktop-excl" => SourceKind::Desktop { exclude_app_sources: true },
                _ => SourceKind::Desktop { exclude_app_sources: false },
            };
            AudioSource {
                id: format!("s{i}"),
                name: spec.rsplit(':').next().unwrap_or(spec).to_owned(),
                kind,
                in_mix: route != "track",
                own_track: route != "mix",
            }
        })
        .collect();
    let live = LiveAudio::new();
    let mode = if std::env::var("REPLAY").is_ok() { Mode::ReplayBuffer } else { Mode::Record };
    let mut rec = capture::default_recorder(live.clone());
    let env = |k: &str| std::env::var(k).ok();
    rec.start(mode, &EncodeSettings {
        output_dir: std::env::temp_dir().join("hc"),
        container_ext: env("EXT").unwrap_or("mp4".into()),
        fps: env("FPS").and_then(|f| f.parse().ok()).unwrap_or(60),
        video_bitrate_kbps: 12000,
        target_height: match env("HEIGHT").and_then(|h| h.parse().ok()) {
            Some(0) => None,
            Some(h) => Some(h),
            None => Some(720),
        },
        keyframe_interval_secs: 2,
        use_hardware: env("SOFTWARE").is_none(),
        replay_seconds: 5,
        video: match env("APP") {
            Some(ids) => capture::VideoSource::Apps {
                ids: ids.split(',').map(str::to_owned).collect(),
                away_when_unfocused: env("AWAY").is_some(),
            },
            None => capture::VideoSource::Screen { id: env("SCREEN").unwrap_or_default() },
        },
        away_screen: None,
        webcam: env("WEBCAM").map(|device| capture::webcam::Webcam {
            device: if device.is_empty() || device == "1" {
                capture::webcam::list_cameras().first().map(|c| c.id.clone()).unwrap_or_default()
            } else {
                device
            },
            format: None,
            placement: std::sync::Arc::new(std::sync::Mutex::new(capture::webcam::Placement::default_for(16.0 / 9.0, 16.0 / 9.0))),
        }),
        sources: sources.clone(),
    })?;
    let secs: u64 = std::env::var("SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(4);
    for _ in 0..secs * 2 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let mut line = String::new();
        for s in &sources {
            let ch = live.channel(&s.id);
            let peak = ch.meter.take().max_peak();
            line += &format!("{}[{:?}] {:>6.1}dB  ", s.name, ch.status(), capture_db(peak));
        }
        let peak = live.master.take().max_peak();
        println!("{line}MIX {:>6.1}dB", capture_db(peak));
    }
    if mode == Mode::ReplayBuffer {
        // Two clips half a second apart; the second is asked for while the
        // first is still being written.
        let first = rec.save_clip();
        std::thread::sleep(std::time::Duration::from_millis(500));
        let second = rec.save_clip();
        for (n, pending) in [first, second].into_iter().enumerate() {
            println!("save {}: {:?}", n + 1, pending.and_then(|p| p.finish()));
        }
    }
    println!("{:?}", rec.stop()?);
    Ok(())
}

fn capture_db(x: f32) -> f32 {
    if x <= 1e-5 { -90.0 } else { 20.0 * x.log10() }
}
