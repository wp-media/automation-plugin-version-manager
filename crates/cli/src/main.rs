//! APVM CLI - Automation Plugin Version Manager

use apvm_core::{Config, Result};

fn main() -> Result<()> {
    // Only enable tracing if RUST_LOG is set (for debugging)
    if std::env::var("RUST_LOG").is_ok() {
        tracing_subscriber::fmt::init();
    }

    let config = Config::load()?;

    // User-facing output: simple println
    println!("APVM initialized");

    // Debug output: only shown with RUST_LOG=debug
    tracing::debug!("Cache dir: {:?}", config.cache_dir);

    Ok(())
}