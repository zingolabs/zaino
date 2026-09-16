//! The orchestration runtime drives the indexer sync.
//!
//! Proves the writer stack end to end at the lifecycle level: a real
//! `SyncEngine` (toy index set + in-memory backend), fed by the mock
//! provisioner, wrapped as a `SyncEngineDriver` (`SyncDriver`), booted and
//! supervised as an `IndexerComponent` by the runtime. This is what the
//! sync-bench crate hand-rolled as a driving loop — now it is the Orchestra's
//! job. (The source-backed provisioner over `zaino-source` is the next slice.)

use zaino_component::{ComponentName, Lifecycle, Managed, StatusSource, StatusWatch};
use zaino_indexer::SyncEngineDriver;
use zaino_runtime::IndexerComponent;
use zaino_sync::engine::{EngineConfig, SyncEngine};
use zaino_sync::primitives::BlockHeight;
use zaino_sync::testing::{toy_index_set, InMemoryBackend, MockProvisioner, TestBlockContext};

fn build_driver(
    target: u64,
) -> SyncEngineDriver<TestBlockContext, InMemoryBackend, MockProvisioner> {
    let backend = InMemoryBackend::new();
    let engine = SyncEngine::from_index_set(
        toy_index_set(),
        backend,
        EngineConfig {
            batch_size: 8,
            start_height: BlockHeight::new(0),
        },
    )
    .expect("valid index set");
    SyncEngineDriver::new(
        engine,
        MockProvisioner::identity(),
        BlockHeight::new(0),
        BlockHeight::new(target),
    )
}

#[tokio::test]
async fn the_runtime_boots_the_indexer_to_ready() {
    let indexer = IndexerComponent::new(ComponentName("indexer"), build_driver(63));

    // Boot it: it starts Syncing, then reaches Ready once caught up to target.
    indexer.spawn().await.expect("spawn");

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
    .expect("indexer reached Ready");

    assert_eq!(indexer.status().lifecycle, Lifecycle::Ready);

    indexer.stop().await.expect("stop");
    assert_eq!(indexer.status().lifecycle, Lifecycle::Offline);
}
