//! The one task that feeds the [`BlockSink`]: bulk from the indexes' resume point, then the
//! quorum tip through the [`ChainHead`]
//!
//! - Bulk = [`BlockFetchPool`] stream up to the final boundary (decoded on every core), the
//!   boundary following the quorum tip mid-pass
//! - Live = [`ChainHead::advance`] per quorum tip; reorg → `reset` + replay from the window
//! - Quorum tip > depth ahead (startup race, long outage) → bulk again
//! - Validators failing (after the pool's own retries) → wait + retry, bulk and live alike

use std::convert::Infallible;
use std::pin::pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::TryStreamExt;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn, Instrument as _, Span};
use zaino_chainview::QuorumTip;
use zaino_non_finalized_state::{Advance, AdvanceError, ChainHead};
use zaino_primitives::types::{Block, BlockHash, BlockRef, Height, ReorgDepth};
use zaino_source::{BlockFetchPool, GetBlock, GetBlockByHash};

use crate::{
    emit,
    publisher::Publisher,
    report::{self, Human, Progress},
    BlockSink,
};

/// Pause before re-reading the quorum tip after a failed fetch (paces an outage, not a blip)
const RETRY_DELAY: Duration = Duration::from_secs(1);

#[derive(Debug, thiserror::Error)]
pub enum ProduceError {
    /// Validator agreeing on the quorum tip rewrote a final height (past the reorg bound)
    #[error("block {height:?} does not link onto the block below it")]
    Unlinked { height: Height },
    #[error(transparent)]
    BelowWindow(AdvanceError),
    #[error("chain view stopped publishing a tip")]
    ChainViewGone,
}

pub struct Producer<S> {
    sink: Publisher<Block>,
    pool: BlockFetchPool<S>,
    tips: watch::Receiver<Option<QuorumTip>>,
    progress: Arc<Progress>,
    /// Span over following the quorum tip (bulk logs under the caller's)
    live: Span,
}

/// Blocks one chain-head step handed the sink
struct Published {
    blocks: u32,
    txs: usize,
    tip: Arc<Block>,
}

