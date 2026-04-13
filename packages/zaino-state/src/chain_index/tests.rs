//! Zaino-State ChainIndex unit tests.

pub(crate) mod finalised_state;
pub(crate) mod mempool;
mod mockchain_tests;
mod proptest_blockgen;
mod sync_loop;
pub(crate) mod vectors;

pub(crate) fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_timer(tracing_subscriber::fmt::time::UtcTime::rfc_3339())
        .with_target(true)
        .try_init()
        .unwrap();
}

use std::path::PathBuf;
use tempfile::TempDir;
use tokio::time::Duration;
use zaino_common::{network::ActivationHeights, DatabaseConfig, Network, StorageConfig};

use crate::{
    chain_index::{
        source::test::MockchainSource,
        tests::vectors::{
            build_active_mockchain_source, build_mockchain_source, load_test_vectors,
        },
        NodeBackedChainIndex, NodeBackedChainIndexSubscriber,
    },
    BlockCacheConfig,
};

async fn load_test_vectors_and_sync_chain_index(
    active_mockchain_source: bool,
) -> (
    Vec<vectors::TestVectorBlockData>,
    NodeBackedChainIndex<MockchainSource>,
    NodeBackedChainIndexSubscriber<MockchainSource>,
    MockchainSource,
) {
    init_tracing();

    let blocks = load_test_vectors().unwrap().blocks;

    let source = if active_mockchain_source {
        build_active_mockchain_source(150, blocks.clone())
    } else {
        build_mockchain_source(blocks.clone())
    };

    let temp_dir: TempDir = tempfile::tempdir().unwrap();
    let db_path: PathBuf = temp_dir.path().to_path_buf();

    let config = BlockCacheConfig {
        storage: StorageConfig {
            database: DatabaseConfig {
                path: db_path,
                ..Default::default()
            },
            ..Default::default()
        },
        db_version: 1,
        network: Network::Regtest(ActivationHeights::default()),
    };

    let indexer = NodeBackedChainIndex::new(source.clone(), config)
        .await
        .unwrap();
    let index_reader = indexer.subscriber();

    loop {
        let check_height: u32 = match active_mockchain_source {
            true => source.active_height() - 100,
            false => 100,
        };
        if index_reader.finalized_state.db_height().await.unwrap()
            == Some(crate::Height(check_height))
        {
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    (blocks, indexer, index_reader, source)
}
