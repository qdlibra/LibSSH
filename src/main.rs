#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;

use tracing_subscriber::EnvFilter;

fn main() -> anyhow::Result<()> {
    // Keep IME enabled. Later milestones route composition through the hidden
    // Slint input in terminal_view.slint so Chinese input remains available.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    app::run()
}
