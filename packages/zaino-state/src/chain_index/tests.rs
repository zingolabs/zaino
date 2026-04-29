//! Zaino-State ChainIndex unit tests.

pub(crate) mod finalised_state;
pub(crate) mod mempool;
mod mockchain_tests;
mod poll;
mod proptest_blockgen;
mod sync_loop;
pub(crate) mod types;
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
        source::mockchain_source::MockchainSource,
        tests::vectors::{
            build_active_mockchain_source, build_mockchain_source, load_test_vectors,
        },
        NodeBackedChainIndex, NodeBackedChainIndexSubscriber, SyncTimings,
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
    // The 2 s poll interval here is load-bearing for other tests: most
    // callers (mockchain_tests, mempool, poll, proptest_blockgen) drop the
    // indexer without calling `shutdown()`, relying on the background sync
    // loop being in its post-success `interval` sleep at teardown to avoid
    // racing with runtime shutdown. Shorter polling lets the test body
    // return before that settle point and exposes the latent race. Tests
    // that need faster setup should use
    // `load_test_vectors_and_sync_chain_index_with_timings` and handle
    // their own teardown.
    load_with_settings(
        active_mockchain_source,
        SyncTimings::default(),
        Duration::from_secs(2),
    )
    .await
}

async fn load_test_vectors_and_sync_chain_index_with_timings(
    active_mockchain_source: bool,
    sync_timings: SyncTimings,
) -> (
    Vec<vectors::TestVectorBlockData>,
    NodeBackedChainIndex<MockchainSource>,
    NodeBackedChainIndexSubscriber<MockchainSource>,
    MockchainSource,
) {
    load_with_settings(
        active_mockchain_source,
        sync_timings,
        Duration::from_millis(25),
    )
    .await
}

async fn load_with_settings(
    active_mockchain_source: bool,
    sync_timings: SyncTimings,
    setup_poll_interval: Duration,
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

    let indexer = NodeBackedChainIndex::new_with_sync_timings(source.clone(), config, sync_timings)
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
        tokio::time::sleep(setup_poll_interval).await;
    }

    (blocks, indexer, index_reader, source)
}
