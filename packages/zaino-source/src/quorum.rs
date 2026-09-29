//! Several validators behind one source.
//!
//! [`Quorum<A>`] holds N sources of one adapter type and answers the same
//! `OneShot*` ports the adapter answers: the chain tip by k-of-n agreement,
//! block reads spread over the members with failover, passthrough questions
//! from the first member that answers, and the mempool from one member at a
//! time. It sits *inside* the client — `ValidatorClient<Quorum<A>>` — so retry
//! stays where it is and nothing above the client learns there are several
//! validators. Design and rules: `quorum.md` beside this crate's guide.
//!
//! ```text
//! tip     = the (hash, height) ≥ k members report        (else: unavailable)
//! fetch   = round-robin start, next member on failure or miss
//! mempool = one member, pinned until it fails
//! ```

mod agreement;
mod config;
mod ports;

use std::num::NonZeroUsize;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::join_all;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, warn};
use zaino_primitives::types::{BlockHash, Height};

use crate::{
    FailureMode, GetChainTipError, NonDomainError, OneShotGetChainTip, QueryError, SourceLifecycle,
    SubscribeBlocks, SubscribeChainTip, TipObservation, ValidatorSource,
};
use agreement::{agree, largest_agreement, Reading};

pub use config::{QuorumBuildError, QuorumConfig};

/// A quorum of validators presented as one source.
///
/// Built from a [`QuorumConfig`] or directly from members with [`new`](Self::new);
/// a quorum of one member behaves as the bare adapter. Hand it to
/// [`ValidatorClient`](crate::ValidatorClient) like any adapter.
pub struct Quorum<A> {
    shared: Arc<Shared<A>>,
    /// The synthesised tip subscription, present once
    /// [`with_tip_polling`](Self::with_tip_polling) has run.
    tip: Option<QuorumTip>,
}

/// What the port impls and the poll task share.
struct Shared<A> {
    members: Vec<A>,
    quorum: NonZeroUsize,
    /// Where the next spread fetch starts.
    cursor: AtomicUsize,
    /// The member the mempool ports are pinned to; moves only when it fails.
    mempool_member: AtomicUsize,
    /// The hash last agreed on: the tie-break for a split at equal height.
    last_agreed: Mutex<Option<BlockHash>>,
}

/// The wrapper could not be built from these members.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QuorumConfigError {
    /// No members were given.
    #[error("a quorum needs at least one member")]
    NoMembers,
    /// More members must agree than there are members.
    #[error("a quorum of {quorum} cannot be reached by {members} members")]
    QuorumExceedsMembers {
        /// The agreement size asked for.
        quorum: usize,
        /// The members available.
        members: usize,
    },
}

/// Fewer members than the quorum agreed on a tip.
///
/// The typed cause behind the non-domain failure the ports return, so a
/// consumer's error chain reaches the counts and the last member failure.
#[derive(Debug, thiserror::Error)]
#[error("quorum not reached: {agreeing} of {members} members agree on a tip, {quorum} required")]
struct BelowQuorum {
    agreeing: usize,
    members: usize,
    quorum: usize,
    /// The last member that failed to answer, when one did.
    #[source]
    last_failure: Option<NonDomainError>,
}

/// One round of tip readings across the members.
struct Round {
    readings: Vec<Reading>,
    /// Members that answered "not ready" rather than failing.
    not_ready: usize,
    /// The last non-domain failure in the round.
    last_failure: Option<NonDomainError>,
}

impl<A> Quorum<A> {
    /// A quorum over `members`, `quorum` of which must agree on the tip.
    pub fn new(
        members: impl IntoIterator<Item = A>,
        quorum: NonZeroUsize,
    ) -> Result<Self, QuorumConfigError> {
        let members: Vec<A> = members.into_iter().collect();
        if members.is_empty() {
            return Err(QuorumConfigError::NoMembers);
        }
        if quorum.get() > members.len() {
            return Err(QuorumConfigError::QuorumExceedsMembers {
                quorum: quorum.get(),
                members: members.len(),
            });
        }
        Ok(Self {
            shared: Arc::new(Shared {
                members,
                quorum,
                cursor: AtomicUsize::new(0),
                mempool_member: AtomicUsize::new(0),
                last_agreed: Mutex::new(None),
            }),
            tip: None,
        })
    }

