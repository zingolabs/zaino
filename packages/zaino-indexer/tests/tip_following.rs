//! The indexer follows the source tip after initial catch-up (standalone mode).
//!
//! A growing mock source pushes tip updates as new blocks arrive; the
//! `SourceSyncDriver` in `SyncTarget::Depth` mode catches up to the initial
//! boundary (ready), then indexes each new range as the tip advances — driven by
//! `SubscribeChainTip`. A minimal direct-drive harness stands in for the
//! runtime's `RunComponent`, so this crate's tests do not depend on
//! `zaino-runtime`.

#[path = "support/run_harness.rs"]
mod run_harness;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use run_harness::drive;
use tokio::sync::watch;

use zaino_component::RunLoop;
use zaino_indexer::{
    FetchConcurrency, FullBlocks, IndexerError, SourceProvisioner, SourceSyncDriver, SyncTarget,
};
use zaino_primitives::types::{Block, BlockHash, Height};
use zaino_source::mock::test_block;
use zaino_source::{
    GetBlockError, GetChainTipError, OneShotGetBlock, OneShotGetChainTip, QueryError, RetryPolicy,
    SubscribeChainTip, TipObservation, ValidatorClient,
};
use zaino_sync::engine::{EngineConfig, SyncEngine};
use zaino_sync::primitives::BlockHeight;
use zaino_sync::testing::{toy_pipelines, InMemoryBackend, TestBlockContext};

/// A mock source whose chain grows: `extend_to` appends blocks and pushes a tip
/// update, so a subscriber follows.
#[derive(Clone)]
struct GrowingSource {
    blocks: Arc<Mutex<HashMap<u32, Block>>>,
    tip: watch::Sender<TipObservation>,
}

impl GrowingSource {
    /// A source seeded with blocks `0..=to`.
    fn new(to: u32) -> Self {
        let mut blocks = HashMap::new();
        for h in 0..=to {
            blocks.insert(h, test_block(h, u8::try_from(h).expect("small height")));
        }
        let tip_obs = tip_observation(to);
        let (tip, _) = watch::channel(tip_obs);
        Self {
            blocks: Arc::new(Mutex::new(blocks)),
            tip,
        }
    }

    /// Append blocks up to `to` and publish the new tip.
    fn extend_to(&self, to: u32) {
        let mut blocks = self.blocks.lock().expect("blocks poisoned");
        let from = u32::try_from(blocks.len()).expect("small chain");
        for h in from..=to {
            blocks.insert(h, test_block(h, u8::try_from(h).expect("small height")));
        }
        drop(blocks);
        self.tip
            .send(tip_observation(to))
            .expect("tip receiver live");
    }
}

fn tip_observation(height: u32) -> TipObservation {
    TipObservation::now(
        BlockHash::from([u8::try_from(height).expect("small height"); 32]),
        Height::try_from(height).expect("valid height"),
    )
}

impl zaino_source::ValidatorSource for GrowingSource {
    type NonDomain = zaino_source::NonDomainError;
}

impl OneShotGetBlock for GrowingSource {
    async fn get_block(&self, height: Height) -> Result<Block, QueryError<GetBlockError>> {
        self.blocks
            .lock()
            .expect("blocks poisoned")
            .get(&u32::from(height))
            .cloned()
            .ok_or(QueryError::Domain(GetBlockError::HeightNotFound(height)))
    }
}

impl OneShotGetChainTip for GrowingSource {
    async fn get_chain_tip(&self) -> Result<(BlockHash, Height), QueryError<GetChainTipError>> {
        let obs = *self.tip.borrow();
        Ok((obs.hash, obs.height))
    }
}

impl SubscribeChainTip for GrowingSource {
    fn subscribe_to_chain_tip(&self) -> Option<watch::Receiver<TipObservation>> {
        Some(self.tip.subscribe())
    }
}

fn to_context(block: Block) -> TestBlockContext {
    TestBlockContext {
        height: u64::from(block.header.height),
        value: u32::from(block.header.height),
    }
}

/// Read how many entries the value-index has committed (one per indexed block).
fn indexed_block_count(backend: &InMemoryBackend) -> usize {
    use zaino_sync::testing::toy_indexes::value_index;
    backend.entries(value_index::ID.into()).len()
}

/// Build a `SourceSyncDriver` over `source` in `SyncTarget::Depth` mode.
fn driver_over(
    source: GrowingSource,
    backend: &InMemoryBackend,
    depth: u32,
) -> impl RunLoop<Error = IndexerError> {
    let engine = SyncEngine::from_pipelines(
        toy_pipelines(),
        backend.clone(),
        EngineConfig {
            batch_size: 4,
            start_height: BlockHeight::new(0),
        },
    )
    .expect("valid index set");

    let validator = ValidatorClient::new(source, RetryPolicy::default());
    let provisioner = Arc::new(SourceProvisioner::<_, _, _, FullBlocks>::new(
        Arc::new(validator),
        to_context as fn(Block) -> TestBlockContext,
        FetchConcurrency::SERIAL,
    ));
    SourceSyncDriver::new(
        engine,
        provisioner,
        Height::try_from(0).expect("valid height"),
        SyncTarget::Depth { depth },
        16,
        backend.clone(),
    )
}

/// Spin until `cond` holds, failing the test if it never does.
async fn wait_until(mut cond: impl FnMut() -> bool) {
    for _ in 0..500 {
        if cond() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("condition never held");
}

#[tokio::test]
async fn the_indexer_follows_the_tip() {
    let source = GrowingSource::new(3); // blocks 0..=3
    let backend = InMemoryBackend::new();
    let driver = driver_over(source.clone(), &backend, 0);

    let mut run = drive(driver);

    // Initial catch-up to tip 3 → ready, 4 blocks indexed (0..=3).
    run.await_ready().await;
    assert_eq!(indexed_block_count(&backend), 4, "blocks 0..=3 indexed");

    // The chain grows to 6 → the indexer follows and indexes 4..=6.
    source.extend_to(6);
    wait_until(|| indexed_block_count(&backend) == 7).await;

    run.stop().await.expect("the run ends cleanly");
}

#[tokio::test]
async fn the_indexer_stops_at_the_finalised_boundary() {
    // Tip 6, depth 2 → the indexer builds only the append-only range [0, 4]; the
    // volatile window (5, 6) is the chain-head's concern, not the finalised
    // index's.
    let source = GrowingSource::new(6);
    let backend = InMemoryBackend::new();
    let driver = driver_over(source, &backend, 2);

    let mut run = drive(driver);
    run.await_ready().await;

    assert_eq!(
        indexed_block_count(&backend),
        5,
        "only blocks 0..=4 (tip 6 − depth 2) are indexed; 5 and 6 stay volatile",
    );

    run.stop().await.expect("the run ends cleanly");
}
