//! EXPLORATORY: resume from the watermark — fresh vs mid-sync.
//!
//! A fresh backend indexes from genesis; a restart over a backend that already
//! holds a watermark resumes just after it, rather than re-indexing from zero.
//! [`assess_start`] formalises the distinction, and the driver's start is derived
//! from it.
//!
//! The two phases stop the first run early with `finalised_depth` (boundary =
//! tip − depth) so there is a genuine mid-sync backend to resume from.

use std::sync::Arc;

use zaino_component::ComponentName;
use zaino_core::Height;
use zaino_indexer::{assess_start, FetchConcurrency, SourceSyncDriver, SyncStart, SyncTuning};
use zaino_indexes::sets::current_zaino::{context_from_block, index_set, CurrentZainoContext};
use zaino_persistence::in_memory::InMemoryBackend;
use zaino_runtime::{IndexerComponent, Orchestra, OrchestraBuilder};
use zaino_source::mock::{test_block, MockChain};
use zaino_source::{RetryPolicy, ValidatorClient};
use zaino_sync::engine::SyncEngine;
use zaino_sync::primitives::BlockHeight;

type Source = ValidatorClient<MockChain>;

/// Build and boot a **resume-safe** indexer that syncs `source` into `backend`,
/// stopping at `tip − finalised_depth`. The start is derived from the backend's
/// watermark by `resuming` — the caller never passes it, which is the point.
/// Returns the running Orchestra (kept alive by the caller).
async fn run_indexer(
    backend: &InMemoryBackend,
    source: Arc<Source>,
    finalised_depth: u32,
) -> Orchestra {
    let driver = SourceSyncDriver::resuming(
        backend,
        index_set(),
        source,
        |block| context_from_block(&block),
        SyncTuning {
            batch_size: 8,
            finalised_depth,
            channel_capacity: 16,
            concurrency: FetchConcurrency::SERIAL,
        },
    )
    .expect("driver builds");
    let indexer = IndexerComponent::new(ComponentName("indexer"), driver);
    OrchestraBuilder::new()
        .boot(indexer)
        .await
        .expect("indexer boots")
        .build()
}

fn committed(backend: &InMemoryBackend) -> Option<BlockHeight> {
    SyncEngine::<CurrentZainoContext, InMemoryBackend>::committed_height(backend)
        .expect("committed height readable")
}

#[tokio::test]
async fn a_restart_resumes_from_the_watermark() {
    let backend = InMemoryBackend::new();
    let chain = MockChain::new()
        .with_block(test_block(0, 1))
        .with_block(test_block(1, 2))
        .with_block(test_block(2, 3))
        .with_block(test_block(3, 4))
        .with_block(test_block(4, 5));
    let source = Arc::new(ValidatorClient::new(chain, RetryPolicy::default()));

    let height = |h: u32| Height::try_from(h).expect("valid test height");

    // Fresh backend: assess says start from genesis.
    assert_eq!(assess_start(&backend).expect("assess"), SyncStart::Fresh);
    assert_eq!(
        assess_start(&backend).expect("assess").next_height(),
        Height::GENESIS
    );

    // Phase 1 — a partial sync (resuming auto-starts at genesis on a fresh
    // backend): tip is 4, depth 2 ⇒ finalised boundary 2.
    let _run1 = run_indexer(&backend, Arc::clone(&source), 2).await;
    assert_eq!(committed(&backend), Some(BlockHeight::new(2)));

    // The backend is now mid-sync: assess resumes just after height 2.
    assert_eq!(
        assess_start(&backend).expect("assess"),
        SyncStart::Resume(height(2))
    );
    assert_eq!(
        assess_start(&backend).expect("assess").next_height(),
        height(3),
        "resume just after the committed tip"
    );

    // Phase 2 — restart: `resuming` reads the watermark and starts at 3 on its
    // own. depth 0 ⇒ boundary 4.
    let _run2 = run_indexer(&backend, Arc::clone(&source), 0).await;
    assert_eq!(
        committed(&backend),
        Some(BlockHeight::new(4)),
        "resumed and extended to the tip, not re-indexed from genesis"
    );
}