    /// How many members the quorum holds.
    pub fn members(&self) -> usize {
        self.shared.members.len()
    }

    /// How many members must agree on the tip.
    pub fn quorum(&self) -> NonZeroUsize {
        self.shared.quorum
    }
}

impl<A: OneShotGetChainTip> Shared<A> {
    /// Ask every member for its tip at once.
    async fn round(&self) -> Round {
        let answers = join_all(self.members.iter().map(|member| member.get_chain_tip())).await;
        let mut round = Round {
            readings: Vec::with_capacity(answers.len()),
            not_ready: 0,
            last_failure: None,
        };
        for (index, answer) in answers.into_iter().enumerate() {
            round.readings.push(match answer {
                Ok(tip) => Some(tip),
                Err(QueryError::Domain(GetChainTipError::NotReady)) => {
                    debug!(member = index, "member is not ready to report a tip");
                    round.not_ready += 1;
                    None
                }
                Err(QueryError::NonDomain(failure)) => {
                    let failure: NonDomainError = failure.into();
                    debug!(member = index, error = %failure, "member failed to report a tip");
                    round.last_failure = Some(failure);
                    None
                }
            });
        }
        round
    }

    /// The tip the members agree on right now.
    ///
    /// Below quorum this is a retryable non-domain failure carrying the
    /// counts, unless every member answered "not ready", which is that domain
    /// answer. A round whose agreement is found also records its hash as the
    /// tie-break for the next.
    async fn agreed_tip(&self) -> Result<(BlockHash, Height), QueryError<GetChainTipError>> {
        let round = self.round().await;
        let previous = *self
            .last_agreed
            .lock()
            .expect("last-agreed tip mutex poisoned");
        match agree(&round.readings, self.quorum.get(), previous) {
            Some(tip) => {
                *self
                    .last_agreed
                    .lock()
                    .expect("last-agreed tip mutex poisoned") = Some(tip.0);
                Ok(tip)
            }
            None if round.not_ready == self.members.len() => {
                Err(QueryError::Domain(GetChainTipError::NotReady))
            }
            None => Err(QueryError::NonDomain(NonDomainError::from_cause(
                FailureMode::Connection,
                BelowQuorum {
                    agreeing: largest_agreement(&round.readings),
                    members: self.members.len(),
                    quorum: self.quorum.get(),
                    last_failure: round.last_failure,
                },
            ))),
        }
    }
}

/// The poll task publishing the agreed tip, and the channel it feeds.
struct QuorumTip {
    tip: watch::Receiver<TipObservation>,
    task: JoinHandle<()>,
}

impl Drop for QuorumTip {
    fn drop(&mut self) {
        // The task holds the sender and would otherwise poll the members for
        // the life of the process.
        self.task.abort();
    }
}

impl<A: OneShotGetChainTip + 'static> Quorum<A> {
    /// Add a tip subscription, polling every member each `interval` and
    /// publishing the tip they agree on.
    ///
    /// Takes one agreed reading before returning, so the subscription always
    /// holds a real tip: a set of members that cannot agree once cannot seed
    /// one, and failing here is the boot-time gate on the validator being
    /// reachable. Later rounds below quorum publish nothing and let the last
    /// reading's age carry the news, so the published tip never moves on a
    /// transient disagreement.
    pub async fn with_tip_polling(
        mut self,
        interval: Duration,
    ) -> Result<Self, QueryError<GetChainTipError>> {
        let (hash, height) = self.shared.agreed_tip().await?;
        let (tx, tip) = watch::channel(TipObservation::now(hash, height));
        let shared = Arc::clone(&self.shared);

        let task = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            // The first tick completes immediately and the seed reading was
            // just taken; skip it so the members are not asked twice at once.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                if tx.is_closed() {
                    return;
                }
                match shared.agreed_tip().await {
                    Ok((hash, height)) => {
                        tx.send_replace(TipObservation::now(hash, height));
                    }
                    Err(error) => {
                        warn!(%error, "tip round below quorum; keeping the last agreed reading");
                    }
                }
            }
        });

        self.tip = Some(QuorumTip { tip, task });
        Ok(self)
    }
}

