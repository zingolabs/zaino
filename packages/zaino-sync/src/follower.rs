//! [`FinalFollower`]: every final block, by height from a trusted member, in order, onto the final
//! stream (`data-sink.md`)
//!
//! - Target = the chain's final tip height only (no header history read: trusted = trusted)
//! - Starts after the lowest durable tip (an index ahead skips what it holds)
//! - Each block's parent = the block sent before it, and = the durable tip of each index it
//!   extends (else `Unlinked` / `Diverged`: stop, never skip)
//! - Bulk and tip alike: each block once, after it turns final (the NFS never sends)
//! - The block at the chain's final tip = `Finalized` (writers commit after it, else on a full
//!   buffer)

use std::num::NonZeroUsize;
use std::sync::Arc;

use futures::stream::{FuturesOrdered, StreamExt};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use zaino_header_chain::VerifiedChain;
use zaino_persistence::IndexKind;
use zaino_primitives::types::{Block, BlockHash, BlockRef, Height};
use zaino_source::ChainDataSource;
use zaino_traffic::{TrafficBalancer, Urgency};

use crate::{emit, fetch_at, IndexerDataSink, Step, Subscription, SyncProgress};

#[derive(Debug, thiserror::Error)]
pub enum FollowError {
    #[error(
        "{index} committed {expected} at {height:?}, the trusted validator's chain has {got} \
         (resync required)"
    )]
    Diverged { index: &'static str, height: Height, expected: BlockHash, got: BlockHash },
    #[error("block {height:?} does not extend the block sent before it (validator history moved)")]
    Unlinked { height: Height },
    #[error("header sync stopped publishing the verified chain")]
    ChainGone,
}

pub struct FinalFollower<S> {
    chain: watch::Receiver<Option<Arc<VerifiedChain>>>,
    balancer: TrafficBalancer<S>,
    lookahead: NonZeroUsize,
    sink: IndexerDataSink<Block>,
    durable: Vec<(IndexKind, Option<BlockRef>)>,
    progress: SyncProgress,
}

impl<S: ChainDataSource> FinalFollower<S> {
    /// `lookahead` = blocks in flight ahead of the next block sent ([`BLOCKS_PER_REQUEST`] per
    /// batched request)
    pub fn new(
        chain: watch::Receiver<Option<Arc<VerifiedChain>>>,
        balancer: TrafficBalancer<S>,
        lookahead: NonZeroUsize,
    ) -> Self {
        Self {
            chain,
            balancer,
            lookahead,
            sink: IndexerDataSink::new(),
            durable: Vec::new(),
            progress: SyncProgress::default(),
        }
    }

    /// `kind`'s stream; `durable` = its store's durable tip; `queue` = the stream's byte budget
    pub fn subscribe(
        &mut self,
        kind: IndexKind,
        durable: Option<BlockRef>,
        queue: NonZeroUsize,
    ) -> Subscription<Block> {
        self.durable.push((kind, durable));
        self.sink.subscribe(kind.name(), queue)
    }

    pub fn progress(&self) -> SyncProgress {
        self.progress.clone()
    }

    /// - Cancel → `Ok`
    /// - Either way: the stream ends with `Shutdown` (every writer commits, stops)
    pub async fn run(self, cancel: CancellationToken) -> Result<(), FollowError> {
        let Self { chain, balancer, lookahead, sink, durable, progress } = self;
        let followed = cancel
            .run_until_cancelled(follow(chain, &balancer, lookahead, &sink, durable, &progress))
            .await;
        sink.shutdown();
        followed.unwrap_or(Ok(()))
    }
}

async fn follow<S: ChainDataSource>(
    mut chain: watch::Receiver<Option<Arc<VerifiedChain>>>,
    balancer: &TrafficBalancer<S>,
    lookahead: NonZeroUsize,
    sink: &IndexerDataSink<Block>,
    durable: Vec<(IndexKind, Option<BlockRef>)>,
    progress: &SyncProgress,
) -> Result<(), FollowError> {
    let next = |tip: &Option<BlockRef>| tip.map_or(Height::GENESIS, |tip| tip.height.next());
    let mut wanted = durable.iter().map(|(_, tip)| next(tip)).min().unwrap_or(Height::GENESIS);
    let mut last_sent: Option<BlockHash> = None;
    let mut fetching = FuturesOrdered::new();
    let requests = lookahead.get().div_ceil(BLOCKS_PER_REQUEST);
    loop {
        let final_height = chain.borrow_and_update().as_ref().map(|chain| chain.final_tip().height);
        while fetching.len() < requests && Some(wanted) <= final_height {
            let mut heights = Vec::with_capacity(BLOCKS_PER_REQUEST);
            while heights.len() < BLOCKS_PER_REQUEST && Some(wanted) <= final_height {
                heights.push(wanted);
                wanted = wanted.next();
            }
            fetching.push_back(fetch_at(balancer.clone(), heights.into(), Urgency::Bulk));
        }
        tokio::select! {
            changed = chain.changed() => changed.map_err(|_| FollowError::ChainGone)?,
            Some(bodies) = fetching.next(), if !fetching.is_empty() => {
                for body in bodies {
                    let block = Arc::clone(body.block());
                    link(&durable, last_sent, &block)?;
                    let height = block.header().height;
                    last_sent = Some(block.header().hash);
                    emit::handed(&block);
                    progress.hand(height);
                    let final_tip = chain.borrow().as_ref().map(|chain| chain.final_tip().height);
                    let step = match Some(height) == final_tip {
                        true => Step::Finalized { height, data: block },
                        false => Step::Apply { height, data: block },
                    };
                    sink.send(step).await;
                }
            }
        }
    }
}

