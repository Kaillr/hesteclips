//! List what the capture backend can see: screens and apps to add as sources.
fn main() {
    let screens = capture::list_screens();
    println!("screens: {}", screens.len());
    for s in &screens {
        println!("  [{}] {}", s.id, s.name);
    }
    let windowed = capture::list_windowed_apps();
    println!("apps with a window: {}", windowed.len());
    for a in &windowed {
        println!("  {} — {}", a.id, a.name);
    }
    let apps = capture::list_apps();
    println!("apps: {}", apps.len());
    for a in &apps {
        println!("  {} — {}", a.id, a.name);
    }
}
