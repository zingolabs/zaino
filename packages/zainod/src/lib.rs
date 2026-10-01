//! Zaino Indexer service.

#![forbid(unsafe_code)]

use std::path::PathBuf;

use crate::config::load_config;
use crate::error::IndexerError;
use crate::indexer::start_indexer;
use tracing::{error, info, Instrument as _};

mod admin;
mod chainview;
pub mod cli;
pub mod config;
pub mod error;
mod fd_limit;
mod index_report;
pub mod indexer;
pub mod logging;
mod metrics;
pub mod paths;
mod status;
pub mod verify;

/// Runs the Zaino indexer until a shutdown signal (`Ok`) or the first failure (`Err`).
///
/// - no in-process restart: a failure ends the process, the service manager restarts it, and
///   the next boot proves its state from disk (`docs/design/durability.md` §6)
/// - logging initialised by the caller
pub async fn run(config_path: PathBuf) -> Result<(), IndexerError> {
    crate::logging::try_init()
        .map_err(|error| IndexerError::ConfigError(format!("logging: {error}")))?;
    daemon(config_path).instrument(crate::logging::component("Zainod")).await
}

async fn daemon(config_path: PathBuf) -> Result<(), IndexerError> {
    info!(version = env!("CARGO_PKG_VERSION"), "Starting");
    let config = load_config(&config_path)?;
    config.warn_about_metrics_endpoint();

    if let Some(endpoint) = config.metrics_endpoint {
        crate::logging::component("Metrics").in_scope(|| crate::metrics::init(endpoint))?;
    }

    let running = start_indexer(config).await.inspect_err(|error| {
        error!(%error, "Startup failed");
    })?;
    match running.await? {
        Ok(()) => {
            info!("Shutdown complete");
            Ok(())
        }
        Err(error) => {
            error!(%error, "Stopped with error");
            Err(error)
        }
    }
}
