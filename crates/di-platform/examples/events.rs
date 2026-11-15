//! Print raw device events for a few seconds: `cargo run -p di-platform --example events [secs]`
fn main() -> anyhow::Result<()> {
    let secs: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(5);
    let rx = di_platform::watch()?;
    let end = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    while let Some(left) = end.checked_duration_since(std::time::Instant::now()) {
        match rx.recv_timeout(left) {
            Ok(ev) => println!("{ev:?}"),
            Err(_) => break,
        }
    }
    Ok(())
}
