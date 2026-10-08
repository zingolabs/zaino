//! A watermark stamped ahead of the headers index is corrected to the highest
//! header held, once, before anything reads it.
//!
//! Indexes three blocks through the real driver, then re-stamps the watermark
//! far above them — the shape a stamp-arithmetic fault leaves behind — and
//! checks the store finds the real top and re-stamps there.

use std::sync::Arc;

use zaino_component::{ComponentName, Lifecycle, ReachabilityProbe};
use zaino_indexer::{FetchConcurrency, SourceSyncDriver, SyncTuning};
use zaino_indexes::index_set::IndexSet;
use zaino_indexes::indexes::headers::{self, HeadersIndex};
use zaino_indexes::sets::current_zaino::{context_from_block, CurrentZaino};
use zaino_persistence::in_memory::InMemoryBackend;
use zaino_persistence::{Backend, BackendWriter, WriteOp};
use zaino_persistence_codec::{encode_key, watermark};
use zaino_primitives::types::Height;
use zaino_runtime::{OrchestraBuilder, RunComponent, ValidatorComponent};
use zaino_source::mock::{test_block, MockChain};
use zaino_source::{RetryPolicy, ValidatorClient};
use zaino_store::StoreReader;
use zaino_store_service::StoreComponent;
use zaino_sync::primitives::BlockHeight;

struct Probe(bool);
impl ReachabilityProbe for Probe {
    async fn reachable(&self) -> bool {
        self.0
    }
}

fn height(h: u32) -> Height {
    Height::try_from(h).expect("valid height")
}

/// Index blocks 0..=2 into `backend` through the real driver and return the
/// store over it.
async fn indexed_store(backend: &InMemoryBackend) -> StoreComponent<InMemoryBackend, CurrentZaino> {
    let chain = MockChain::new()
        .with_block(test_block(0, 1))
        .with_block(test_block(1, 2))
        .with_block(test_block(2, 3));
    let source = Arc::new(ValidatorClient::new(chain, RetryPolicy::default()));
    let driver = SourceSyncDriver::resuming(
        backend,
        CurrentZaino::pipelines(),
        source,
        |block| context_from_block(&block),
        SyncTuning {
            batch_size: 8,
            finalised_depth: 0,
            channel_capacity: 16,
            concurrency: FetchConcurrency::SERIAL,
        },
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
    store
}

#[tokio::test]
async fn a_consistent_watermark_is_left_alone() {
    let backend = InMemoryBackend::new();
    let store = indexed_store(&backend).await;

    assert_eq!(
        store.reader().repair_watermark().expect("check runs"),
        None,
        "a watermark at the highest header needs no repair"
    );
    assert_eq!(
        watermark::read(&backend.reader().expect("reader")).expect("read"),
        Some(height(2))
    );
}

#[tokio::test]
async fn a_watermark_ahead_of_the_index_is_re_stamped_at_the_highest_header() {
    let backend = InMemoryBackend::new();
    let store = indexed_store(&backend).await;

    // The fault shape: a stamp a full batch beyond anything indexed.
    backend
        .writer()
        .expect("writer")
        .commit(vec![watermark::stamp(height(1_002))])
        .expect("stamp");

    let repair = store
        .reader()
        .repair_watermark()
        .expect("repair runs")
        .expect("the stamp was ahead of the index");
    assert_eq!(repair.claimed, height(1_002));
    assert_eq!(repair.corrected, height(2));
    assert!(
        repair.trimmed.is_empty(),
        "nothing was held above the corrected tip, so nothing is trimmed: {:?}",
        repair.trimmed
    );
    assert_eq!(
        watermark::read(&backend.reader().expect("reader")).expect("read"),
        Some(height(2)),
        "the stamp now names the highest header held"
    );
    // Idempotent: a second check finds nothing to do.
    assert_eq!(store.reader().repair_watermark().expect("check runs"), None);
}

#[tokio::test]
async fn a_hole_below_a_held_watermark_moves_the_stamp_beneath_it() {
    let backend = InMemoryBackend::new();
    let store = indexed_store(&backend).await;

    // The other fault shape: a range that failed to index (height 1 missing)
    // behind a later one that succeeded (height 2 present, stamp at 2).
    backend
        .writer()
        .expect("writer")
        .commit(vec![WriteOp::Delete {
            namespace: headers::ID.into(),
            key: encode_key::<HeadersIndex>(&BlockHeight::new(1)),
        }])
        .expect("delete");

    let repair = store
        .reader()
        .repair_watermark()
        .expect("repair runs")
        .expect("the hole moves the stamp beneath it");
    assert_eq!(repair.claimed, height(2));
    assert_eq!(
        repair.corrected,
        height(0),
        "the stamp moves below the lowest hole so the indexer re-covers it"
    );
    assert!(
        !repair.trimmed.is_empty(),
        "the walk-ordered heights above the corrected tip are trimmed so resume can re-append them"
    );
    assert_eq!(
        watermark::read(&backend.reader().expect("reader")).expect("read"),
        Some(height(0))
    );
}
