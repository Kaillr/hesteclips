//! List the cameras the app can use, with their ids:
//! `cargo run -p capture --example cameras`. `OPEN=<part of a name>` also
//! opens that camera for a few seconds, reporting how it goes.
fn main() {
    let cams = capture::webcam::list_cameras();
    for c in &cams {
        println!("{}\t{}", c.name, c.id);
    }
    if let Ok(part) = std::env::var("OPEN") {
        let Some(c) = cams.iter().find(|c| c.name.contains(&part)) else { return };
        capture::webcam::keep_open(Some((c.id.clone(), None)));
        for _ in 0..10 {
            std::thread::sleep(std::time::Duration::from_millis(500));
            println!("{:?}", capture::webcam::status());
        }
        capture::webcam::keep_open(None);
    }
}
