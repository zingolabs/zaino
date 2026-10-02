//! The driver consumes the reorg horizon the volatile tier publishes.
//!
//! These drive a real [`SourceSyncDriver`] over a mock validator ([`MockChain`]
//! behind a [`ValidatorClient`]) and the toy index set, in [`SyncTarget::Seam`]
//! mode, so the seam contract is exercised end to end: the horizon bounds what is
//! indexed, a branch disagreement at the horizon is refused before anything is
//! written, and a horizon published mid-run is followed.

use std::sync::Arc;
use std::time::Duration;

use zaino_component::{CancellationToken, RunLoop, RunReporter};
use zaino_finality::Seam;
use zaino_primitives::types::{Block, BlockHash, Height};
use zaino_source::mock::{test_block, MockChain};
use zaino_source::{RetryPolicy, ValidatorClient};
use zaino_sync::backend::Backend;
use zaino_sync::testing::{toy_pipelines, InMemoryBackend, TestBlockContext};

use crate::{FetchConcurrency, IndexerError, SourceSyncDriver, SyncTarget, SyncTuning};

fn height(n: u32) -> Height {
    Height::try_from(n).expect("test heights are in range")
}

fn hash(n: u8) -> BlockHash {
    BlockHash::from([n; 32])
}

fn tuning() -> SyncTuning {
    SyncTuning {
        batch_size: 4,
        channel_capacity: 8,
        concurrency: FetchConcurrency::SERIAL,
    }
}

/// Project a fetched block into the toy set's context (height only).
fn to_context(block: Block) -> TestBlockContext {
    TestBlockContext {
        height: u64::from(block.header.height),
        value: u32::from(block.header.height),
    }
}

/// A no-op reporter: these tests observe durability through the backend, not the
/// readiness signal.
fn reporter() -> RunReporter {
    RunReporter::new(|_report| {})
}

/// A mock validator serving blocks `0..=tip`, each block's hash a function of its
/// height (`test_block(h, h)`), wrapped in the resilient [`ValidatorClient`] the
/// provisioner binds.
fn source_to(tip: u32) -> Arc<ValidatorClient<MockChain>> {
    let mut chain = MockChain::new();
    for h in 0..=tip {
        chain = chain.with_block(test_block(h, u8::try_from(h).expect("small height")));
    }
    Arc::new(ValidatorClient::new(chain, RetryPolicy::default()))
}

/// Polls the persisted watermark until it reaches `target`, so a test observes
/// durability rather than guessing at a delay.
async fn wait_for_watermark(backend: &InMemoryBackend, target: Height) {
    for _ in 0..400 {
        let committed = backend.reader().ok().and_then(|reader| {
            zaino_persistence_codec::watermark::read(&reader)
                .ok()
                .flatten()
        });
        if committed == Some(target) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the watermark never reached {target:?}");
}

/// The committed watermark currently on disk.
fn committed(backend: &InMemoryBackend) -> Option<Height> {
    zaino_persistence_codec::watermark::read(&backend.reader().expect("reader")).expect("read")
}

#[tokio::test]
async fn the_driver_refuses_a_block_whose_hash_differs_from_the_horizon() {
    // Review Focus 5: a branch disagreement at the seam must be refused before
    // anything is written, not detected after divergent data is committed.
    let (mut horizon, watermark) = Seam::new(0, 10).split();
    let released = horizon
        .advance(height(10), hash(0xAA))
        .expect("first publish is legal");
    assert_eq!(released.height(), height(10));

    // The source serves a different block at height 10 than the horizon names.
    let mut chain = MockChain::new();
    for h in 0..=9 {
        chain = chain.with_block(test_block(h, u8::try_from(h).expect("small height")));
    }
    chain = chain.with_block(test_block(10, 0xBB));
    let source = Arc::new(ValidatorClient::new(chain, RetryPolicy::default()));

    let backend = InMemoryBackend::new();
    let driver = SourceSyncDriver::resuming(
        &backend,
        toy_pipelines(),
        source,
        to_context,
        tuning(),
        SyncTarget::Seam(watermark),
    )
    .expect("the driver builds");

    let error = Arc::new(driver)
        .run(CancellationToken::new(), reporter())
        .await
        .expect_err("a branch disagreement at the horizon is refused");
    assert!(
        matches!(error, IndexerError::HorizonBranchMismatch { .. }),
        "got {error:?}"
    );
    assert_eq!(committed(&backend), None, "nothing was committed");
}

#[tokio::test]
async fn the_driver_indexes_to_the_published_horizon() {
    let (mut horizon, watermark) = Seam::new(0, 10).split();
    horizon.advance(height(10), hash(10)).expect("legal");

    let backend = InMemoryBackend::new();
    let driver = SourceSyncDriver::resuming(
        &backend,
        toy_pipelines(),
        source_to(10),
        to_context,
        tuning(),
        SyncTarget::Seam(watermark),
    )
    .expect("the driver builds");

    let cancel = CancellationToken::new();
    let run = tokio::spawn({
        let cancel = cancel.clone();
        async move { Arc::new(driver).run(cancel, reporter()).await }
    });

    // The driver reaches the horizon and no further.
    wait_for_watermark(&backend, height(10)).await;
    cancel.cancel();
    run.await
        .expect("the task joins")
        .expect("the run ends cleanly");
}

#[tokio::test]
async fn the_driver_follows_the_horizon_as_it_advances() {
    // Exercises the await path: the horizon is published *after* the driver is
    // running, so the follow loop learns of it through `await_released`.
    let (mut horizon, watermark) = Seam::new(0, 10).split();

    let backend = InMemoryBackend::new();
    let driver = SourceSyncDriver::resuming(
        &backend,
        toy_pipelines(),
        source_to(10),
        to_context,
        tuning(),
        SyncTarget::Seam(watermark),
    )
    .expect("the driver builds");

    let cancel = CancellationToken::new();
    let run = tokio::spawn({
        let cancel = cancel.clone();
        async move { Arc::new(driver).run(cancel, reporter()).await }
    });

    // Nothing is published yet, so nothing is committed; the follow loop awaits.
    // Publish a first horizon — the driver follows it to height 5.
    horizon.advance(height(5), hash(5)).expect("legal");
    wait_for_watermark(&backend, height(5)).await;

    // A later horizon advances the watermark further, through the await path.
    horizon.advance(height(10), hash(10)).expect("legal");
    wait_for_watermark(&backend, height(10)).await;

    cancel.cancel();
    run.await
        .expect("the task joins")
        .expect("the run ends cleanly");
}
