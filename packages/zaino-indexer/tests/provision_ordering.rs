//! The concurrent provisioner delivers blocks in ascending height order even
//! when fetches complete out of order.
//!
//! The engine's `(SelfCumulative, Append)` indexes thread a carry in height
//! order, so this ordering is load-bearing, not cosmetic. A source here delays
//! each `get_block` by `(max - height)` units, so higher heights *finish first*;
//! with `concurrency > 1` all fetches are in flight at once, so completion order
//! is the reverse of height order. `FuturesOrdered` must still hand the engine
//! blocks `0, 1, 2, …`.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use zaino_indexer::{FetchConcurrency, FullBlocks, SourceProvisioner};
use zaino_primitives::types::{Block, BlockHash, Height};
use zaino_source::mock::test_block;
use zaino_source::{
    GetBlockError, GetChainTipError, NonDomainError, OneShotGetBlock, OneShotGetChainTip,
    QueryError, RetryPolicy, SubscribeChainTip, ValidatorClient, ValidatorSource,
};

const MAX: u32 = 7;

/// A source whose `get_block(h)` sleeps `(MAX - h)` × 5 ms, so the *highest*
/// height resolves first — the reverse of the order the provisioner must emit.
#[derive(Clone)]
struct ReverseDelaySource;

impl ValidatorSource for ReverseDelaySource {
    type NonDomain = NonDomainError;
}

impl OneShotGetBlock for ReverseDelaySource {
    async fn get_block(&self, height: Height) -> Result<Block, QueryError<GetBlockError>> {
        let h = u32::from(height);
        // Higher height → shorter delay → completes sooner.
        tokio::time::sleep(Duration::from_millis(u64::from(MAX - h) * 5)).await;
        Ok(test_block(h, u8::try_from(h).expect("small height")))
    }
}

impl OneShotGetChainTip for ReverseDelaySource {
    async fn get_chain_tip(&self) -> Result<(BlockHash, Height), QueryError<GetChainTipError>> {
        let hash = BlockHash::from([u8::try_from(MAX).expect("small height"); 32]);
        Ok((hash, Height::try_from(MAX).expect("valid")))
    }
}

impl SubscribeChainTip for ReverseDelaySource {}

#[tokio::test]
async fn concurrent_fetch_delivers_in_ascending_height_order() {
    let source = Arc::new(ValidatorClient::new(
        ReverseDelaySource,
        RetryPolicy::default(),
    ));
    // Concurrency > the range, so every fetch is in flight and completion order
    // is fully reversed relative to height.
    let concurrency = FetchConcurrency::new(
        NonZeroUsize::new(usize::try_from(MAX + 4).expect("small")).expect("non-zero"),
    );
    let provisioner = Arc::new(SourceProvisioner::<_, _, _, FullBlocks>::new(
        source,
        |block: Block| u32::from(block.header.height),
        concurrency,
    ));

    let (tx, mut rx) = tokio::sync::mpsc::channel::<u32>(64);
    let feed = Arc::clone(&provisioner);
    let from = Height::try_from(0).expect("valid");
    let to = Height::try_from(MAX).expect("valid");
    let task = tokio::spawn(async move { feed.provision(from, to, tx).await });

    let mut received = Vec::new();
    while let Some(h) = rx.recv().await {
        received.push(h);
    }
    task.await.expect("join").expect("provision ok");

    let expected: Vec<u32> = (0..=MAX).collect();
    assert_eq!(
        received, expected,
        "blocks must arrive in ascending height order despite reversed completion"
    );
}