/// Blocks per batched `getblock` request (4 full 2 MB blocks as hex = 16 MiB < the 32 MiB reply
/// cap)
const BLOCKS_PER_REQUEST: usize = 4;

/// `block`'s parent = each durable tip it extends (an index's own chain), then = the block sent
/// before it (the validator's history unmoved)
fn link(
    durable: &[(IndexKind, Option<BlockRef>)],
    last_sent: Option<BlockHash>,
    block: &Block,
) -> Result<(), FollowError> {
    let (height, parent) = (block.header().height, block.header().prev_hash);
    for (kind, tip) in durable {
        let Some(tip) = tip.filter(|tip| tip.height.next() == height) else { continue };
        if parent != tip.hash {
            let (index, height, expected) = (kind.name(), tip.height, tip.hash);
            return Err(FollowError::Diverged { index, height, expected, got: parent });
        }
    }
    match last_sent {
        Some(last) if last != parent => Err(FollowError::Unlinked { height }),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use zaino_header_chain::testing::HeaderViews;
    use zaino_primitives::testing::{h, MockChain};
    use zaino_source::testing::{Lie, MockValidator};
    use zaino_traffic::{Limits, Trusted};

    use super::*;

    /// Chain A 0..=11, final 5 then 8; an honest member + a `Lie::Poisoned` liar (by height
    /// too); two subscribers, one durable at A2, one fresh:
    /// - both queues = A0..=A8, once each, in order, the honest bodies (a lie never sent)
    /// - the final tip's block (A5, then A8) `Finalized`, every other `Apply`
    /// - nothing above the final tip; progress = the last height sent, blocks counted
    /// - cancel → `Ok`, `Shutdown` last in every queue
    #[tokio::test(start_paused = true)]
    async fn every_final_block_reaches_every_subscriber_once_in_order_from_the_lowest_durable_tip()
    {
        let mut chain = MockChain::regtest();
        let a11 = chain.mine_empty(11);
        let trunk = chain.blocks(a11);
        let members = [None, Some(Lie::Poisoned)].map(|lie| {
            let member = MockValidator::following(&chain, a11);
            member.lie(lie);
            Arc::new(member)
        });
        let limits = Limits::new(8).expect("8 ≥ MIN_CONNECTIONS");
        let trusted = members.iter().map(|member| Trusted {
            source: Arc::clone(member),
            priority: 0,
            limits,
        });
        let (balancer, balancing) = TrafficBalancer::new(trusted.collect(), None);
        let stop_balancing = CancellationToken::new();
        tokio::spawn(balancing.run(stop_balancing.clone()));
        let final_at_five = Arc::new(chain.verified_final(a11, h(5)));
        let (verified, verified_rx) = watch::channel(Some(final_at_five));

        let lookahead = NonZeroUsize::new(3).expect("nonzero");
        let mut follower = FinalFollower::new(verified_rx, balancer, lookahead);
        let durable_tip_a2 = Some(trunk[2].at());
        let mut durable_at_a2 =
            follower.subscribe(IndexKind::BlockHash, durable_tip_a2, NonZeroUsize::MAX);
        let mut fresh = follower.subscribe(IndexKind::TreeState, None, NonZeroUsize::MAX);
        let progress = follower.progress();
        let cancel = CancellationToken::new();
        let running = tokio::spawn(follower.run(cancel.clone()));
        let sent = async |queue: &mut Subscription<Block>, count: usize| {
            let mut sent = Vec::new();
            for _ in 0..count {
                let (height, data, finalized) = match queue.next().await {
                    Step::Apply { height, data } => (height, data, false),
                    Step::Finalized { height, data } => (height, data, true),
                    Step::Shutdown => panic!("Shutdown early"),
                };
                assert_eq!(data.header().height, height, "step height = its block's");
                sent.push((data.at(), finalized));
            }
            sent
        };
        let steps = |range: std::ops::RangeInclusive<usize>, final_tip: usize| -> Vec<_> {
            range.map(|at| (trunk[at].at(), at == final_tip)).collect()
        };

        let first = steps(0..=5, 5);
        assert_eq!(sent(&mut durable_at_a2, 6).await, first, "from the lowest durable tip (fresh)");
        assert_eq!(sent(&mut fresh, 6).await, first, "the same steps to every subscriber");
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        assert_eq!(progress.handed(), Some(h(5)), "nothing above the final tip");

        verified.send_replace(Some(Arc::new(chain.verified_final(a11, h(8)))));
        let next = steps(6..=8, 8);
        assert_eq!(sent(&mut durable_at_a2, 3).await, next, "the final tip moved: its blocks next");
        assert_eq!(sent(&mut fresh, 3).await, next);
        assert_eq!((progress.handed(), progress.blocks()), (Some(h(8)), 9));

        cancel.cancel();
        running.await.expect("follower task").expect("cancel = clean stop");
        for queue in [&mut durable_at_a2, &mut fresh] {
            assert!(matches!(queue.next().await, Step::Shutdown), "Shutdown last");
        }
        stop_balancing.cancel();
    }

    /// Broken links stop the follower, naming what broke:
    /// - durable X1 (A1's sibling), final 5: A2's parent ≠ X1 → `Diverged` naming the index,
    ///   before any step
    /// - A0..=A5 sent, then the validator's history moves (B 5..=11 off A4), final 8: B6's parent
    ///   ≠ A5 → `Unlinked` at 6, nothing past A5 sent
    #[tokio::test(start_paused = true)]
    async fn a_block_off_the_chain_already_sent_or_committed_stops_the_follower() {
        let mut chain = MockChain::regtest();
        let a7 = chain.mine_empty(7);
        let x1 = chain.fork(h(0)).mine_empty(1).tip();
        let b11 = chain.fork(h(4)).mine_empty(7).tip();
        let limits = Limits::new(8).expect("8 ≥ MIN_CONNECTIONS");
        let validator = Arc::new(MockValidator::following(&chain, a7));
        let trusted = vec![Trusted { source: Arc::clone(&validator), priority: 0, limits }];
        let (balancer, balancing) = TrafficBalancer::new(trusted, None);
        let stop_balancing = CancellationToken::new();
        tokio::spawn(balancing.run(stop_balancing.clone()));

        let final_at_five = Some(Arc::new(chain.verified_final(a7, h(5))));
        let (_verified, verified_rx) = watch::channel(final_at_five);
        let mut follower = FinalFollower::new(verified_rx, balancer.clone(), NonZeroUsize::MIN);
        let durable_tip_a4 = Some(chain.blocks(a7)[4].at());
        let mut above = follower.subscribe(IndexKind::TreeState, durable_tip_a4, NonZeroUsize::MAX);
        let mut foreign = follower.subscribe(IndexKind::BlockHash, Some(x1), NonZeroUsize::MAX);
        let stopped =
            follower.run(CancellationToken::new()).await.expect_err("X1 off the validator's chain");
        let a1 = chain.blocks(a7)[1].header().hash;
        assert!(
            matches!(stopped, FollowError::Diverged { index: "block_hash", height, expected, got }
                if height == h(1) && expected == x1.hash && got == a1),
            "{stopped}"
        );
        for queue in [&mut foreign, &mut above] {
            assert!(matches!(queue.next().await, Step::Shutdown), "no step before the stop");
        }

        let (verified, verified_rx) =
            watch::channel(Some(Arc::new(chain.verified_final(a7, h(5)))));
        let mut follower = FinalFollower::new(verified_rx, balancer, NonZeroUsize::MIN);
        let mut fresh = follower.subscribe(IndexKind::TreeState, None, NonZeroUsize::MAX);
        let running = tokio::spawn(follower.run(CancellationToken::new()));
        let mut sent = Vec::new();
        for _ in 0..=5 {
            let data = match fresh.next().await {
                Step::Apply { data, .. } | Step::Finalized { data, .. } => data,
                Step::Shutdown => panic!("Shutdown early"),
            };
            sent.push(data.at());
        }
        let trunk: Vec<BlockRef> = chain.blocks(a7)[..=5].iter().map(|block| block.at()).collect();
        assert_eq!(sent, trunk, "A0..=A5");
        validator.follow(&chain, b11);
        verified.send_replace(Some(Arc::new(chain.verified_final(b11, h(8)))));
        let stopped = running.await.expect("follower task").expect_err("B6 does not extend A5");
        assert!(matches!(stopped, FollowError::Unlinked { height } if height == h(6)), "{stopped}");
        assert!(matches!(fresh.next().await, Step::Shutdown), "nothing past A5 sent");
        stop_balancing.cancel();
    }
}