impl<S: GetBlock + GetBlockByHash + Send + Sync + 'static> Producer<S> {
    /// - `depth` = blocks below the tip kept reorg-able
    /// - `durable` = every subscriber's durable tip (production starts after the rearmost)
    pub fn new(
        sink: BlockSink,
        pool: BlockFetchPool<S>,
        tips: watch::Receiver<Option<QuorumTip>>,
        depth: ReorgDepth,
        durable: impl IntoIterator<Item = Option<Height>>,
    ) -> Self {
        let sink = Publisher::new(sink, depth, durable);
        Self { sink, pool, tips, progress: Arc::default(), live: Span::none() }
    }

    /// Following the quorum tip logs under `span`
    pub fn with_live_span(mut self, span: Span) -> Self {
        self.live = span;
        self
    }

    /// Cancel → `Ok`; either way the sink ends with `Shutdown` (every follower persists and stops)
    pub async fn run(mut self, cancel: CancellationToken) -> Result<(), ProduceError> {
        let report = report::run(Arc::clone(&self.progress));
        let produce = async {
            tokio::select! {
                produced = self.produce() => produced,
                () = report => unreachable!("the reporter loops until dropped"),
            }
        };
        let produced = cancel.run_until_cancelled(produce).await;
        self.sink.shutdown();
        match produced {
            None => Ok(()),
            Some(Err(error)) => Err(error),
        }
    }

    async fn produce(&mut self) -> Result<Infallible, ProduceError> {
        let live = self.live.clone();
        let mut head = self.bulk(None).await?;
        loop {
            let tip = self.tip().await?;
            let pool = self.agreeing(tip);
            let ahead = u32::from(tip.block.height).saturating_sub(u32::from(head.tip().height));
            if ahead <= self.sink.depth().get() {
                if self.step(&mut head, tip.block, &pool).instrument(live.clone()).await? {
                    self.changed().await?;
                }
                continue;
            }
            // bulk's `set_tip` finalizes the window → only once the validator still holds our tip
            // (then buried past the reorg bound); else step onto its block at our height = reorg
            let ours = head.tip();
            let theirs = block_at(&pool, ours.height).instrument(live.clone()).await.header().hash;
            if theirs == ours.hash {
                head = self.bulk(Some(ours.hash)).await?;
            } else {
                let theirs = BlockRef { hash: theirs, height: ours.height };
                self.step(&mut head, theirs, &pool).instrument(live.clone()).await?;
            }
        }
    }

    /// Validators agreeing on `tip`: the only ones trusted to answer by height
    /// - another may serve a stale branch at a height `tip` made final (a lagging node)
    fn agreeing(&self, tip: QuorumTip) -> BlockFetchPool<S> {
        self.pool.among(tip.agreed_by.positions())
    }

    /// `head` advanced onto `tip`, the sink caught up (`false` = fetch failed, retry paced)
    async fn step(
        &mut self,
        head: &mut ChainHead,
        tip: BlockRef,
        pool: &BlockFetchPool<S>,
    ) -> Result<bool, ProduceError> {
        let before = head.tip();
        match head.advance(tip, pool).await {
            // bulk may have announced a higher tip, since retreated onto its anchor
            Ok(Advance::Unchanged) => {
                self.sink.set_tip(tip.height).await;
                emit::tip(tip.height);
            }
            Ok(Advance::Extended) => {
                let published = self.publish(head, tip).await;
                self.advanced(&published);
            }
            Ok(Advance::Reorg { fork }) => {
                let resume = self.sink.reset().await;
                assert!(resume <= fork, "reorg at {fork:?} reaches final height {resume:?}");
                emit::reorg();
                let published = self.publish(head, tip).await;
                warn!(
                    fork = u32::from(fork),
                    dropped = u32::from(before.height) + 1 - u32::from(fork),
                    added = u32::from(tip.height) + 1 - u32::from(fork),
                    height = u32::from(tip.height),
                    hash = %tip.hash,
                    "Chain reorg detected"
                );
                self.advanced(&published);
            }
            Err(error @ AdvanceError::BelowWindow { .. }) => {
                return Err(ProduceError::BelowWindow(error))
            }
            Err(error) => {
                warn!(%error, retry = %Human(RETRY_DELAY), "Chain tip fetch failed");
                tokio::time::sleep(RETRY_DELAY).await;
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// New tip + where finality now stands (one line per chain-head step, ~75 s apart)
    fn advanced(&self, published: &Published) {
        let header = published.tip.header();
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_secs());
        let age = Duration::from_secs(now.saturating_sub(u64::from(header.time)));
        let finalized = self.sink.final_tip().map_or(0, u32::from);
        info!(
            height = u32::from(header.height),
            hash = %header.hash,
            blocks = published.blocks,
            txs = published.txs,
            age = %Human(age),
            finalized,
            "Chain tip advanced"
        );
    }

    /// Heights `start` (the sink's next) to `end` (tip − depth), both inclusive, into the sink;
    /// a fresh chain head anchored on the last block
    ///
    /// - `end` follows the quorum tip mid-pass (one pass per catch-up, not per tip snapshot)
    /// - Stream exhausted below a raised `end` / failed fetch → reopened from what was added,
    ///   from the validators agreeing on the tip that raised it
    /// - `parent` = hash the first block must extend (`None` at boot: followers check their own)
    async fn bulk(&mut self, mut parent: Option<BlockHash>) -> Result<ChainHead, ProduceError> {
        let start = self.sink.next();
        // final already (an index durable past `start`), whatever the tip says now
        let owed = self.sink.final_tip();
        let needed = owed.map_or(start, |owed| owed.max(start));
        // indexes ahead of the validators (rolled back / resyncing) → wait for them
        let mut tip = loop {
            let tip = self.tip().await?;
            if tip.block.height >= needed {
                break tip;
            }
            self.changed().await?;
        };
        let mut end = tip.block.height.saturating_sub(self.sink.depth().get());
        end = owed.map_or(end, |owed| owed.max(end));
        // durable through the final boundary already (restart near the tip) → anchor on the
        // durable tip, not re-added; everything above stays non-final, added live
        if end < start {
            let durable = start.checked_sub(1).expect("height 0 final under any tip");
            let anchor = block_at(&self.agreeing(tip), durable).await;
            return Ok(ChainHead::new(Arc::new(anchor), self.sink.depth()));
        }
        self.progress.start(start, end, tip.block.height);
        self.sink.set_tip(tip.block.height).await;
        emit::tip(tip.block.height);

        let mut last = None;
        while self.sink.next() <= end {
            // this stream's end, inclusive (`end` may rise under it)
            let pass_end = end;
            let pool = self.agreeing(tip);
            let mut blocks = pin!(pool.blocks(self.sink.next(), pass_end));
            let fetched = loop {
                match blocks.try_next().await {
                    Ok(Some(block)) => {
                        if parent.is_some_and(|parent| parent != block.header().prev_hash) {
                            return Err(ProduceError::Unlinked { height: block.header().height });
                        }
                        parent = Some(block.header().hash);
                        let block = Arc::new(block);
                        self.add(&block).await;
                        last = Some(block);
                        self.extend(&mut end, &mut tip).await;
                    }
                    Ok(None) => break Ok(()),
                    Err(error) => break Err(error),
                }
            };
            match fetched {
                Ok(()) => assert_eq!(self.sink.next(), pass_end.next(), "bulk stream ended early"),
                Err(error) => {
                    warn!(
                        %error,
                        next = u32::from(self.sink.next()),
                        retry = %Human(RETRY_DELAY),
                        "Block fetch failed"
                    );
                    tokio::time::sleep(RETRY_DELAY).await;
                }
            }
        }
        self.progress.finish();
        let anchor = last.expect("bulk range start..=end never empty");
        Ok(ChainHead::new(anchor, self.sink.depth()))
    }

    /// Quorum tip past `end` + depth → `end` raised, `target` = that tip
    ///
    /// - `set_tip` before the raise (every block up to the new `end` lands final)
    async fn extend(&mut self, end: &mut Height, target: &mut QuorumTip) {
        let Some(tip) = *self.tips.borrow_and_update() else {
            return;
        };
        let raised = tip.block.height.saturating_sub(self.sink.depth().get());
        if raised > *end {
            self.sink.set_tip(tip.block.height).await;
            emit::tip(tip.block.height);
            self.progress.extend(raised);
            *end = raised;
            *target = tip;
        }
    }

    /// Sink catches up to `head` (after an extension, or a reset that rewound it)
    async fn publish(&mut self, head: &ChainHead, tip: BlockRef) -> Published {
        self.sink.set_tip(tip.height).await;
        emit::tip(tip.height);
        let (mut blocks, mut txs) = (0, 0);
        // retreat onto the final boundary → nothing non-final left to replay
        for block in head.best_chain_from(self.sink.next()) {
            self.add(block).await;
            blocks += 1;
            txs += block.transactions().len();
        }
        let past_tip = tip.height.checked_add(1).expect("tip below the height maximum");
        assert_eq!(self.sink.next(), past_tip, "sink not at the chain head tip");
        let tip = head.best_chain_from(tip.height).next().expect("window holds its tip");
        Published { blocks, txs, tip: Arc::clone(tip) }
    }

    async fn add(&mut self, block: &Arc<Block>) {
        emit::added(block);
        self.progress.added(block);
        self.sink.add(block.header().height, Arc::clone(block)).await;
    }

    /// Latest quorum tip, waiting out a lost quorum
    async fn tip(&mut self) -> Result<QuorumTip, ProduceError> {
        loop {
            if let Some(tip) = *self.tips.borrow_and_update() {
                return Ok(tip);
            }
            self.changed().await?;
        }
    }

    async fn changed(&mut self) -> Result<(), ProduceError> {
        self.tips.changed().await.map_err(|_| ProduceError::ChainViewGone)
    }
}

/// Block `height` off `pool`, retried (paced) until a validator answers
async fn block_at<S>(pool: &BlockFetchPool<S>, height: Height) -> Block
where
    S: GetBlock + GetBlockByHash + Send + Sync + 'static,
{
    loop {
        match pin!(pool.blocks(height, height)).try_next().await {
            Ok(Some(block)) => return block,
            Ok(None) => unreachable!("a one-height fetch yields its block or fails"),
            Err(error) => {
                warn!(%error, height = u32::from(height), retry = %Human(RETRY_DELAY), "Block fetch failed");
                tokio::time::sleep(RETRY_DELAY).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use zaino_chainview::EndpointSet;
    use zaino_primitives::types::{BlockHeader, Transaction};
    use zaino_source::mock::MockChain;
    use zaino_source::FetchRoute;

    use super::*;
    use crate::Step;

    /// Depth 3, index empty, quorum tip A5 (inclusive end 2), one-step queue: tip moves to A8
    /// mid-bulk → end raised to 5, 3 to 5 (both inclusive) final on arrival, then 6 to 8 (both
    /// inclusive) live (non-final); tip moves to B9
    /// (forks after A6) → reset, replay from the first non-final height out of the window, the
    /// fork itself fetched; tip retreats to B7, then onto the final A6, then forward to B8;
    /// cancel → `Shutdown` last in the queue
    #[tokio::test]
    async fn bulks_to_a_moving_final_boundary_then_follows_the_quorum_tip_through_reorgs() {
        let block = |height: u32, byte: u8, parent: u8| {
            Block::new(
                BlockHeader::for_tests(height, [byte; 32], [parent; 32], 0),
                vec![Transaction {
                    txid: [byte; 32].into(),
                    transparent: Default::default(),
                    sprout: Default::default(),
                    sapling: Default::default(),
                    orchard: Default::default(),
                    ironwood: Default::default(),
                }],
            )
        };
        let a: Vec<_> = (0..=8).map(|h| block(h, 0x10 + h as u8, 0x0f + h as u8)).collect();
        let b = [block(7, 0x27, 0x16), block(8, 0x28, 0x27), block(9, 0x29, 0x28)];
        let validator = |blocks: Vec<&Block>| {
            Arc::new(
                blocks
                    .into_iter()
                    .fold(MockChain::new(), |chain, block| chain.with_block(block.clone())),
            )
        };
        let pool = BlockFetchPool::new(
            vec![validator(a.iter().collect()), validator(a[..=6].iter().chain(&b).collect())],
            FetchRoute::Spread,
            NonZeroUsize::new(4).expect("nz"),
        );
        let quorum = |block: &Block, agreed_by: &[usize]| {
            Some(QuorumTip {
                block: BlockRef { hash: block.header().hash, height: block.header().height },
                agreed_by: EndpointSet::at(agreed_by.iter().copied()),
            })
        };
        let (tips, tips_rx) = watch::channel(quorum(&a[5], &[0, 1]));
        let depth = ReorgDepth::new(std::num::NonZeroU32::new(3).expect("nz"));
        let mut block_sink = BlockSink::new("blocks");
        // budget 1 = one queued step (producer parks on the next add until the test takes)
        let mut index = block_sink.subscribe("index", NonZeroUsize::new(1).expect("nz"));
        let cancel = CancellationToken::new();
        let producer = Producer::new(block_sink, pool, tips_rx, depth, [None]);
        let producer = tokio::spawn(producer.run(cancel.clone()));

        let mut steps = Vec::new();
        // `count` block / finality / reset steps
        let mut take = async |count: usize| {
            for _ in 0..count {
                let step = match index.next().await {
                    Step::Apply { height, finalized, data } => {
                        assert_eq!(data.header().height, height);
                        let byte = <[u8; 32]>::from(data.header().hash)[0];
                        format!("{byte:x}{}", if finalized { "f" } else { "" })
                    }
                    Step::Finalized { height } => format!("F{height}"),
                    Step::Reset => "R".to_owned(),
                    Step::Shutdown => panic!("producer shut down uncancelled"),
                };
                steps.push(step);
            }
        };
        take(1).await;
        tips.send_replace(quorum(&a[8], &[0]));
        take(8).await;
        tips.send_replace(quorum(&b[2], &[1]));
        take(5).await;
        tips.send_replace(quorum(&b[0], &[1]));
        take(2).await;
        tips.send_replace(quorum(&a[6], &[0, 1]));
        take(1).await;
        tips.send_replace(quorum(&b[1], &[1]));
        take(2).await;

        // bulk final past the raised end, live non-final, reset replays from 6 (now final) onto B;
        // retreat to B7 → reset + replay B7; retreat onto the final A6 → bare reset; B8 → forward
        assert_eq!(steps.join(" "), "10f 11f 12f 13f 14f 15f 16 17 18 R 16f 27 28 29 R 27 R 27 28");
        cancel.cancel();
        producer.await.expect("join").expect("cancel = clean stop");
        assert!(matches!(index.next().await, Step::Shutdown), "cancel → Shutdown");
    }
}
