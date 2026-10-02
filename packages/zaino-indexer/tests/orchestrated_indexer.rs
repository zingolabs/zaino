//! A `SyncEngineDriver` boots to Ready and stops cleanly.
//!
//! Proves the writer stack at the lifecycle level: a real `SyncEngine` (toy index
//! set + in-memory backend), fed by the mock provisioner, wrapped as a
//! `SyncEngineDriver` (`RunLoop`), driven to its caught-up point and then
//! cancelled. The runtime's `RunComponent` supervises the same `RunLoop` in
//! production; here a minimal direct-drive harness stands in, so this crate's
//! tests do not depend on `zaino-runtime`.

#[path = "support/run_harness.rs"]
mod run_harness;

use run_harness::drive;
use zaino_indexer::SyncEngineDriver;
use zaino_sync::engine::{EngineConfig, SyncEngine};
use zaino_sync::primitives::BlockHeight;
use zaino_sync::testing::{toy_pipelines, InMemoryBackend, MockProvisioner, TestBlockContext};

fn build_driver(
    target: u64,
) -> SyncEngineDriver<TestBlockContext, InMemoryBackend, MockProvisioner> {
    let backend = InMemoryBackend::new();
    let engine = SyncEngine::from_pipelines(
        toy_pipelines(),
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
async fn the_driver_boots_the_indexer_to_ready() {
    // It starts syncing, then signals ready once caught up to the target.
    let mut run = drive(build_driver(63));
    run.await_ready().await;
    run.stop().await.expect("the run ends cleanly");
}
