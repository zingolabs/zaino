//! The driver indexes from a **source** end to end.
//!
//! Over a real source seam: a `MockChain` (implementing dev's `zaino-source`
//! capability traits) feeds a `SourceProvisioner`, which streams into a real
//! `SyncEngine` via `sync_channel`, driven by a `SourceSyncDriver`. Swap
//! `MockChain` for a zebra adapter and the same driver indexes a real chain — the
//! provisioner is generic over the source. A minimal direct-drive harness stands
//! in for the runtime's `RunComponent`, so this crate's tests do not depend on
//! `zaino-runtime`.

#[path = "support/run_harness.rs"]
mod run_harness;

use std::sync::Arc;

use run_harness::drive;
use zaino_indexer::{
    FetchConcurrency, FullBlocks, SourceProvisioner, SourceSyncDriver, SyncTarget,
};
use zaino_primitives::types::{Block, Height};
use zaino_source::mock::{test_block, MockChain};
use zaino_source::{RetryPolicy, ValidatorClient};
use zaino_sync::engine::{EngineConfig, SyncEngine};
use zaino_sync::primitives::BlockHeight;
use zaino_sync::testing::{toy_pipelines, InMemoryBackend, TestBlockContext};

/// Project a fetched block into the toy set's context (height only).
fn to_context(block: Block) -> TestBlockContext {
    TestBlockContext {
        height: u64::from(block.header.height),
        value: u32::from(block.header.height),
    }
}

#[tokio::test]
async fn the_driver_indexes_from_a_source() {
    // A mock validator with blocks 0..=7 (last is the tip).
    let mut chain = MockChain::new();
    for h in 0u32..=7 {
        chain = chain.with_block(test_block(h, u8::try_from(h).expect("small height")));
    }

    let backend = InMemoryBackend::new();
    let engine = SyncEngine::from_pipelines(
        toy_pipelines(),
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
        // Standalone over a non-reorging mock: index right to the tip.
        SyncTarget::Depth { depth: 0 },
        16,
        backend,
    );

    // It fetches from the source, indexes to the tip, and reaches ready.
    let mut run = drive(driver);
    run.await_ready().await;
    run.stop().await.expect("the run ends cleanly");
}
