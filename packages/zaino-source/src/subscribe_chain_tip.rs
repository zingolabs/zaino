//! Subscription: wake on best-chain tip readings.

use std::time::{Duration, Instant};

use tokio::sync::watch;
use zaino_primitives::types::{BlockHash, Height};

/// A tip reading, paired with when it was taken.
///
/// The timestamp is what makes a quiet chain distinguishable from a dead
/// source. A bare tip cannot express "my last three reads failed": a subscriber
/// holding an unchanging value could not tell whether no block had been mined
/// or the validator had become unreachable, and acting on a stale tip in the
/// belief that it is current is worse than knowing the reading is old.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TipObservation {
    /// Hash of the best-chain tip.
    pub hash: BlockHash,
    /// Height of the best-chain tip.
    pub height: Height,
    /// When this reading was taken.
    ///
    /// Monotonic, so it measures age correctly across a wall-clock adjustment.
    pub observed_at: Instant,
}

impl TipObservation {
    /// Record a tip as observed now.
    pub fn now(hash: BlockHash, height: Height) -> Self {
        Self {
            hash,
            height,
            observed_at: Instant::now(),
        }
    }

    /// How long ago this reading was taken.
    ///
    /// Compare against whatever staleness bound the consumer cares about; a
    /// reading much older than the source's publishing cadence means the source
    /// has stopped answering, not that the chain has stopped moving.
    pub fn age(&self) -> Duration {
        self.observed_at.elapsed()
    }
}

/// Subscribe to best-chain tip readings.
///
/// Unlike [`SubscribeBlocks`](super::SubscribeBlocks), which only signals
/// *that* the source advanced, this carries the tip itself, so a consumer
/// wanting the current tip needs no follow-up query and cannot observe a tip
/// newer than the one that woke it.
///
/// The two are separate capabilities because they answer different questions. A
/// block arriving is not necessarily a tip change: a block extending a side
/// chain wakes [`SubscribeBlocks`](super::SubscribeBlocks) while the best tip
/// stays put.
///
/// # Spurious wakes are expected
///
/// An implementation may publish a reading whose tip is unchanged — a poller
/// does so on every successful read, which is what keeps
/// [`TipObservation::observed_at`] a liveness signal rather than a record of
/// the last time the chain happened to move. Consumers must compare
/// [`TipObservation::hash`] against what they last acted on rather than
/// treating every wake as a new tip.
///
/// [`watch`] coalesces, so a consumer that falls behind a burst sees the latest
/// reading rather than a backlog — which is what a tip subscriber wants, since
/// intermediate tips are already stale by the time they are read.
///
/// Returning `None` means this source offers no tip subscription at all. A
/// source reached over a request/response transport has no native stream, but
/// one can be synthesised for it with [`PolledChainTip`](super::PolledChainTip)
/// rather than leaving every consumer to poll separately.
pub trait SubscribeChainTip: Send + Sync {
    /// Subscribe to tip readings, if this source can provide them.
    fn subscribe_to_chain_tip(&self) -> Option<watch::Receiver<TipObservation>> {
        None
    }
}
