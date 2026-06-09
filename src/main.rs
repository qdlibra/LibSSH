#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod cli;
mod config;
mod i18n;
mod proxy;
mod sftp;
mod ssh;
mod ssh_config;
mod updater;
mod system;

use tracing_subscriber::EnvFilter;

fn main() -> anyhow::Result<()> {
    // Keep IME enabled. Later milestones route composition through the hidden
    // Slint input in terminal_view.slint so Chinese input remains available.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();
    if cli::handles_args(&args) {
        return cli::run_args(&args);
    }

    app::run()
}
