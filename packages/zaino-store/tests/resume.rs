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
use zaino_indexer::{assess_start, SourceProvisioner, SourceSyncDriver, SyncStart};
use zaino_indexes::sets::current_zaino::{context_from_block, index_set, CurrentZainoContext};
use zaino_persistence::in_memory::InMemoryBackend;
use zaino_runtime::{IndexerComponent, Orchestra, OrchestraBuilder};
use zaino_source::mock::{test_block, MockChain};
use zaino_source::{RetryPolicy, ValidatorClient};
use zaino_sync::engine::{EngineConfig, SyncEngine};
use zaino_sync::primitives::BlockHeight;

type Source = ValidatorClient<MockChain>;

/// Build and boot an indexer that syncs `source` into `backend`, starting at
/// height `start` and stopping at `tip − finalised_depth`. Returns the running
/// Orchestra (kept alive by the caller).
async fn run_indexer(
    backend: &InMemoryBackend,
    source: Arc<Source>,
    start: Height,
    finalised_depth: u32,
) -> Orchestra {
    let engine = SyncEngine::from_index_set(
        index_set(),
        backend.clone(),
        EngineConfig {
            batch_size: 8,
            start_height: BlockHeight::new(u64::from(start)),
        },
    )
    .expect("engine builds");
    let provisioner = Arc::new(SourceProvisioner::new(source, |block| {
        context_from_block(&block)
    }));
    let driver = SourceSyncDriver::new(engine, provisioner, start, finalised_depth, 16);
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

    // Phase 1 — a partial sync: tip is 4, depth 2 ⇒ finalised boundary 2.
    let _run1 = run_indexer(&backend, Arc::clone(&source), Height::GENESIS, 2).await;
    assert_eq!(committed(&backend), Some(BlockHeight::new(2)));

    // The backend is now mid-sync: assess resumes just after height 2.
    assert_eq!(
        assess_start(&backend).expect("assess"),
        SyncStart::Resume(height(2))
    );
    let resume = assess_start(&backend).expect("assess").next_height();
    assert_eq!(resume, height(3), "resume just after the committed tip");

    // Phase 2 — restart from the resume point: depth 0 ⇒ boundary 4.
    let _run2 = run_indexer(&backend, Arc::clone(&source), resume, 0).await;
    assert_eq!(
        committed(&backend),
        Some(BlockHeight::new(4)),
        "resumed and extended to the tip, not re-indexed from genesis"
    );
}
