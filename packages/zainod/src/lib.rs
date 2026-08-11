//! Zaino Indexer service.

#![warn(missing_docs)]
#![forbid(unsafe_code)]

use std::path::PathBuf;

use crate::config::load_config;
use crate::error::IndexerError;
use crate::indexer::start_indexer;
use tracing::{error, info};

pub mod cli;
pub mod config;
pub mod error;
pub mod indexer;
#[cfg(feature = "prometheus")]
pub mod metrics;
#[cfg(feature = "profile")]
pub mod profile;

/// Run the Zaino indexer.
///
/// Runs the main indexer loop with restart support.
/// Logging should be initialized by the caller before calling this function.
/// Returns an error if config loading or indexer startup fails.
pub async fn run(config_path: PathBuf) -> Result<(), IndexerError> {
    zaino_common::logging::try_init();

    info!(version = env!("CARGO_PKG_VERSION"), "zainod started");
    let config = load_config(&config_path)?;

    #[cfg(feature = "prometheus")]
    if let Some(endpoint) = config.metrics_endpoint {
        crate::metrics::init(endpoint)?;
    }

    // Hold the profiler for the whole process; the report is built from the
    // graceful-shutdown path below, after the serve loop returns.
    #[cfg(feature = "profile")]
    let profiler = crate::profile::start_profiler();

    let result = run_indexer_loop(config).await;

    #[cfg(feature = "profile")]
    if let Some(guard) = profiler {
        crate::profile::write_profile(guard);
    }

    result
}

/// The restart-aware indexer loop: spawn the indexer, await its serve task, and
/// restart on [`IndexerError::Restart`]. Returns `Ok(())` on a graceful
/// shutdown (SIGTERM/ctrl-c or an internal `Closing` transition).
async fn run_indexer_loop(config: config::ZainodConfig) -> Result<(), IndexerError> {
    loop {
        match start_indexer(config.clone()).await {
            Ok(joinhandle_result) => {
                info!("Zaino Indexer started successfully.");
                match joinhandle_result.await {
                    Ok(indexer_result) => match indexer_result {
                        Ok(()) => {
                            info!("Exiting Zaino successfully.");
                            return Ok(());
                        }
                        Err(IndexerError::Restart) => {
                            error!("Zaino encountered critical error, restarting.");
                            continue;
                        }
                        Err(e) => {
                            error!(%e, "exiting Zaino with error");
                            return Err(e);
                        }
                    },
                    Err(e) => {
                        error!(%e, "Zaino exited early with error");
                        return Err(e.into());
                    }
                }
            }
            Err(e) => {
                error!(%e, "Zaino failed to start");
                return Err(e);
            }
        }
    }
}
