//! End-to-end: the store serves a persisted header's hash and timestamp through
//! the [`HeaderRead`] tier read.
//!
//! Blocks are synced through the real engine, then each height's header is read
//! back. Every block carries a distinct timestamp, so a header read that dropped
//! or defaulted the time would fail the assertion. A height above the watermark
//! is a domain miss (`Ok(None)`), not an error.

use std::sync::Arc;

use zaino_component::{ComponentName, Lifecycle, ReachabilityProbe};
use zaino_indexer::{FetchConcurrency, SourceSyncDriver, SyncTarget, SyncTuning};
use zaino_indexes::index_set::IndexSet;
use zaino_indexes::sets::current_zaino::{context_from_block, CurrentZaino};
use zaino_persistence::in_memory::InMemoryBackend;
use zaino_primitives::types::{Block, BlockHash, Height};
use zaino_runtime::{OrchestraBuilder, RunComponent, ValidatorComponent};
use zaino_service::{HeaderRead, HeaderSummary, TakeSnapshot};
use zaino_source::mock::{test_block, MockChain};
use zaino_source::{RetryPolicy, ValidatorClient};
use zaino_store::StoreReader;
use zaino_store_service::StoreComponent;

struct Probe(bool);
impl ReachabilityProbe for Probe {
    async fn reachable(&self) -> bool {
        self.0
    }
}

/// A mock block at `height` (hash `[hash_byte; 32]`) carrying a distinct
/// timestamp, so a header read is pinned to the persisted value, not a default.
fn timed_block(height: u32, hash_byte: u8, time: u32) -> Block {
    let mut block = test_block(height, hash_byte);
    block.header.time = time;
    block
}

#[tokio::test]
async fn store_serves_a_persisted_header() {
    let backend = InMemoryBackend::new();

    let chain = MockChain::new()
        .with_block(timed_block(0, 1, 1_000))
        .with_block(timed_block(1, 2, 2_222))
        .with_block(timed_block(2, 3, 3_333));
    let source = Arc::new(ValidatorClient::new(chain, RetryPolicy::default()));

    let driver = SourceSyncDriver::resuming(
        &backend,
        CurrentZaino::pipelines(),
        source,
        |block| context_from_block(&block),
        SyncTuning {
            batch_size: 8,
            channel_capacity: 16,
            concurrency: FetchConcurrency::SERIAL,
        },
        SyncTarget::Depth { depth: 0 },
    )
    .expect("driver builds");
    let indexer = RunComponent::new(ComponentName("indexer"), driver);
    let store = StoreComponent::new(
        ComponentName("store"),
        StoreReader::<_, CurrentZaino>::new(Arc::new(backend.clone())),
    );
    let validator = ValidatorComponent::connect(&Probe(true))
        .await
        .expect("validator reachable");

    let orchestra = OrchestraBuilder::new()
        .boot_observed(validator)
        .await
        .boot(indexer)
        .await
        .expect("indexer boots")
        .boot(store.clone())
        .await
        .expect("store boots")
        .build();
    for status in orchestra.statuses() {
        assert_eq!(status.lifecycle, Lifecycle::Ready, "{}", status.name);
    }

    let snapshot = store.reader().snapshot().await.expect("snapshot");

    // Each persisted header reads back with its own hash and distinct time.
    for (height, hash_byte, time) in [(0u32, 1u8, 1_000u32), (1, 2, 2_222), (2, 3, 3_333)] {
        let summary = snapshot
            .header(Height::try_from(height).expect("valid height"))
            .await
            .expect("read succeeds")
            .expect("height is finalised");
        assert_eq!(
            summary,
            HeaderSummary {
                hash: BlockHash::from([hash_byte; 32]),
                time,
            },
            "header at height {height}",
        );
    }

    // Above the finalised watermark there is no block: a domain miss, not a
    // failure.
    let above = snapshot
        .header(Height::try_from(99).expect("valid height"))
        .await
        .expect("read succeeds");
    assert_eq!(above, None, "above the watermark is Ok(None)");
}
