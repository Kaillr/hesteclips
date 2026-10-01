fn main() {
    let d = capture::macos::list_shareable();
    println!("screens: {}", d.screens.len());
    for s in &d.screens { println!("  [{}] {}", s.id, s.name); }
    println!("apps: {}", d.apps.len());
    for a in d.apps.iter().take(8) { println!("  {} — {}", a.id, a.name); }
}
