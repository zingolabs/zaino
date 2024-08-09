//! Zingo-Indexer daemon

use zingoproxylib::{config::IndexerConfig, indexer::Indexer};

#[tokio::main]
async fn main() {
    Indexer::start(IndexerConfig::default()).await.unwrap();
}
