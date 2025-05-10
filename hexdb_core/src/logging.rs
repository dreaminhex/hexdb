// HexDB Core Logging
// This module provides a logging utility for the HexDB engine, using the
// `tracing` and `tracing-subscriber` crates. It initializes a subscriber that
// formats log messages with timestamps, thread information, and log levels.

use tracing_subscriber::{fmt, EnvFilter};

pub fn init_logging(service_name: &str) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));

    fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_level(true)
        .with_line_number(true)
        .with_thread_ids(true)
        .with_thread_names(true)
        .with_timer(fmt::time::UtcTime::rfc_3339())
        .with_max_level(tracing::Level::TRACE)
        .with_writer(std::io::stdout)
        .with_ansi(atty::is(atty::Stream::Stdout))
        .pretty()
        .init();

    tracing::info!(service = %service_name, "🗎 Logging initialized.");
}
