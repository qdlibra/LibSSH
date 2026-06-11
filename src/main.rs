#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod cli;
mod config;
mod history;
mod i18n;
mod proxy;
mod sftp;
mod ssh;
mod ssh_config;
mod updater;
mod system;
mod logbuf;

use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;

fn main() -> anyhow::Result<()> {
    // Keep IME enabled. Later milestones route composition through the hidden
    // Slint input in terminal_view.slint so Chinese input remains available.
    //
    // 两层订阅：① stderr 照旧（env-filter 控制，默认 info）；② 应用内缓冲，仅收
    // WARN/ERROR，供「运行日志」浮层查看 —— release 版无控制台时这是唯一途径。
    let log_buffer = logbuf::new_buffer();
    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")));
    let buffer_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(logbuf::BufferMakeWriter::new(log_buffer.clone()))
        .with_filter(LevelFilter::WARN);
    tracing_subscriber::registry()
        .with(stderr_layer)
        .with(buffer_layer)
        .init();

    let args: Vec<String> = std::env::args().collect();
    if cli::handles_args(&args) {
        return cli::run_args(&args);
    }

    app::run(log_buffer)
}
