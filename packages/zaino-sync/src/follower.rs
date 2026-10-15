//! [`FinalizedFollower`]: every finalized block, in order, onto the finalized stream (`usage.md`)
//!
//! - Target = the chain's finalized tip height
//! - A height the verified chain holds a header for: by hash, from any member (peers included),
//!   checked against that header; below the headers held (bulk): by height from a trusted
//!   member, batched (trusted = trusted)
//! - Starts after the lowest durable tip (an index ahead skips what it holds)
//! - Each block's parent = the block sent before it, and = the durable tip of each index it
//!   extends (else `Unlinked` / `Diverged`: stop, never skip)
//! - Bulk and tip alike: each block once, after it is finalized (the NFS never sends)
//! - The block at the chain's finalized tip = `FinalizedTip` (writers commit after it, else on a
//!   full buffer)

use std::num::NonZeroUsize;
use std::sync::Arc;

use futures::stream::{FuturesOrdered, StreamExt};
use tokio::sync::watch;
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};
use zaino_header_chain::VerifiedChain;
use zaino_persistence::IndexKind;
use zaino_primitives::types::{Block, BlockRef};
use zaino_source::{ChainDataSource, ResponseSizeCap};
use zaino_traffic::{TrafficBalancer, Urgency};
use zcash_protocol::consensus::{BlockHeight, H0};
use zcash_protocol::BlockHash;

use crate::{emit, fetch, fetch_at, FinalizedStream, Step, Subscription, SyncProgress};

#[derive(Debug, thiserror::Error)]
pub enum FollowError {
    #[error(
        "{index} committed {expected} at {height:?}, the trusted validator's chain has {got} \
         (resync required)"
    )]
    Diverged { index: &'static str, height: BlockHeight, expected: BlockHash, got: BlockHash },
    #[error("block {height:?} does not extend the block sent before it (validator history moved)")]
    Unlinked { height: BlockHeight },
    #[error("header sync stopped publishing the verified chain")]
    ChainGone,
}

pub struct FinalizedFollower<S> {
    chain: watch::Receiver<Option<Arc<VerifiedChain>>>,
    balancer: TrafficBalancer<S>,
    lookahead: NonZeroUsize,
    sink: FinalizedStream<Block>,
    durable: Vec<(IndexKind, Option<BlockRef>)>,
    progress: SyncProgress,
}

