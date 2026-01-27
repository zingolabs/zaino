//! Zingo-Indexer daemon

use clap::Parser;
use std::{path::PathBuf, time::Duration};
use tracing::{error, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};
use tracing_throttle::{Policy, TracingRateLimitLayer};

use zainodlib::{config::load_config, error::IndexerError, indexer::start_indexer};

#[derive(Parser, Debug)]
#[command(name = "Zaino", about = "The Zcash Indexing Service", version)]
struct Args {
    /// Path to the configuration file
    #[arg(short, long, value_name = "FILE")]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() {
    // Set up rate limiting for repeated log messages (e.g., connection failures)
    // Uses exponential backoff: emits 1st, 2nd, 4th, 8th... occurrence
    let rate_limit = TracingRateLimitLayer::builder()
        .with_policy(Policy::exponential_backoff())
        .with_summary_interval(Duration::from_secs(60))
        .with_max_signatures(10_000)
        .build()
        .expect("failed to build rate limit layer");

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_timer(tracing_subscriber::fmt::time::UtcTime::rfc_3339())
                .with_target(true),
        )
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(rate_limit)
        .init();

    let args = Args::parse();

    let config_path = args
        .config
        .unwrap_or_else(|| PathBuf::from("./zainod/zindexer.toml"));

    loop {
        match start_indexer(load_config(&config_path).unwrap()).await {
            Ok(joinhandle_result) => {
                info!("Zaino Indexer started successfully.");
                match joinhandle_result.await {
                    Ok(indexer_result) => match indexer_result {
                        Ok(()) => {
                            info!("Exiting Zaino successfully.");
                            break;
                        }
                        Err(IndexerError::Restart) => {
                            error!("Zaino encountered critical error, restarting.");
                            continue;
                        }
                        Err(e) => {
                            error!("Exiting Zaino with error: {}", e);
                            break;
                        }
                    },
                    Err(e) => {
                        error!("Zaino exited early with error: {}", e);
                        break;
                    }
                }
            }
            Err(e) => {
                error!("Zaino failed to start with error: {}", e);
                break;
            }
        }
    }
}
