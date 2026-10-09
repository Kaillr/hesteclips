//! The app in front, as game detection sees it, every half second (`SECS=n`).
//! `cargo run -p capture --example focus`
fn main() {
    let secs: u64 = std::env::var("SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(5);
    for _ in 0..secs * 2 {
        match capture::foreground_app_path() {
            Some((id, path)) => {
                let steam = capture::stores::steam_game(&path).map(|g| g.name).or_else(|| capture::stores::steam_dir(&path).map(|(_, d)| d));
                println!("front: {id} at {} (Steam game: {})", path.display(), steam.as_deref().unwrap_or("-"));
            }
            None => println!("front: nothing known"),
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}
