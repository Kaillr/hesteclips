//! A web page drawn over the screen in a recording, checked in the file
//! (`cargo run -p capture --example page_layer`, macOS: WebKit runs on the
//! main thread, which a test doesn't have). The page is in three bands: a
//! colour that changes ten times a second, blue at half alpha, and clear.
//! Needs Screen Recording allowed to the terminal, and ffmpeg.

#[cfg(target_os = "macos")]
fn main() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use capture::layers::{Layer, LayerSource};
    use capture::{EncodeSettings, Framing, Mode, Recorder, ScreenPlacement, VideoSource};
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
    use objc2_foundation::{NSDate, NSRunLoop};

    let mtm = MainThreadMarker::new().expect("the main thread");
    NSApplication::sharedApplication(mtm).setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    let html = r#"<html><body style="margin:0;background:transparent">
<div id=a style="position:absolute;left:0;top:0;width:200px;height:300px;background:rgb(255,0,0)"></div>
<div style="position:absolute;left:200px;top:0;width:100px;height:300px;background:rgba(0,0,255,0.5)"></div>
<script>let n=0;setInterval(()=>{n++;document.getElementById('a').style.background=n%2?'rgb(0,255,0)':'rgb(255,0,0)'},100);</script>
</body></html>"#;
    let url = format!("data:text/html,{}", html.bytes().map(|b| if b.is_ascii_alphanumeric() { (b as char).to_string() } else { format!("%{b:02X}") }).collect::<String>());
    let done = Arc::new(AtomicBool::new(false));
    let done2 = done.clone();
    std::thread::spawn(move || {
        let ok = check(url);
        done2.store(true, Ordering::Relaxed);
        std::process::exit(if ok { 0 } else { 1 });
    });
    while !done.load(Ordering::Relaxed) {
        NSRunLoop::currentRunLoop().runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(0.05));
    }

    /// Record 4 s of the screen with the page over its middle; true if the
    /// file shows it as it should.
    fn check(url: String) -> bool {
        let dir = std::env::temp_dir().join(format!("hc-page-layer-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let screen = capture::list_screens().into_iter().next().expect("a screen");
        let video = VideoSource::Screen { id: screen.id.clone(), hook: Default::default(), method: Default::default() };
        let size = capture::VideoSize { short_side: Some(720), shape: None, exact: None };
        let (w, h) = capture::frame_size(&video, size).unwrap();
        // The page at its own size (400×300 of the video's pixels), at (100, 100).
        let rect = [100.0 / w as f32, 100.0 / h as f32, 400.0 / w as f32, 300.0 / h as f32];
        let page = LayerSource::Browser { url, width: 400, height: 300, css: String::new(), placement: ScreenPlacement { rect, ..ScreenPlacement::whole(Framing::Stretch) } };
        let settings = EncodeSettings {
            output_dir: dir.clone(),
            container_ext: "mp4".into(),
            fps: 30,
            blend_samples: 1,
            blend_shape: Default::default(),
            video_bitrate_kbps: 8000,
            size,
            use_hardware: true,
            replay_seconds: 10,
            video,
            away_screen: None,
            webcam: None,
            layers: Arc::new(Mutex::new(vec![Layer { id: 0, source: LayerSource::Main }, Layer { id: 1, source: page }])),
            screen: Arc::new(Mutex::new(ScreenPlacement::whole(Framing::Fit))),
            sources: Vec::new(),
            stream: None,
        };
        let mut rec = capture::SckRecorder::new(capture::mixer::LiveAudio::new());
        rec.start(Mode::Record, &settings).unwrap();
        std::thread::sleep(Duration::from_secs(4));
        let file = rec.stop(Mode::Record, None).unwrap().expect("a recording");
        // Frames from 2.5 s on, every 50 ms, as RGB.
        let out = std::process::Command::new("ffmpeg").args(["-v", "error", "-ss", "2.5", "-i"]).arg(&file).args(["-frames:v", "10", "-vf", "fps=20", "-f", "rawvideo", "-pix_fmt", "rgb24", "-"]).output().unwrap();
        let frame = (w * h * 3) as usize;
        let frames: Vec<&[u8]> = out.stdout.chunks_exact(frame).collect();
        let px = |f: &[u8], x: u32, y: u32| {
            let i = ((y * w + x) * 3) as usize;
            [f[i], f[i + 1], f[i + 2]]
        };
        let colours: Vec<[u8; 3]> = frames.iter().map(|f| px(f, 200, 250)).collect();
        let (half, clear, beside) = (px(frames[0], 350, 250), px(frames[0], 450, 250), px(frames[0], 50, 250));
        println!("{} frames; the changing band {colours:?}; half blue {half:?}; clear {clear:?} (the screen beside it {beside:?})", frames.len());
        let red = colours.iter().any(|[r, g, b]| *r > 200 && *g < 60 && *b < 60);
        let green = colours.iter().any(|[r, g, b]| *g > 200 && *r < 60 && *b < 60);
        let blueish = half[2] as i32 > half[0] as i32 + 60 && half[2] as i32 > half[1] as i32 + 60;
        let _ = std::fs::remove_dir_all(&dir);
        println!("its colour changes (red and green both seen): {}; half blue is blue over the screen: {blueish}", red && green);
        red && green && blueish
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {}
