//! A camera's formats, and for each one the size of the frames that really
//! arrive: `cargo run --release -p capture --example camera_formats -- [name part]`.
fn main() {
    let part = std::env::args().nth(1).unwrap_or_default();
    let cams = capture::webcam::list_cameras();
    let Some(cam) = cams.iter().find(|c| c.name.contains(&part)) else { return };
    println!("{}", cam.name);
    capture::webcam::keep_open(Some((cam.id.clone(), None)));
    std::thread::sleep(std::time::Duration::from_secs(3));
    let formats = capture::webcam::formats(&cam.id);
    println!("auto: {:?} frames {:?}", capture::webcam::status(), capture::webcam::frame_size());
    for f in formats {
        capture::webcam::keep_open(Some((cam.id.clone(), Some(f))));
        std::thread::sleep(std::time::Duration::from_millis(2500));
        println!("{:<24} status {:?}  frames {:?}", f.label(), capture::webcam::status(), capture::webcam::frame_size());
    }
    capture::webcam::keep_open(None);
}
