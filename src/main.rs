fn main() {
    if let Err(error) = rustprofile::run() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}
