//! [`FinalFollower`]: every final block, fetched in order, onto the final stream (`data-sink.md`)
//!
//! - Starts after the lowest durable tip (an index ahead skips what it holds)
//! - Nothing sent until every durable tip = the final chain's block at its height (else resync)
//! - Bulk and tip alike: each block once, after it turns final (the NFS never sends)

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

use crate::{emit, fetch, IndexerDataSink, Step, Subscription, SyncProgress};

#[derive(Debug, thiserror::Error)]
pub enum FollowError {
    #[error(
        "{index} committed {expected} at {height:?}, the verified chain has {got} (resync \
         required)"
    )]
    Diverged { index: &'static str, height: Height, expected: BlockHash, got: BlockHash },
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
    /// `lookahead` = fetches in flight ahead of the next block sent
    pub fn new(
        chain: watch::Receiver<Option<Arc<VerifiedChain>>>,
        balancer: TrafficBalancer<S>,
        lookahead: NonZeroUsize,
    ) -> Self {
        Self {
            chain,
            balancer,
            lookahead,
            sink: IndexerDataSink::new("final"),
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
    mut unconfirmed: Vec<(IndexKind, Option<BlockRef>)>,
    progress: &SyncProgress,
) -> Result<(), FollowError> {
    let next = |tip: &Option<BlockRef>| tip.map_or(Height::GENESIS, |tip| tip.height.next());
    let mut wanted = unconfirmed.iter().map(|(_, tip)| next(tip)).min().unwrap_or(Height::GENESIS);
    let mut fetching = FuturesOrdered::new();
    loop {
        if let Some(verified) = chain.borrow_and_update().clone() {
            confirm(&verified, &mut unconfirmed)?;
            let final_height = verified.final_tip().map(|tip| tip.height);
            while unconfirmed.is_empty()
                && fetching.len() < lookahead.get()
                && Some(wanted) <= final_height
            {
                let hash = verified.hash_at(wanted).expect("at or below the final tip");
                let record = verified.header_at(wanted).expect("at or below the final tip");
                let at = BlockRef { hash, height: wanted };
                fetching.push_back(fetch(balancer.clone(), at, record, Urgency::Bulk));
                wanted = wanted.next();
            }
        }
        tokio::select! {
            changed = chain.changed() => changed.map_err(|_| FollowError::ChainGone)?,
            Some(body) = fetching.next(), if !fetching.is_empty() => {
                let (height, block) = (body.at().height, Arc::clone(body.block()));
                emit::handed(&block);
                progress.hand(height);
                sink.send(Step::Apply { height, data: block }).await;
            }
        }
    }
}

/// Durable tips at or below the final tip: each the final chain's block there, then dropped
fn confirm(
    chain: &VerifiedChain,
    unconfirmed: &mut Vec<(IndexKind, Option<BlockRef>)>,
) -> Result<(), FollowError> {
    let final_height = chain.final_tip().map(|tip| tip.height);
    let mut diverged = None;
    unconfirmed.retain(|(kind, tip)| {
        let Some(tip) = tip else { return false };
        if Some(tip.height) > final_height {
            return true;
        }
        let got = chain.hash_at(tip.height).expect("at or below the final tip");
        if got != tip.hash {
            let (index, height, expected) = (kind.name(), tip.height, tip.hash);
            diverged.get_or_insert(FollowError::Diverged { index, height, expected, got });
        }
        false
    });
    diverged.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use zaino_header_chain::testing::{insert, HeaderViews};
    use zaino_primitives::testing::{h, MockChain};
    use zaino_primitives::types::ReorgDepth;
    use zaino_source::testing::{Lie, MockValidator};
    use zaino_traffic::{Limits, Trusted};

    use super::*;

    const DEPTH: ReorgDepth = ReorgDepth::new(NonZeroU32::new(3).expect("nonzero"));

    /// Chain A 0..=8 (final 5), then A 9..=11 (final 8); an honest member + a `Lie::Poisoned` liar;
    /// two subscribers, one durable at A2, one fresh:
    /// - both queues = A0..=A8, once each, in order, the honest bodies (a lie never sent)
    /// - nothing above the final tip; progress = the last height sent, blocks counted
    /// - cancel → `Ok`, `Shutdown` last in every queue
    #[tokio::test(start_paused = true)]
    async fn every_final_block_reaches_every_subscriber_once_in_order_from_the_lowest_durable_tip()
    {
        let mut chain = MockChain::regtest();
        let a8 = chain.mine_empty(8);
        let mut headers = chain.header_chain(DEPTH);
        insert(&mut headers, &chain.blocks(a8)).expect("valid headers");
        headers.finalize(headers.finalizable().expect("8 − 3")).expect("in-memory store");
        let members = [None, Some(Lie::Poisoned)].map(|lie| {
            let member = MockValidator::following(&chain, a8);
            member.lie(lie);
            Arc::new(member)
        });
        let limits = Limits::new(8, None).expect("8 ≥ MIN_CONNECTIONS");
        let trusted = members.iter().map(|member| Trusted {
            source: Arc::clone(member),
            priority: 0,
            limits,
        });
        let (balancer, balancing) = TrafficBalancer::new(trusted.collect(), None);
        let stop_balancing = CancellationToken::new();
        tokio::spawn(balancing.run(stop_balancing.clone()));
        let (verified, verified_rx) = watch::channel(headers.verified().map(Arc::new));

        let lookahead = NonZeroUsize::new(3).expect("nonzero");
        let mut follower = FinalFollower::new(verified_rx, balancer, lookahead);
        let a2 = chain.blocks(a8)[2].at();
        let mut durable_at_a2 =
            follower.subscribe(IndexKind::BlockHash, Some(a2), NonZeroUsize::MAX);
        let mut fresh = follower.subscribe(IndexKind::TreeState, None, NonZeroUsize::MAX);
        let progress = follower.progress();
        let cancel = CancellationToken::new();
        let running = tokio::spawn(follower.run(cancel.clone()));
        let sent = async |queue: &mut Subscription<Block>, count: usize| {
            let mut sent = Vec::new();
            for _ in 0..count {
                let Step::Apply { height, data } = queue.next().await else {
                    panic!("Shutdown early")
                };
                assert_eq!(data.header().height, height, "step height = its block's");
                sent.push(data.at());
            }
            sent
        };

        let first: Vec<BlockRef> = chain.blocks(a8)[..=5].iter().map(|block| block.at()).collect();
        assert_eq!(sent(&mut durable_at_a2, 6).await, first, "from the lowest durable tip (fresh)");
        assert_eq!(sent(&mut fresh, 6).await, first, "the same steps to every subscriber");
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        assert_eq!(progress.handed(), Some(h(5)), "nothing above the final tip");

        let a11 = chain.mine_empty(3);
        insert(&mut headers, &chain.blocks(a11)[9..]).expect("valid headers");
        headers.finalize(headers.finalizable().expect("11 − 3")).expect("in-memory store");
        members.iter().for_each(|member| member.follow(&chain, a11));
        verified.send_replace(headers.verified().map(Arc::new));
        let next: Vec<BlockRef> = chain.blocks(a11)[6..=8].iter().map(|block| block.at()).collect();
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

    /// Durable X1 (A1's sibling) under final A2: `Diverged` naming the index, before any step;
    /// a durable tip above the final tip waits instead (a lost header store catching up)
    #[tokio::test(start_paused = true)]
    async fn a_durable_tip_off_the_final_chain_stops_the_follower_before_any_step() {
        let mut chain = MockChain::regtest();
        let a5 = chain.mine_empty(5);
        let x1 = chain.fork(h(0)).mine_empty(1).tip();
        let mut headers = chain.header_chain(DEPTH);
        insert(&mut headers, &chain.blocks(a5)).expect("valid headers");
        headers.finalize(headers.finalizable().expect("5 − 3")).expect("in-memory store");
        let (_verified, verified_rx) = watch::channel(headers.verified().map(Arc::new));
        let limits = Limits::new(8, None).expect("8 ≥ MIN_CONNECTIONS");
        let source = Arc::new(MockValidator::following(&chain, a5));
        let (balancer, _never_driven) =
            TrafficBalancer::new(vec![Trusted { source, priority: 0, limits }], None);

        let mut follower = FinalFollower::new(verified_rx, balancer, NonZeroUsize::MIN);
        let a4 = chain.blocks(a5)[4].at();
        let mut above_final = follower.subscribe(IndexKind::TreeState, Some(a4), NonZeroUsize::MAX);
        let mut foreign = follower.subscribe(IndexKind::BlockHash, Some(x1), NonZeroUsize::MAX);
        let stopped =
            follower.run(CancellationToken::new()).await.expect_err("X1 off the final chain");
        let a1 = chain.blocks(a5)[1].header().hash;
        assert!(
            matches!(stopped, FollowError::Diverged { index: "block_hash", height, expected, got }
                if u32::from(height) == 1 && expected == x1.hash && got == a1),
            "{stopped}"
        );
        for queue in [&mut foreign, &mut above_final] {
            assert!(matches!(queue.next().await, Step::Shutdown), "no step before the stop");
        }
    }
}
