//! Zaino indexer daemon library

#![forbid(unsafe_code)]

use std::path::PathBuf;

use crate::config::load_config;
use crate::error::IndexerError;
use crate::indexer::start_indexer;
use tracing::{error, info, Instrument as _};

mod admin;
#[cfg(feature = "snapshot")]
mod bootstrap;
mod chainview;
pub mod cli;
mod config;
mod disk_monitor;
pub mod error;
mod fd_limit;
mod indexer;
pub mod logging;
mod metrics;
mod notify;
pub mod paths;
mod peers;
mod progress;
mod status;
mod stores;
pub mod verify;

/// Until a shutdown signal (`Ok`) or the first failure (`Err`)
///
/// - no in-process restart: failure ends the process, the service manager restarts it, next boot
///   proves its state from disk (`docs/design/durability.md` §6)
/// - logging initialised by the caller
pub async fn run(config_path: PathBuf) -> Result<(), IndexerError> {
    crate::logging::try_init()
        .map_err(|error| IndexerError::ConfigError(format!("logging: {error}")))?;
    daemon(config_path).instrument(crate::logging::component("Zainod")).await
}

async fn daemon(config_path: PathBuf) -> Result<(), IndexerError> {
    info!(version = env!("CARGO_PKG_VERSION"), "Starting");
    let config = load_config(&config_path)?;
    // Before any startup work (a bad `[snapshot]` must fail before it downloads)
    config.validate()?;
    // Before the snapshot bootstrap (`/statusz` lists the configured indexes from the start)
    crate::status::configure(
        zaino_nfs::INDEXES.into_iter().filter(|kind| config.enabled(*kind).is_some()),
    );
    config.warn_about_metrics_listener();
    // Before the snapshot bootstrap (its progress extends systemd's start timeout)
    crate::notify::spawn();

    if let Some(endpoint) = config.metrics.listen_address {
        crate::logging::component("Metrics").in_scope(|| crate::metrics::init(endpoint))?;
    }
    // After the admin listener (`/statusz` reports the bootstrap), before any index opens
    #[cfg(feature = "snapshot")]
    if let Some(snapshot) = &config.snapshot {
        crate::bootstrap::bootstrap(snapshot, &config)
            .instrument(crate::logging::component("Snapshot"))
            .await
            .inspect_err(|error| error!(%error, "Startup failed"))?;
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
