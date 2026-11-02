//! A camera's settings, as the Camera page lists them:
//! `cargo run -p capture --example camera_settings -- [name part]`.
//! `SET=<id in hex>=<value>` also sets one, reads it back, and puts it back.
fn main() {
    let part = std::env::args().nth(1).unwrap_or_default();
    let cams = capture::webcam::list_cameras();
    let Some(cam) = cams.iter().find(|c| c.name.contains(&part)) else { return };
    println!("{}", cam.name);
    // `REMEMBER=<id in hex>=<value>`: as the app does, put back as it opens.
    if let Some((id, value)) = std::env::var("REMEMBER").ok().and_then(|s| {
        let (id, v) = s.split_once('=')?;
        Some((u32::from_str_radix(id.trim_start_matches("0x"), 16).ok()?, v.parse::<i64>().ok()?))
    }) {
        capture::webcam::remember_controls(&cam.id, vec![(id, value)]);
    }
    capture::webcam::keep_open(Some((cam.id.clone(), None)));
    let t = std::time::Instant::now();
    while !matches!(capture::webcam::status(), capture::webcam::Status::Live { .. }) && t.elapsed().as_secs() < 10 {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    println!("{:?}", capture::webcam::status());
    let show = || {
        for c in capture::webcam::camera_controls(&cam.id) {
            let kind = match &c.kind {
                capture::webcam::ControlKind::Menu(items) => format!("menu {:?}", items.iter().map(|(v, n)| format!("{v}={n}")).collect::<Vec<_>>()),
                k => format!("{k:?}"),
            };
            println!("{:>10x} {:28} {} = {} (default {}){}", c.id, c.name, kind, c.value, c.default, if c.inactive { " inactive" } else { "" });
        }
    };
    show();
    let t = std::time::Instant::now();
    for _ in 0..10 {
        capture::webcam::camera_controls(&cam.id);
    }
    println!("reading them all: {:.1} ms", t.elapsed().as_secs_f64() * 100.0);
    if let Some((id, value)) = std::env::var("SET").ok().and_then(|s| {
        let (id, v) = s.split_once('=')?;
        Some((u32::from_str_radix(id.trim_start_matches("0x"), 16).ok()?, v.parse::<i64>().ok()?))
    }) {
        let before = capture::webcam::camera_controls(&cam.id).into_iter().find(|c| c.id == id);
        println!("set {id:#x} = {value}: {:?}", capture::webcam::set_camera_control(&cam.id, id, value));
        let after = capture::webcam::camera_controls(&cam.id).into_iter().find(|c| c.id == id);
        println!("now {:?}", after.map(|c| c.value));
        if let Some(b) = before {
            println!("put back {}: {:?}", b.value, capture::webcam::set_camera_control(&cam.id, id, b.value));
        }
    }
    capture::webcam::keep_open(None);
}