impl<S: ChainDataSource> FinalizedFollower<S> {
    /// `lookahead` = blocks in flight ahead of the next block sent, by hash and by height alike
    pub fn new(
        chain: watch::Receiver<Option<Arc<VerifiedChain>>>,
        balancer: TrafficBalancer<S>,
        lookahead: NonZeroUsize,
    ) -> Self {
        Self {
            chain,
            balancer,
            lookahead,
            sink: FinalizedStream::new(),
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

    /// - Cancel → `Ok` (the chain gone after it included: header sync stops on the same cancel)
    /// - Either way: the stream ends with `Shutdown` (every writer commits, stops)
    pub async fn run(self, cancel: CancellationToken) -> Result<(), FollowError> {
        let Self { chain, balancer, lookahead, sink, durable, progress } = self;
        let followed = cancel
            .run_until_cancelled(follow(chain, &balancer, lookahead, &sink, durable, &progress))
            .await;
        sink.shutdown();
        match followed {
            Some(Err(FollowError::ChainGone)) if cancel.is_cancelled() => Ok(()),
            followed => followed.unwrap_or(Ok(())),
        }
    }
}

async fn follow<S: ChainDataSource>(
    mut chain: watch::Receiver<Option<Arc<VerifiedChain>>>,
    balancer: &TrafficBalancer<S>,
    lookahead: NonZeroUsize,
    sink: &FinalizedStream<Block>,
    durable: Vec<(IndexKind, Option<BlockRef>)>,
    progress: &SyncProgress,
) -> Result<(), FollowError> {
    let next = |tip: &Option<BlockRef>| tip.map_or(H0, |tip| tip.height + 1);
    let mut wanted = durable.iter().map(|(_, tip)| next(tip)).min().unwrap_or(H0);
    let mut last_sent: Option<BlockHash> = None;
    let mut fetching = FuturesOrdered::new();
    let mut in_flight = 0;
    loop {
        let verified = chain.borrow_and_update().clone();
        let finalized_height = verified.as_ref().map(|chain| chain.finalized_tip().height);
        let header_at = |height| verified.as_ref().and_then(|chain| chain.header_at(height));
        while in_flight < lookahead.get() && Some(wanted) <= finalized_height {
            // own task: fetches progress while `send` waits on a full queue (else every in-flight
            // reply sits unread); abort on drop: no fetch outlives the follower
            let fetched = match header_at(wanted) {
                Some(record) => {
                    let at = BlockRef { hash: record.hash, height: wanted };
                    wanted = wanted + 1;
                    in_flight += 1;
                    let one = fetch(balancer.clone(), at, record, Urgency::Bulk);
                    tokio::spawn(async move { vec![one.await] })
                }
                None => {
                    let room = BLOCKS_PER_REQUEST.min(lookahead.get() - in_flight);
                    let mut heights = Vec::with_capacity(room);
                    // stops at the first held header (from there: by hash, checked)
                    while heights.len() < room
                        && Some(wanted) <= finalized_height
                        && header_at(wanted).is_none()
                    {
                        heights.push(wanted);
                        wanted = wanted + 1;
                    }
                    in_flight += heights.len();
                    tokio::spawn(fetch_at(balancer.clone(), heights.into(), Urgency::Bulk))
                }
            };
            fetching.push_back(AbortOnDropHandle::new(fetched));
        }
        tokio::select! {
            changed = chain.changed() => changed.map_err(|_| FollowError::ChainGone)?,
            Some(joined) = fetching.next(), if !fetching.is_empty() => {
                let bodies = joined.unwrap_or_else(|join| std::panic::resume_unwind(join.into_panic()));
                in_flight -= bodies.len();
                for body in bodies {
                    let block = Arc::clone(body.block());
                    link(&durable, last_sent, &block)?;
                    let height = block.header().height;
                    last_sent = Some(block.header().hash);
                    emit::handed(&block);
                    progress.hand(height);
                    let finalized_tip =
                        chain.borrow().as_ref().map(|chain| chain.finalized_tip().height);
                    let step = match Some(height) == finalized_tip {
                        true => Step::FinalizedTip(block),
                        false => Step::Apply(block),
                    };
                    sink.send(step).await;
                }
            }
        }
    }
}

/// Blocks per by-height request: what one reply at the largest cap holds (a smaller cap = the
/// source splits it)
const BLOCKS_PER_REQUEST: usize = ResponseSizeCap::MAX.blocks_per_batch().get();

/// `block`'s parent = each durable tip it extends (an index's own chain), then = the block sent
/// before it (the validator's history unmoved)
fn link(
    durable: &[(IndexKind, Option<BlockRef>)],
    last_sent: Option<BlockHash>,
    block: &Block,
) -> Result<(), FollowError> {
    let (height, parent) = (block.header().height, block.header().prev_hash);
    for (kind, tip) in durable {
        let Some(tip) = tip.filter(|tip| tip.height + 1 == height) else { continue };
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
    use zaino_source::testing::{within, Lie, ValidatorViews};
    use zaino_traffic::{Limits, TrustedValidator};

    use super::*;

    /// Chain A 0..=11, finalized 5 then 8; an honest member + a `Lie::Poisoned` liar (by height
    /// too); two subscribers, one durable at A2, one fresh:
    /// - both queues = A0..=A8, once each, in order, the honest bodies (a lie never sent)
    /// - the finalized tip's block (A5, then A8) `FinalizedTip`, every other `Apply`
    /// - nothing above the finalized tip; progress = the last height sent, blocks counted
    /// - cancel → `Ok`, `Shutdown` last in every queue
    #[tokio::test(start_paused = true)]
    async fn finalized_blocks_reach_every_subscriber_once_in_order_from_the_lowest_durable_tip() {
        let mut chain = MockChain::regtest();
        let a11 = chain.mine_empty(11);
        let trunk = chain.blocks(a11);
        let members = [None, Some(Lie::Poisoned)].map(|lie| {
            let member = chain.validator(a11);
            member.lie(lie);
            Arc::new(member)
        });
        let limits = Limits::new(8).expect("8 ≥ MIN_CONNECTIONS");
        let trusted = members.iter().enumerate().map(|(index, member)| TrustedValidator {
            address: format!("member{index}"),
            source: Arc::clone(member),
            priority: 0,
            limits,
        });
        let (balancer, balancing) = TrafficBalancer::new(trusted.collect(), None);
        let stop_balancing = CancellationToken::new();
        tokio::spawn(balancing.run(stop_balancing.clone()));
        let finalized_at_five = Arc::new(chain.verified_finalized(a11, h(5)));
        let (verified, verified_rx) = watch::channel(Some(finalized_at_five));

        let lookahead = NonZeroUsize::new(3).expect("nonzero");
        let mut follower = FinalizedFollower::new(verified_rx, balancer, lookahead);
        let durable_tip_a2 = Some(trunk[2].block_ref());
        let mut durable_at_a2 =
            follower.subscribe(IndexKind::BlockHash, durable_tip_a2, NonZeroUsize::MAX);
        let mut fresh = follower.subscribe(IndexKind::TreeState, None, NonZeroUsize::MAX);
        let progress = follower.progress();
        let cancel = CancellationToken::new();
        let running = tokio::spawn(follower.run(cancel.clone()));
        let sent = async |queue: &mut Subscription<Block>, count: usize| {
            let mut sent = Vec::new();
            for _ in 0..count {
                let (data, finalized) = match within("a step", queue.next()).await {
                    Step::Apply(data) => (data, false),
                    Step::FinalizedTip(data) => (data, true),
                    Step::Shutdown => panic!("Shutdown early"),
                };
                sent.push((data.block_ref(), finalized));
            }
            sent
        };
        let steps = |range: std::ops::RangeInclusive<usize>, finalized_tip: usize| -> Vec<_> {
            range.map(|at| (trunk[at].block_ref(), at == finalized_tip)).collect()
        };

        let first = steps(0..=5, 5);
        assert_eq!(sent(&mut durable_at_a2, 6).await, first, "from the lowest durable tip (fresh)");
        assert_eq!(sent(&mut fresh, 6).await, first, "the same steps to every subscriber");
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        assert_eq!(progress.handed(), Some(h(5)), "nothing above the finalized tip");

        verified.send_replace(Some(Arc::new(chain.verified_finalized(a11, h(8)))));
        let next = steps(6..=8, 8);
        assert_eq!(
            sent(&mut durable_at_a2, 3).await,
            next,
            "the finalized tip moved: its blocks next"
        );
        assert_eq!(sent(&mut fresh, 3).await, next);
        assert_eq!((progress.handed(), progress.blocks()), (Some(h(8)), 9));

        cancel.cancel();
        let stopped = within("the follower stopped", running).await;
        stopped.expect("follower task").expect("cancel = clean stop");
        for queue in [&mut durable_at_a2, &mut fresh] {
            let last = within("Shutdown", queue.next()).await;
            assert!(matches!(last, Step::Shutdown), "Shutdown last");
        }
        stop_balancing.cancel();
    }

    /// Broken links stop the follower, naming what broke:
    /// - durable X1 (A1's sibling), finalized 5: A2's parent ≠ X1 → `Diverged` naming the index,
    ///   before any step
    /// - A0..=A5 sent, then the validator's history moves (B 5..=11 off A4), finalized 8: B6's
    ///   parent ≠ A5 → `Unlinked` at 6, nothing past A5 sent
    #[tokio::test(start_paused = true)]
    async fn a_block_off_the_chain_already_sent_or_committed_stops_the_follower() {
        let mut chain = MockChain::regtest();
        let a7 = chain.mine_empty(7);
        let x1 = chain.fork(h(0)).mine_empty(1).tip();
        let b11 = chain.fork(h(4)).mine_empty(7).tip();
        let limits = Limits::new(8).expect("8 ≥ MIN_CONNECTIONS");
        let validator = Arc::new(chain.validator(a7));
        let (address, source) = ("validator".to_owned(), Arc::clone(&validator));
        let trusted = vec![TrustedValidator { address, source, priority: 0, limits }];
        let (balancer, balancing) = TrafficBalancer::new(trusted, None);
        let stop_balancing = CancellationToken::new();
        tokio::spawn(balancing.run(stop_balancing.clone()));

        let finalized_at_five = Some(Arc::new(chain.verified_finalized(a7, h(5))));
        let (_verified, verified_rx) = watch::channel(finalized_at_five);
        let mut follower = FinalizedFollower::new(verified_rx, balancer.clone(), NonZeroUsize::MIN);
        let durable_tip_a4 = Some(chain.blocks(a7)[4].block_ref());
        let mut above = follower.subscribe(IndexKind::TreeState, durable_tip_a4, NonZeroUsize::MAX);
        let mut foreign = follower.subscribe(IndexKind::BlockHash, Some(x1), NonZeroUsize::MAX);
        let stopped = within("the follower stopped", follower.run(CancellationToken::new())).await;
        let stopped = stopped.expect_err("X1 off the validator's chain");
        let a1 = chain.blocks(a7)[1].header().hash;
        assert!(
            matches!(stopped, FollowError::Diverged { index: "block_hash", height, expected, got }
                if height == h(1) && expected == x1.hash && got == a1),
            "{stopped}"
        );
        for queue in [&mut foreign, &mut above] {
            let first = within("Shutdown", queue.next()).await;
            assert!(matches!(first, Step::Shutdown), "no step before the stop");
        }

        let (verified, verified_rx) =
            watch::channel(Some(Arc::new(chain.verified_finalized(a7, h(5)))));
        let mut follower = FinalizedFollower::new(verified_rx, balancer, NonZeroUsize::MIN);
        let mut fresh = follower.subscribe(IndexKind::TreeState, None, NonZeroUsize::MAX);
        let running = tokio::spawn(follower.run(CancellationToken::new()));
        let mut sent = Vec::new();
        for _ in 0..=5 {
            let data = match within("a step", fresh.next()).await {
                Step::Apply(data) | Step::FinalizedTip(data) => data,
                Step::Shutdown => panic!("Shutdown early"),
            };
            sent.push(data.block_ref());
        }
        let trunk: Vec<BlockRef> =
            chain.blocks(a7)[..=5].iter().map(|block| block.block_ref()).collect();
        assert_eq!(sent, trunk, "A0..=A5");
        validator.follow(&chain, b11);
        verified.send_replace(Some(Arc::new(chain.verified_finalized(b11, h(8)))));
        let stopped = within("the follower stopped", running).await.expect("follower task");
        let stopped = stopped.expect_err("B6 does not extend A5");
        assert!(matches!(stopped, FollowError::Unlinked { height } if height == h(6)), "{stopped}");
        let last = within("Shutdown", fresh.next()).await;
        assert!(matches!(last, Step::Shutdown), "nothing past A5 sent");
        stop_balancing.cancel();
    }
}

#[cfg(test)]
mod peers {
    use std::time::Duration;

    use zaino_header_chain::testing::HeaderViews;
    use zaino_primitives::testing::{h, MockChain};
    use zaino_source::testing::{Port, ValidatorViews};
    use zaino_traffic::testing::MockPeerSet;
    use zaino_traffic::{Limits, Peers, TrustedValidator};

    use super::*;

    /// The trusted validator unreachable throughout, the peer set holding A0..=A11; finalized 8 on
    /// a chain naming every header: A0..=A8 reach the stream from the peer set alone (each checked
    /// against its verified header; the validator, asked first, never answers)
    #[tokio::test(start_paused = true)]
    async fn finalized_blocks_the_verified_chain_names_come_from_peers_when_no_validator_answers() {
        let mut chain = MockChain::regtest();
        let a11 = chain.mine_empty(11);
        let trunk = chain.blocks(a11);
        let validator = Arc::new(chain.validator(a11));
        validator.reachable(&Port::ALL, false);
        let limits = Limits::new(8).expect("8 ≥ MIN_CONNECTIONS");
        let (address, source) = ("validator".to_owned(), Arc::clone(&validator));
        let trusted = vec![TrustedValidator { address, source, priority: 0, limits }];
        let peers = Peers { transport: Arc::new(MockPeerSet::new(&chain, a11)), in_flight: 4 };
        let (balancer, balancing) = TrafficBalancer::new(trusted, Some(peers));
        let cancel = CancellationToken::new();
        tokio::spawn(balancing.run(cancel.clone()));
        let finalized_at_eight = Some(Arc::new(chain.verified_finalized(a11, h(8))));
        let (_verified, verified_rx) = watch::channel(finalized_at_eight);
        let lookahead = NonZeroUsize::new(3).expect("nonzero");
        let mut follower = FinalizedFollower::new(verified_rx, balancer, lookahead);
        let mut queue = follower.subscribe(IndexKind::BlockHash, None, NonZeroUsize::MAX);
        tokio::spawn(follower.run(cancel.clone()));

        let mut sent = Vec::new();
        for _ in 0..=8 {
            let step = tokio::time::timeout(Duration::from_secs(60), queue.next()).await;
            match step.expect("a finalized block within a minute") {
                Step::Apply(data) | Step::FinalizedTip(data) => sent.push(data.block_ref()),
                Step::Shutdown => panic!("Shutdown early"),
            }
        }
        let expected: Vec<BlockRef> = trunk[..=8].iter().map(|block| block.block_ref()).collect();
        assert_eq!(sent, expected, "A0..=A8 in order, served while the validator never answered");
        cancel.cancel();
    }

    /// Headers held from A3 (anchored there), finalized 8; the trusted validator on B (B3.. off A2),
    /// the peer set on A; lookahead past one by-height batch:
    /// - A0..=A2 by height (below the headers: the validator's, shared by both chains)
    /// - A3..=A8 by hash, checked: the batch stops at A3 (by height they would be B's)
    #[tokio::test(start_paused = true)]
    async fn a_by_height_batch_stops_where_the_verified_headers_begin() {
        use std::num::NonZeroU32;

        use zaino_header_chain::{decode_header, testing::insert, HeaderChain};
        use zaino_primitives::types::FinalityDepth;

        let mut chain = MockChain::regtest();
        let a11 = chain.mine_empty(11);
        let trunk = chain.blocks(a11);
        let b11 = chain.fork(h(2)).mine_empty(9).tip();
        let validator = Arc::new(chain.validator(b11));
        let limits = Limits::new(8).expect("8 ≥ MIN_CONNECTIONS");
        let (address, source) = ("validator".to_owned(), Arc::clone(&validator));
        let trusted = vec![TrustedValidator { address, source, priority: 0, limits }];
        let peers = Peers { transport: Arc::new(MockPeerSet::new(&chain, a11)), in_flight: 4 };
        let (balancer, balancing) = TrafficBalancer::new(trusted, Some(peers));
        let cancel = CancellationToken::new();
        tokio::spawn(balancing.run(cancel.clone()));

        let depth = FinalityDepth::new(NonZeroU32::new(3).expect("non-zero"));
        let mut headers = HeaderChain::new(depth);
        let a3 = decode_header(&chain.header_bytes(trunk[3].header().hash)).expect("decodes");
        headers.anchor(&a3, h(3));
        insert(&mut headers, &trunk[4..]).expect("A4..=A11 link onto A3");
        headers.finalize(headers.finalizable().expect("A11 = A8 + depth"));
        let verified = headers.verified().expect("anchored");
        assert_eq!(
            (verified.header_at(h(2)), verified.finalized_tip()),
            (None, trunk[8].block_ref())
        );
        let (_verified, verified_rx) = watch::channel(Some(Arc::new(verified)));
        let lookahead = NonZeroUsize::new(16).expect("nonzero");
        let mut follower = FinalizedFollower::new(verified_rx, balancer, lookahead);
        let mut queue = follower.subscribe(IndexKind::BlockHash, None, NonZeroUsize::MAX);
        tokio::spawn(follower.run(cancel.clone()));

        let mut sent = Vec::new();
        for _ in 0..=8 {
            let step = tokio::time::timeout(Duration::from_secs(60), queue.next()).await;
            match step.expect("a finalized block within a minute") {
                Step::Apply(data) | Step::FinalizedTip(data) => sent.push(data.block_ref()),
                Step::Shutdown => panic!("Shutdown early"),
            }
        }
        let expected: Vec<BlockRef> = trunk[..=8].iter().map(|block| block.block_ref()).collect();
        assert_eq!(sent, expected, "A0..=A8: the verified chain's, none of B's");
        cancel.cancel();
    }
}
