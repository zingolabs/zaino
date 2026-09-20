//! The orchestration runtime drives a **source-backed** indexer.
//!
//! End to end over a real source seam: a `MockChain` (implementing dev's
//! `zaino-source` capability traits) feeds a `SourceProvisioner`, which streams
//! into a real `SyncEngine` via `sync_channel`, driven by a `SourceSyncDriver`
//! and supervised as an `IndexerComponent`. Swap `MockChain` for a zebra adapter
//! and the same driver indexes a real chain — the provisioner is generic over
//! the source.

use std::sync::Arc;

use zaino_component::{ComponentName, Lifecycle, Managed, StatusSource, StatusWatch};
use zaino_indexer::{FetchConcurrency, FullBlocks, SourceProvisioner, SourceSyncDriver};
use zaino_primitives::types::{Block, Height};
use zaino_runtime::IndexerComponent;
use zaino_source::mock::{test_block, MockChain};
use zaino_source::{RetryPolicy, ValidatorClient};
use zaino_sync::engine::{EngineConfig, SyncEngine};
use zaino_sync::primitives::BlockHeight;
use zaino_sync::testing::{toy_index_set, InMemoryBackend, TestBlockContext};

/// Project a fetched block into the toy set's context (height only).
fn to_context(block: Block) -> TestBlockContext {
    TestBlockContext {
        height: u64::from(block.header.height),
        value: u32::from(block.header.height),
    }
}

#[tokio::test]
async fn the_runtime_indexes_from_a_source() {
    // A mock validator with blocks 0..=7 (last is the tip).
    let mut chain = MockChain::new();
    for h in 0u32..=7 {
        chain = chain.with_block(test_block(h, u8::try_from(h).expect("small height")));
    }

    let backend = InMemoryBackend::new();
    let engine = SyncEngine::from_index_set(
        toy_index_set(),
        backend.clone(),
        EngineConfig {
            batch_size: 4,
            start_height: BlockHeight::new(0),
        },
    )
    .expect("valid index set");

    // Wrap the raw mock adapter in the resilient decorator — the provisioner
    // binds the resilient ports (GetBlock/GetChainTip), so retry/backoff and
    // SourceError::Unavailable come from ValidatorClient, not from the consumer.
    let source = ValidatorClient::new(chain, RetryPolicy::default());
    let provisioner = Arc::new(SourceProvisioner::<_, _, _, FullBlocks>::new(
        Arc::new(source),
        to_context,
        FetchConcurrency::SERIAL,
    ));
    let driver = SourceSyncDriver::new(
        engine,
        provisioner,
        Height::try_from(0).expect("valid height"),
        0, // finalised_depth: non-reorging mock, index right to the tip
        16,
        backend,
    );
    let indexer = IndexerComponent::new(ComponentName("indexer"), driver);

    indexer.spawn().await.expect("spawn");

    // It fetches from the source, indexes to the tip, and reaches Ready.
    let mut status = indexer.subscribe();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if status.borrow_and_update().lifecycle == Lifecycle::Ready {
                return;
            }
            status.changed().await.expect("status stream open");
        }
    })
    .await
    .expect("indexer reached Ready from the source");

    indexer.stop().await.expect("stop");
    assert_eq!(indexer.status().lifecycle, Lifecycle::Offline);
}
