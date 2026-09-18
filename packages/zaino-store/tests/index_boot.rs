//! EXPLORATORY: end-to-end indexing boot.
//!
//! A **real** sync engine (over the CurrentZaino index set), driven by the
//! runtime's `IndexerComponent`, indexes a mock chain into a KV backend that the
//! `StoreComponent` reads behind — all booted by the Orchestra in dependency
//! order. Proves the write path boots and *actually indexes* (not the no-op
//! driver of `bringup.rs`): after boot, the shared backend holds the finalised
//! range up to the mock tip.
//!
//! It also asserts the store *consumes* the watermark: a snapshot reports the
//! real indexed tip (composed on read from the headers index) and a serviceable
//! range bounded by it. Address reads remain stubbed.

use std::sync::Arc;

use zaino_component::{ComponentName, Lifecycle, ReachabilityProbe};
use zaino_core::Height;
use zaino_indexer::{SourceProvisioner, SourceSyncDriver};
use zaino_indexes::sets::current_zaino::{context_from_block, index_set, CurrentZainoContext};
use zaino_persistence::in_memory::InMemoryBackend;
use zaino_runtime::{IndexerComponent, OrchestraBuilder, ValidatorComponent};
use zaino_service::{Snapshot, TakeSnapshot};
use zaino_source::mock::{test_block, MockChain};
use zaino_source::{RetryPolicy, ValidatorClient};
use zaino_store::{StoreComponent, StoreReader};
use zaino_sync::engine::{EngineConfig, SyncEngine};
use zaino_sync::primitives::BlockHeight;

/// A validator that is reachable.
struct Probe(bool);
impl ReachabilityProbe for Probe {
    async fn reachable(&self) -> bool {
        self.0
    }
}

#[tokio::test]
async fn runtime_boots_and_indexes_a_mock_chain() {
    // One KV backend, shared by clone (Arc interior): the engine writes, the
    // store reads the same bytes.
    let backend = InMemoryBackend::new();

    // A mock validator with blocks 0..=2, behind the resilient client.
    let chain = MockChain::new()
        .with_block(test_block(0, 1))
        .with_block(test_block(1, 2))
        .with_block(test_block(2, 3));
    let source = Arc::new(ValidatorClient::new(chain, RetryPolicy::default()));

    // The real sync engine over the CurrentZaino index set + the shared backend.
    let engine = SyncEngine::from_index_set(
        index_set(),
        backend.clone(),
        EngineConfig {
            batch_size: 8,
            start_height: BlockHeight::new(0),
        },
    )
    .expect("engine builds");
    let provisioner = Arc::new(SourceProvisioner::new(source, |block| {
        context_from_block(&block)
    }));
    // finalised_depth = 0: a non-reorging mock, so the finalised boundary is the
    // tip and the whole chain is safe to index.
    let driver = SourceSyncDriver::new(engine, provisioner, Height::GENESIS, 0, 16);
    let indexer = IndexerComponent::new(ComponentName("indexer"), driver);

    // The store reads behind the same backend.
    let store = StoreComponent::new(
        ComponentName("store"),
        StoreReader::new(Arc::new(backend.clone())),
    );

    let validator = ValidatorComponent::connect(&Probe(true))
        .await
        .expect("validator reachable");

    // validator (observed) → indexer (writer; reaches Ready only once caught up
    // to the tip) → store (reader).
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
        assert_eq!(
            status.lifecycle,
            Lifecycle::Ready,
            "{} should be Ready after bringup",
            status.name
        );
    }

    // The engine committed the finalised range into the shared backend — proof
    // that indexing actually ran end to end, not just that components booted.
    let committed = SyncEngine::<CurrentZainoContext, InMemoryBackend>::committed_height(&backend)
        .expect("committed height readable");
    assert_eq!(
        committed,
        Some(BlockHeight::new(2)),
        "indexed the finalised range up to the mock tip"
    );

    // The store *consumes* that watermark and composes the tip on read: a
    // snapshot reports a real BlockId (height + hash) read from the headers
    // index, and a serviceable range bounded by the finalised tip.
    let snapshot = store.reader().snapshot().await.expect("snapshot");
    let tip = snapshot.pinned_tip().expect("a pinned tip after indexing");
    assert_eq!(
        u32::from(tip.height),
        2,
        "store's pinned tip is the indexed tip"
    );
    assert_eq!(
        u32::from(snapshot.serviceable_range().finalized_tip),
        2,
        "store is serviceable up to the finalised tip"
    );
}
