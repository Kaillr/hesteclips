//! `HESTEFILES_TOKEN=… [HESTEFILES_PUBLIC=1] cargo run -p hestefiles --example upload -- <file> [base_id] [path] [name]`
use std::sync::atomic::AtomicBool;

fn main() {
    let token = std::env::var("HESTEFILES_TOKEN").expect("set HESTEFILES_TOKEN");
    let client = hestefiles::Client::new(hestefiles::DEFAULT_DOMAIN, &token);
    let mut args = std::env::args().skip(1);
    let file = std::path::PathBuf::from(args.next().expect("file to upload"));
    let base = args.next().unwrap_or_else(|| "root".into());
    let path = args.next().unwrap_or_default();
    let name = args.next().unwrap_or_else(|| file.file_name().unwrap().to_string_lossy().into_owned());
    let public = std::env::var_os("HESTEFILES_PUBLIC").is_some();
    let t = std::time::Instant::now();
    let result = client.upload(&file, &base, &path, &name, public, &AtomicBool::new(false), |p| eprintln!("{p:?}"));
    println!("{result:?} in {:.1}s", t.elapsed().as_secs_f64());
}
