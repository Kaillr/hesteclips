//! Record with the ScreenCaptureKit backend to /tmp/hc, printing live meters.
//!
//! Args are sources: `mic:<device>`, `desktop`, `desktop-excl` (desktop minus app
//! sources), `app:<bundle id>`. Append `@mix`, `@track` to limit where it goes
//! (default both). Env: REPLAY=1, SECS=n, EXT=mkv, HEIGHT=n.
use capture::sources::{AudioSource, SourceKind};
use capture::{EncodeSettings, Mode, Recorder, SckRecorder, mixer::LiveAudio};

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
    let mut rec = SckRecorder::new(live.clone());
    rec.start(mode, &EncodeSettings {
        output_dir: "/tmp/hc".into(),
        container_ext: std::env::var("EXT").unwrap_or("mp4".into()),
        fps: 60,
        video_bitrate_kbps: 12000,
        target_height: std::env::var("HEIGHT").ok().and_then(|h| h.parse().ok()).or(Some(720)),
        keyframe_interval_secs: 2,
        use_hardware: true,
        replay_seconds: 5,
        screen_id: String::new(),
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
        println!("save: {:?}", rec.save_clip());
    }
    println!("{:?}", rec.stop()?);
    Ok(())
}

fn capture_db(x: f32) -> f32 {
    if x <= 1e-5 { -90.0 } else { 20.0 * x.log10() }
}
