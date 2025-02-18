//! Zingo-Indexer daemon

use clap::{CommandFactory, Parser};
use std::path::PathBuf;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use zainodlib::{config::load_config, error::IndexerError, indexer::Indexer};

#[derive(Parser, Debug)]
#[command(name = "Zaino", about = "The Zcash Indexing Service", version)]
struct Args {
    /// Path to the configuration file
    #[arg(short, long, value_name = "FILE")]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_timer(tracing_subscriber::fmt::time::UtcTime::rfc_3339())
        .with_target(true)
        .init();

    let args = Args::parse();

    if std::env::args().any(|arg| arg == "--version" || arg == "-V") {
        let cmd = Args::command();
        println!("{}", cmd.get_version().unwrap());
        return;
    }

    let config_path = args
        .config
        .unwrap_or_else(|| PathBuf::from("./zainod/zindexer.toml"));

    loop {
        match Indexer::start(load_config(&config_path).unwrap()).await {
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
