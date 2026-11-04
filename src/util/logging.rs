use std::path::Path;

use anyhow::{Context, Result};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

/// Initialize tracing to both stdout and a single file in `log_dir`.
/// Returns a guard that must be kept alive for the process lifetime.
pub fn init_dual_logging(log_dir: &Path, log_file_name: &str) -> Result<WorkerGuard> {
    std::fs::create_dir_all(log_dir).context("Failed to create log directory")?;

    let file_appender = tracing_appender::rolling::never(log_dir, log_file_name);
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()),
        ))
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stdout))
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(non_blocking)
                .with_ansi(false),
        )
        .try_init()
        .context("Failed to initialize tracing subscriber")?;

    Ok(guard)
}
