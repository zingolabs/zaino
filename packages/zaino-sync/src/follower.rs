//! [`FinalizedFollower`]: every finalized block, in order, onto the finalized stream (`usage.md`)
//!
//! - Target = the chain's finalized tip height
//! - A height the verified chain holds a header for: by hash, from any member (peers included),
//!   checked against that header
//! - Below the headers held (bulk): by height from a trusted member, batched
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
use zaino_header_chain::HeaderChainSnapshot;
use zaino_persistence::IndexKind;
use zaino_primitives::types::{Block, BlockRef};
use zaino_source::{ChainDataSource, ResponseSizeCap};
use zaino_traffic::{TrafficBalancer, Urgency};
use zcash_protocol::consensus::{BlockHeight, H0};
use zcash_protocol::BlockHash;

use crate::{fetch, fetch_at, metrics, FinalizedStream, Step, Subscription, SyncProgress};

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
    chain: watch::Receiver<Option<Arc<HeaderChainSnapshot>>>,
    balancer: TrafficBalancer<S>,
    lookahead: NonZeroUsize,
    sink: FinalizedStream<Block>,
    durable: Vec<(IndexKind, Option<BlockRef>)>,
    progress: SyncProgress,
}

impl<S: ChainDataSource> FinalizedFollower<S> {
    /// `lookahead` = blocks in flight ahead of the next block sent, by hash and by height alike
    pub fn new(
        chain: watch::Receiver<Option<Arc<HeaderChainSnapshot>>>,
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

    /// `kind`'s stream (`durable` = its store's durable tip, `queue` = the stream's byte budget)
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
    mut chain: watch::Receiver<Option<Arc<HeaderChainSnapshot>>>,
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
            // - own task: fetches progress while `send` waits on a full queue (else replies unread)
            // - abort on drop: no fetch outlives the follower
            let fetched = match header_at(wanted) {
                Some(record) => {
                    let at = BlockRef { hash: record.hash, height: wanted };
                    wanted = wanted + 1;
                    in_flight += 1;
                    let one = fetch(balancer.clone(), at, Urgency::Bulk);
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
                    metrics::handed(&block);
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

/// Balancer over mock validators + finalized-stream readers (infra: nothing asserted on here)
#[cfg(test)]
mod test_kit {
    use zaino_source::testing::{within, MockValidator};
    use zaino_source::ZcashLegacyRpcClient;
    use zaino_traffic::{Limits, Peers, TrustedValidator};

    use super::*;

    /// `trusted` (one priority) + `peers`, balancing until `stop`
    pub(super) fn balancer(
        trusted: &[&MockValidator],
        peers: Option<Peers>,
        stop: &CancellationToken,
    ) -> TrafficBalancer<ZcashLegacyRpcClient> {
        let limits = Limits::new(8).expect("8 ≥ MIN_CONNECTIONS");
        let trusted = trusted.iter().enumerate().map(|(index, validator)| TrustedValidator {
            address: format!("validator{index}"),
            source: Arc::new(validator.client()),
            priority: 0,
            limits,
        });
        let (balancer, balancing) = TrafficBalancer::new(trusted.collect(), peers);
        tokio::spawn(balancing.run(stop.clone()));
        balancer
    }

    #[derive(Debug, PartialEq, Eq)]
    pub(super) struct Sent {
        pub(super) block: BlockRef,
        pub(super) transactions: usize,
        pub(super) finalized_tip: bool,
    }

    impl Sent {
        pub(super) fn of(block: &Block, finalized_tip: bool) -> Self {
            Self {
                block: block.block_ref(),
                transactions: block.transactions().len(),
                finalized_tip,
            }
        }
    }

    /// Next `count` steps off `queue` (an early `Shutdown` panics naming what came before it)
    pub(super) async fn received(queue: &mut Subscription<Block>, count: usize) -> Vec<Sent> {
        let mut sent = Vec::new();
        for _ in 0..count {
            match within("a step", queue.next()).await {
                Step::Apply(block) => sent.push(Sent::of(&block, false)),
                Step::FinalizedTip(block) => sent.push(Sent::of(&block, true)),
                Step::Shutdown => panic!("Shutdown after {sent:?}, {count} steps expected"),
            }
        }
        sent
    }

    pub(super) async fn assert_shutdown_next(queue: &mut Subscription<Block>, why: &str) {
        match within("Shutdown", queue.next()).await {
            Step::Shutdown => {}
            Step::Apply(block) | Step::FinalizedTip(block) => {
                panic!("{why}: {:?} sent before Shutdown", block.block_ref())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ops::RangeInclusive;
    use std::time::Duration;

    use zaino_source::testing::{h, within, Lie, MockChain};

    use super::test_kit::{assert_shutdown_next, balancer, received, Sent};
    use super::*;

    /// Paused clock: every fetch in flight lands long before this
    const IDLE: Duration = Duration::from_secs(60);

    /// Chain A0..=A11, finalized 5 then 8, honest validator + `Lie::Poisoned` liar, subscribers
    /// durable at A2 and fresh
    ///
    /// - both queues = A0..=A8 once each, in order, honest bodies, finalized tip's `FinalizedTip`
    /// - nothing above the finalized tip, progress = last height sent
    /// - cancel → `Ok`, `Shutdown` last
    #[tokio::test(start_paused = true)]
    async fn finalized_blocks_reach_every_subscriber_once_in_order_from_the_lowest_durable_tip() {
        let mut chain = MockChain::regtest();
        let a11 = chain.mine_empty(11);
        let trunk = chain.blocks(a11);
        let (honest, liar) = (chain.validator(a11), chain.validator(a11));
        liar.lie(Some(Lie::Poisoned));
        let balancer = balancer(&[&honest, &liar], None, &CancellationToken::new());
        let finalized_at_five = HeaderChainSnapshot::finalized(&chain, a11, h(5));
        let (verified, verified_rx) = watch::channel(Some(Arc::new(finalized_at_five)));
        let lookahead = NonZeroUsize::new(3).expect("nonzero");
        let mut follower = FinalizedFollower::new(verified_rx, balancer, lookahead);
        let a2 = Some(trunk[2].block_ref());
        let mut durable_at_a2 = follower.subscribe(IndexKind::BlockHash, a2, NonZeroUsize::MAX);
        let mut fresh = follower.subscribe(IndexKind::TreeState, None, NonZeroUsize::MAX);
        let progress = follower.progress();
        let cancel = CancellationToken::new();
        let running = tokio::spawn(follower.run(cancel.clone()));
        let trunk_steps = |range: RangeInclusive<usize>, finalized_tip: usize| -> Vec<Sent> {
            range.map(|at| Sent::of(&trunk[at], at == finalized_tip)).collect()
        };

        let to_five = trunk_steps(0..=5, 5);
        let got = received(&mut durable_at_a2, 6).await;
        assert_eq!(got, to_five, "durable at A2: from the lowest durable tip (fresh's)");
        assert_eq!(received(&mut fresh, 6).await, to_five, "fresh: the same steps");
        tokio::time::sleep(IDLE).await;
        assert_eq!(progress.handed(), Some(h(5)), "idle at finalized 5: nothing above it");

        let finalized_at_eight = HeaderChainSnapshot::finalized(&chain, a11, h(8));
        verified.send_replace(Some(Arc::new(finalized_at_eight)));
        let six_to_eight = trunk_steps(6..=8, 8);
        let got = received(&mut durable_at_a2, 3).await;
        assert_eq!(got, six_to_eight, "durable at A2: finalized tip moved to 8");
        assert_eq!(received(&mut fresh, 3).await, six_to_eight, "fresh: finalized tip moved to 8");
        assert_eq!(progress.handed(), Some(h(8)), "progress = last height sent");
        assert_eq!(progress.blocks(), 9, "A0..=A8 counted once each");

        cancel.cancel();
        let stopped = within("the follower stopped", running).await.expect("follower task");
        assert!(matches!(stopped, Ok(())), "cancel = clean stop: {stopped:?}");
        assert_shutdown_next(&mut durable_at_a2, "durable at A2: Shutdown last").await;
        assert_shutdown_next(&mut fresh, "fresh: Shutdown last").await;
    }

    /// block_hash durable at X1 (A1's sibling), validator on A, finalized 5: A2's parent A1 ≠ X1
    #[tokio::test(start_paused = true)]
    async fn a_durable_tip_off_the_validators_chain_stops_the_follower_before_any_step() {
        let mut chain = MockChain::regtest();
        let a7 = chain.mine_empty(7);
        let x1 = chain.fork(h(0)).mine_empty(1).tip();
        let validator = chain.validator(a7);
        let balancer = balancer(&[&validator], None, &CancellationToken::new());
        let finalized_at_five = HeaderChainSnapshot::finalized(&chain, a7, h(5));
        let (_verified, verified_rx) = watch::channel(Some(Arc::new(finalized_at_five)));
        let mut follower = FinalizedFollower::new(verified_rx, balancer, NonZeroUsize::MIN);
        let mut at_x1 = follower.subscribe(IndexKind::BlockHash, Some(x1), NonZeroUsize::MAX);

        let stopped = within("the follower stopped", follower.run(CancellationToken::new())).await;

        let a1 = chain.blocks(a7)[1].header().hash;
        assert!(
            matches!(&stopped, Err(FollowError::Diverged { index: "block_hash", height, expected, got })
                if *height == h(1) && *expected == x1.hash && *got == a1),
            "Diverged naming block_hash at 1, X1 committed, A1 served: {stopped:?}"
        );
        assert_shutdown_next(&mut at_x1, "no step before the stop").await;
    }

    /// A0..=A5 sent, then the validator moves to B (B5..=B11 off A4), finalized 8: B6's parent
    /// B5 ≠ A5
    #[tokio::test(start_paused = true)]
    async fn a_validator_history_moving_under_the_blocks_sent_stops_the_follower() {
        let mut chain = MockChain::regtest();
        let a7 = chain.mine_empty(7);
        let b11 = chain.fork(h(4)).mine_empty(7).tip();
        let validator = chain.validator(a7);
        let balancer = balancer(&[&validator], None, &CancellationToken::new());
        let finalized_at_five = HeaderChainSnapshot::finalized(&chain, a7, h(5));
        let (verified, verified_rx) = watch::channel(Some(Arc::new(finalized_at_five)));
        let mut follower = FinalizedFollower::new(verified_rx, balancer, NonZeroUsize::MIN);
        let mut fresh = follower.subscribe(IndexKind::TreeState, None, NonZeroUsize::MAX);
        let running = tokio::spawn(follower.run(CancellationToken::new()));
        let a0_to_a5: Vec<BlockRef> =
            chain.blocks(a7)[..=5].iter().map(|b| b.block_ref()).collect();
        let sent = received(&mut fresh, 6).await.into_iter().map(|sent| sent.block);
        assert_eq!(sent.collect::<Vec<_>>(), a0_to_a5, "finalized 5 on A");

        validator.follow(&chain, b11);
        let b_finalized_at_eight = HeaderChainSnapshot::finalized(&chain, b11, h(8));
        verified.send_replace(Some(Arc::new(b_finalized_at_eight)));

        let stopped = within("the follower stopped", running).await.expect("follower task");
        assert!(
            matches!(stopped, Err(FollowError::Unlinked { height }) if height == h(6)),
            "Unlinked at B6: {stopped:?}"
        );
        assert_shutdown_next(&mut fresh, "nothing past A5 sent").await;
    }
}

#[cfg(test)]
mod peers {
    use std::num::NonZeroU32;

    use zaino_header_chain::{decode_header, testing::insert, HeaderChain};
    use zaino_primitives::types::FinalityDepth;
    use zaino_source::testing::{h, MockChain};
    use zaino_traffic::testing::MockPeerSet;
    use zaino_traffic::Peers;

    use super::test_kit::{balancer, received};
    use super::*;

    /// Trusted validator down throughout, peer set holding A0..=A11, finalized 8 on a chain
    /// naming every header: A0..=A8 from the peer set alone, each checked against its header
    #[tokio::test(start_paused = true)]
    async fn finalized_blocks_the_verified_chain_names_come_from_peers_when_no_validator_answers() {
        let mut chain = MockChain::regtest();
        let a11 = chain.mine_empty(11);
        let validator = chain.validator(a11);
        validator.down(true);
        let peers = Peers { transport: Arc::new(MockPeerSet::new(&chain, a11)), in_flight: 4 };
        let balancer = balancer(&[&validator], Some(peers), &CancellationToken::new());
        let finalized_at_eight = HeaderChainSnapshot::finalized(&chain, a11, h(8));
        let (_verified, verified_rx) = watch::channel(Some(Arc::new(finalized_at_eight)));
        let lookahead = NonZeroUsize::new(3).expect("nonzero");
        let mut follower = FinalizedFollower::new(verified_rx, balancer, lookahead);
        let mut queue = follower.subscribe(IndexKind::BlockHash, None, NonZeroUsize::MAX);
        tokio::spawn(follower.run(CancellationToken::new()));

        let sent: Vec<BlockRef> = received(&mut queue, 9).await.iter().map(|s| s.block).collect();

        let a0_to_a8: Vec<BlockRef> =
            chain.blocks(a11)[..=8].iter().map(|b| b.block_ref()).collect();
        assert_eq!(sent, a0_to_a8, "A0..=A8 in order, served while the validator never answered");
    }

    /// Headers held from A3 (anchored there), finalized 8, trusted validator on B (B3.. off A2),
    /// peer set on A, lookahead past one by-height batch
    ///
    /// - A0..=A2 by height (below the headers: the validator's, shared by both chains)
    /// - A3..=A8 by hash, checked: the batch stops at A3 (by height they would be B's)
    #[tokio::test(start_paused = true)]
    async fn a_by_height_batch_stops_where_the_verified_headers_begin() {
        let mut chain = MockChain::regtest();
        let a11 = chain.mine_empty(11);
        let trunk = chain.blocks(a11);
        let b11 = chain.fork(h(2)).mine_empty(9).tip();
        let validator = chain.validator(b11);
        let peers = Peers { transport: Arc::new(MockPeerSet::new(&chain, a11)), in_flight: 4 };
        let balancer = balancer(&[&validator], Some(peers), &CancellationToken::new());
        let depth = FinalityDepth::new(NonZeroU32::new(3).expect("non-zero"));
        let mut headers = HeaderChain::new(depth);
        let a3 = decode_header(&chain.header_bytes(trunk[3].header().hash)).expect("decodes");
        headers.anchor(&a3, h(3));
        insert(&mut headers, &trunk[4..]).expect("A4..=A11 link onto A3");
        headers.finalize(headers.finalizable().expect("A11 = A8 + depth"));
        let verified = headers.snapshot().expect("anchored");
        assert_eq!(verified.header_at(h(2)), None, "no header below the A3 anchor");
        assert_eq!(verified.finalized_tip(), trunk[8].block_ref(), "finalized A8");
        let (_verified, verified_rx) = watch::channel(Some(Arc::new(verified)));
        let lookahead = NonZeroUsize::new(16).expect("nonzero");
        let mut follower = FinalizedFollower::new(verified_rx, balancer, lookahead);
        let mut queue = follower.subscribe(IndexKind::BlockHash, None, NonZeroUsize::MAX);
        tokio::spawn(follower.run(CancellationToken::new()));

        let sent: Vec<BlockRef> = received(&mut queue, 9).await.iter().map(|s| s.block).collect();

        let a0_to_a8: Vec<BlockRef> = trunk[..=8].iter().map(|block| block.block_ref()).collect();
        assert_eq!(sent, a0_to_a8, "A0..=A8: the verified chain's, none of B's");
    }
}
