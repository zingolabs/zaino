//! Zaino indexer daemon library

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::sync::Arc;

use crate::config::load_config;
use crate::error::IndexerError;
use crate::indexer::start_indexer;
use crate::status::DaemonStatus;
use tracing::{error, info, Instrument as _};

mod admin;
mod bootstrap;
mod chainview;
pub mod cli;
mod config;
mod disk_monitor;
mod egress;
pub mod error;
mod fd_limit;
pub mod hold;
mod indexer;
pub mod logging;
mod metrics;
mod notify;
pub mod paths;
mod peers;
mod progress;
mod status;
mod stores;
mod utxos;
pub mod verify;

pub use crate::config::generate_config;

/// Until a shutdown signal (`Ok`) or the first failure (`Err`)
///
/// - no in-process restart: failure ends the process, the service manager restarts it, next boot
///   proves its state from disk (`zaino-persistence/usage.md` "The commit and crash contract")
/// - caller installs logging first ([`logging::init`]; none installed = every line dropped)
pub async fn run(config_path: PathBuf) -> Result<(), IndexerError> {
    daemon(config_path).instrument(crate::logging::component("Zainod")).await
}

async fn daemon(config_path: PathBuf) -> Result<(), IndexerError> {
    info!(version = env!("CARGO_PKG_VERSION"), "Starting");
    let config = load_config(&config_path)?;
    // Before any startup work (a bad `[snapshot]` must fail before it downloads)
    config.validate()?;
    let enabled = zaino_persistence::IndexKind::ALL.into_iter();
    let enabled = enabled.filter(|kind| config.enabled(*kind).is_some());
    let status = Arc::new(DaemonStatus::new(config.network, enabled));
    // Before the index-snapshot bootstrap (a held node reads held from its first answer)
    let (state_dir, goaway_delay) =
        (config.serve.state_dir.clone(), config.serve.hold_goaway_delay());
    crate::hold::spawn(Arc::clone(&status), state_dir, goaway_delay);
    config.warn_about_metrics_listener();
    // Before the index-snapshot bootstrap (its progress extends systemd's start timeout)
    crate::notify::spawn(Arc::clone(&status));

    if let Some(endpoint) = config.metrics.listen_address {
        let status = Arc::clone(&status);
        crate::logging::component("Metrics").in_scope(|| crate::metrics::init(endpoint, status))?;
    }
    // After the admin listener (`/readyz` reports the index-snapshot bootstrap inside)
    let running = start_indexer(config, status).await.inspect_err(|error| {
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
