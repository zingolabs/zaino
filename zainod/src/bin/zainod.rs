//! Zingo-Indexer daemon

use clap::Parser;
use std::path::PathBuf;
use zainodlib::{config::load_config, indexer::Indexer};

#[derive(Parser, Debug)]
#[command(name = "zindexer", about = "A server for Zingo-Indexer")]
struct Args {
    /// Path to the configuration file
    #[arg(short, long, value_name = "FILE")]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() {
    Indexer::start(load_config(
        &Args::parse()
            .config
            .unwrap_or_else(|| PathBuf::from("./zainod/zindexer.toml")),
    ))
    .await
    .unwrap();
}
