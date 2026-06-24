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

/// Run the Zaino indexer.
///
/// Runs the main indexer loop with restart support.
/// Logging should be initialized by the caller before calling this function.
/// Returns an error if config loading or indexer startup fails.
pub async fn run(config_path: PathBuf) -> Result<(), IndexerError> {
    zaino_common::logging::try_init();

    info!("zainod v{}", env!("CARGO_PKG_VERSION"));
    let config = load_config(&config_path)?;

    #[cfg(feature = "prometheus")]
    if let Some(endpoint) = config.metrics_endpoint {
        crate::metrics::init(endpoint)?;
    }

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
                            error!("Exiting Zaino with error: {}", e);
                            return Err(e);
                        }
                    },
                    Err(e) => {
                        error!("Zaino exited early with error: {}", e);
                        return Err(e.into());
                    }
                }
            }
            Err(e) => {
                error!("Zaino failed to start with error: {}", e);
                return Err(e);
            }
        }
    }
}
