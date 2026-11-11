//! `HESTEFILES_TOKEN=… cargo run -p hestefiles --example browse [base_id] [path]`
fn main() {
    let token = std::env::var("HESTEFILES_TOKEN").expect("set HESTEFILES_TOKEN");
    let client = hestefiles::Client::new(hestefiles::DEFAULT_DOMAIN, &token);
    let mut args = std::env::args().skip(1);
    match args.next() {
        None => {
            println!("{:#?}", client.validate_token());
            println!("{:#?}", client.base_folders());
        }
        Some(base) => {
            let path = args.next().unwrap_or_default();
            println!("{:#?}", client.directory(&base, &path, hestefiles::Include::Folders).map(|d| d.folders));
        }
    }
}