impl<A: ValidatorSource> ValidatorSource for Quorum<A> {
    // Members speak their own non-domain vocabulary; the quorum presents the
    // seam type whichever member answered.
    type NonDomain = NonDomainError;
}

impl<A: Send + Sync> SubscribeChainTip for Quorum<A> {
    fn subscribe_to_chain_tip(&self) -> Option<watch::Receiver<TipObservation>> {
        self.tip.as_ref().map(|tip| tip.tip.clone())
    }
}

// Block arrivals are not a question a set of validators answers as one; the
// default `None` says there is no push path, as for the single adapter.
impl<A: Send + Sync> SubscribeBlocks for Quorum<A> {}

impl<A: SourceLifecycle> SourceLifecycle for Quorum<A> {
    fn shutdown(&self) {
        for member in &self.shared.members {
            member.shutdown();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::*;
    use crate::mock::{test_block, MockChain};
    use crate::{FailureMode, OneShotGetBlock, OneShotGetBlockByHash, OneShotGetMempoolTxids};

    fn height(h: u32) -> Height {
        Height::try_from(h).expect("test height")
    }

    fn hash(byte: u8) -> BlockHash {
        BlockHash::from([byte; 32])
    }

    /// A member holding heights `0..=tip`, block `h` hashed `[10 + h; 32]`.
    fn member(tip: u32) -> MockChain {
        (0..=tip).fold(MockChain::new(), |chain, h| {
            chain.with_block(test_block(h, u8::try_from(10 + h).expect("small height")))
        })
    }

    fn quorum_of(members: Vec<MockChain>, k: usize) -> Quorum<MockChain> {
        Quorum::new(members, NonZeroUsize::new(k).expect("non-zero")).expect("valid quorum")
    }

    #[test]
    fn construction_rejects_no_members_and_an_unreachable_quorum() {
        let none: Vec<MockChain> = vec![];
        assert_eq!(
            Quorum::new(none, NonZeroUsize::MIN).err(),
            Some(QuorumConfigError::NoMembers)
        );
        let err = Quorum::new(vec![member(0)], NonZeroUsize::new(2).expect("non-zero")).err();
        assert_eq!(
            err,
            Some(QuorumConfigError::QuorumExceedsMembers {
                quorum: 2,
                members: 1
            })
        );
    }

    #[tokio::test]
    async fn two_of_three_members_agree_on_the_tip_and_the_lagging_one_is_outvoted() {
        let quorum = quorum_of(vec![member(5), member(5), member(4)], 2);
        let tip = quorum.get_chain_tip().await.expect("agreement");
        assert_eq!(tip, (hash(15), height(5)));
    }

    #[tokio::test]
    async fn below_quorum_is_a_retryable_non_domain_failure_naming_the_counts() {
        let quorum = quorum_of(vec![member(5), member(4), member(3)], 2);
        let err = quorum.get_chain_tip().await.expect_err("no agreement");
        match err {
            QueryError::NonDomain(failure) => {
                assert_eq!(failure.mode, FailureMode::Connection, "retryable");
                assert_eq!(
                    failure.to_string(),
                    "quorum not reached: 1 of 3 members agree on a tip, 2 required"
                );
            }
            QueryError::Domain(d) => panic!("expected a non-domain failure, got {d:?}"),
        }
    }

    #[tokio::test]
    async fn a_failing_member_is_skipped_without_retrying_it() {
        // Member 0 fails once; the quorum still answers from members 1 and 2
        // and never asks member 0 a second time within the call.
        let flaky = member(5).fail_next(1, FailureMode::Timeout);
        let quorum = quorum_of(vec![flaky, member(5), member(5)], 2);
        assert_eq!(
            quorum.get_chain_tip().await.expect("two agree"),
            (hash(15), height(5))
        );
    }

    #[tokio::test]
    async fn fetches_fail_over_to_a_member_that_holds_the_block() {
        // Member 0 lags at height 3; height 5 must come from a member that has it.
        let quorum = quorum_of(vec![member(3), member(5), member(5)], 2);
        for _ in 0..3 {
            let block = quorum
                .get_block(height(5))
                .await
                .expect("some member holds it");
            assert_eq!(block.header.hash, hash(15));
        }
    }

    #[tokio::test]
    async fn a_miss_is_reported_only_when_every_member_misses() {
        let quorum = quorum_of(vec![member(3), member(3)], 1);
        let err = quorum
            .get_block(height(9))
            .await
            .expect_err("nobody holds it");
        assert!(
            matches!(err, QueryError::Domain(_)),
            "unanimous miss is a miss"
        );

        // One member unreachable, the other misses: the block may exist on the
        // unreachable one, so the answer is the failure, not the miss.
        let quorum = quorum_of(
            vec![member(3).fail_next(1, FailureMode::Connection), member(3)],
            1,
        );
        let err = quorum.get_block(height(9)).await.expect_err("unknown");
        assert!(matches!(err, QueryError::NonDomain(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn by_hash_comes_from_whichever_member_holds_it() {
        let quorum = quorum_of(vec![member(2), member(5)], 1);
        let block = quorum
            .get_block_by_hash(hash(15))
            .await
            .expect("member 1 holds height 5");
        assert_eq!(block.header.height, height(5));
    }

    #[tokio::test]
    async fn the_mempool_stays_on_one_member_until_it_fails() {
        let quorum = quorum_of(
            vec![member(1).fail_next(1, FailureMode::Timeout), member(1)],
            1,
        );
        assert_eq!(quorum.shared.mempool_member.load(Ordering::Relaxed), 0);
        quorum.get_mempool_txids().await.expect("fails over");
        assert_eq!(
            quorum.shared.mempool_member.load(Ordering::Relaxed),
            1,
            "pinned to the member that answered"
        );
        quorum.get_mempool_txids().await.expect("same member");
        assert_eq!(quorum.shared.mempool_member.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn the_published_tip_does_not_move_on_a_transient_disagreement() {
        let quorum = quorum_of(vec![member(5), member(5), member(5)], 2)
            .with_tip_polling(Duration::from_millis(5))
            .await
            .expect("seeded");
        let mut subscription = quorum.subscribe_to_chain_tip().expect("polling");
        assert_eq!(subscription.borrow_and_update().height, height(5));

        // The subscription keeps the seeded tip while rounds keep agreeing.
        subscription.changed().await.expect("publishes again");
        assert_eq!(subscription.borrow_and_update().height, height(5));
    }

    #[tokio::test]
    async fn a_single_member_quorum_behaves_as_the_bare_adapter() {
        let quorum = quorum_of(vec![member(2)], 1);
        assert_eq!(
            quorum.get_chain_tip().await.expect("its own tip"),
            (hash(12), height(2))
        );
        let block = quorum.get_block(height(1)).await.expect("held");
        assert_eq!(block.header.hash, hash(11));
        assert!(matches!(
            quorum.get_block(height(7)).await,
            Err(QueryError::Domain(_))
        ));
    }

    #[tokio::test]
    async fn seeding_the_subscription_fails_below_quorum() {
        let result = quorum_of(vec![member(5), member(4), member(3)], 2)
            .with_tip_polling(Duration::from_secs(60))
            .await;
        assert!(result.is_err(), "no agreed tip to seed with");
    }

    /// The wrapper provides the one-shot ports its members do, so the client
    /// over it provides the canonical ones. A compile-time check.
    #[test]
    fn the_client_over_a_quorum_provides_the_canonical_ports() {
        fn assert_bound<S>()
        where
            S: crate::GetBlock + crate::GetBlockByHash + crate::GetChainTip + crate::GetTreestate,
        {
        }
        assert_bound::<crate::ValidatorClient<Quorum<MockChain>>>();
    }
}
