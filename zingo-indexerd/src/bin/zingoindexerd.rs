//! Zingo-Indexer daemon

use zingoindexerlib::{config::IndexerConfig, indexer::Indexer};

#[tokio::main]
async fn main() {
    Indexer::start(IndexerConfig::default()).await.unwrap();
}
