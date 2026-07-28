//! Synthesise a tip subscription for sources that have no native stream.

use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::{GetChainTip, GetChainTipError, QueryError, SubscribeChainTip, TipObservation};

/// A tip subscription built by polling a source that has no native stream.
///
/// A decorator rather than something baked into each adapter, matching
/// [`Resilient`](crate::Resilient): the capability is synthesised on top of any
/// source that can answer [`GetChainTip`], so one implementation serves every
/// pollable source rather than each adapter growing its own poll loop.
///
/// Deliberately not a wrapper that re-exposes the source's other traits. It
/// owns only the polling, so a composite holds it *alongside* its adapters and
/// delegates [`SubscribeChainTip`] to it — no forwarding boilerplate for the
/// thirty other queries it has nothing to say about.
///
/// # What it is not
///
/// Polling is not a push stream, and this type does not pretend otherwise.
/// Readings arrive at the configured cadence rather than when a block lands, so
/// a subscriber sees a tip change up to one interval late. What it does
/// guarantee is that every published reading is stamped with when it was taken,
/// so a subscriber can tell a quiet chain from a source that has stopped
/// answering — see [`TipObservation::age`].
///
/// The poll task stops when this value is dropped.
pub struct PolledChainTip {
    tip: watch::Receiver<TipObservation>,
    task: JoinHandle<()>,
}

impl PolledChainTip {
    /// Start polling `source` every `interval`.
    ///
    /// Takes one reading before returning, so the subscription always has a
    /// real tip to hand out and never a placeholder. That makes construction
    /// fallible: a source that cannot answer once cannot seed a subscription,
    /// and failing here is better than handing back a channel that may never
    /// carry anything.
    ///
    /// Later failures do not stop the poller. It keeps the last good reading
    /// and keeps trying, letting the reading's age carry the bad news — a
    /// transient outage should not tear down a subscription that will recover.
    pub async fn spawn<S>(
        source: S,
        interval: Duration,
    ) -> Result<Self, QueryError<GetChainTipError>>
    where
        S: GetChainTip + Send + 'static,
    {
        let (hash, height) = source.get_chain_tip().await?;
        let (tx, tip) = watch::channel(TipObservation::now(hash, height));

        let task = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            // The first tick completes immediately, and the constructor has
            // just read the tip; skip it so the source is not asked twice in
            // quick succession at startup.
            ticker.tick().await;

            loop {
                ticker.tick().await;

                // Every receiver is gone, so nothing would observe further
                // readings. Stop rather than keep querying the validator.
                if tx.is_closed() {
                    return;
                }

                if let Ok((hash, height)) = source.get_chain_tip().await {
                    // Published on every success, including an unchanged tip:
                    // that is what keeps `observed_at` a liveness signal rather
                    // than a record of when the chain last moved.
                    tx.send_replace(TipObservation::now(hash, height));
                }
            }
        });

        Ok(Self { tip, task })
    }

    /// The most recent reading, without waiting for the next one.
    pub fn current(&self) -> TipObservation {
        *self.tip.borrow()
    }
}

impl SubscribeChainTip for PolledChainTip {
    fn subscribe_to_chain_tip(&self) -> Option<watch::Receiver<TipObservation>> {
        Some(self.tip.clone())
    }
}

impl Drop for PolledChainTip {
    fn drop(&mut self) {
        // The task holds the sender and would otherwise poll the validator for
        // the life of the process.
        self.task.abort();
    }
}

#[cfg(all(test, feature = "testing"))]
mod tests {
    use super::*;
    use crate::mock::MockChain;
    use zaino_primitives::types::{Block, BlockHash, BlockHeader, ChainMetadata, Height};

    fn height(h: u32) -> Height {
        Height::try_from(h).expect("valid height")
    }

    fn hash(byte: u8) -> BlockHash {
        BlockHash::from([byte; 32])
    }

    fn test_block(h: u32, hash_byte: u8) -> Block {
        Block {
            header: BlockHeader {
                hash: hash(hash_byte),
                prev_hash: BlockHash::ZERO,
                height: height(h),
                time: 0,
                merkle_root: [0; 32].into(),
                block_commitments: [0; 32].into(),
                bits: 0,
                nonce: [0; 32],
            },
            transactions: vec![],
            chain_metadata: ChainMetadata {
                sapling_tree_size: 0,
                orchard_tree_size: 0,
                ironwood_tree_size: 0,
            },
        }
    }

    /// The constructor must not hand back a subscription seeded with a
    /// placeholder, so it reads once and fails if that read fails.
    #[tokio::test]
    async fn spawn_fails_when_the_source_has_no_tip() {
        let result = PolledChainTip::spawn(MockChain::new(), Duration::from_millis(10)).await;

        assert!(
            result.is_err(),
            "empty chain should not seed a subscription"
        );
    }

    #[tokio::test]
    async fn current_reading_is_available_immediately() {
        let chain = MockChain::new().with_block(test_block(0, 1));

        let polled = PolledChainTip::spawn(chain, Duration::from_secs(60))
            .await
            .expect("seeded from the initial read");

        let observation = polled.current();
        assert_eq!(observation.height, height(0));
        assert_eq!(observation.hash, hash(1));
        assert!(observation.age() < Duration::from_secs(1));
    }

    /// A subscriber must see readings continue to arrive while the tip is
    /// unchanged — that is what distinguishes a quiet chain from a dead poller.
    #[tokio::test]
    async fn republishes_an_unchanged_tip_so_age_stays_current() {
        let chain = MockChain::new().with_block(test_block(0, 1));

        let polled = PolledChainTip::spawn(chain, Duration::from_millis(5))
            .await
            .expect("seeded");
        let mut subscriber = polled
            .subscribe_to_chain_tip()
            .expect("always subscribable");

        subscriber.changed().await.expect("poller publishes again");

        let observation = *subscriber.borrow_and_update();
        assert_eq!(observation.hash, hash(1), "tip did not change");
        assert!(
            observation.age() < Duration::from_secs(1),
            "reading is fresh"
        );
    }

    /// Dropping the subscription must stop the background poll, or a
    /// short-lived subscriber would query the validator forever.
    #[tokio::test]
    async fn dropping_stops_the_poll_task() {
        let chain = MockChain::new().with_block(test_block(0, 1));

        let polled = PolledChainTip::spawn(chain, Duration::from_millis(5))
            .await
            .expect("seeded");
        let task = polled.task.abort_handle();

        drop(polled);
        tokio::task::yield_now().await;

        assert!(task.is_finished(), "poll task outlived its subscription");
    }
}
