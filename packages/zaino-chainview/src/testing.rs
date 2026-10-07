//! [`MockPeers`]: the Zcash p2p network as a script (announcements the test sends, pushes recorded)

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::Mutex;

use bytes::Bytes;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use futures::StreamExt;
use tokio::sync::broadcast;
use zaino_primitives::types::{Height, TransactionId};
use zaino_source::{FailureMode, NonDomainError};

use crate::{Heard, ValidatorP2pSource};

/// `live` announce and take pushes (answering nothing, as a peer does); `dead` refuse a connection
pub struct MockPeers {
    live: Vec<SocketAddr>,
    dead: BTreeSet<SocketAddr>,
    pushes: Mutex<Vec<SocketAddr>>,
    announce: broadcast::Sender<Heard>,
}

impl MockPeers {
    pub fn new(
        live: impl IntoIterator<Item = SocketAddr>,
        dead: impl IntoIterator<Item = SocketAddr>,
    ) -> Self {
        Self {
            live: live.into_iter().collect(),
            dead: dead.into_iter().collect(),
            pushes: Mutex::new(Vec::new()),
            announce: broadcast::channel(64).0,
        }
    }

    /// One `inv` from `peer` (a watch subscribed first: `heard()`)
    pub fn announce(&self, peer: SocketAddr, txids: Vec<TransactionId>) {
        let heard = Heard { peer, txids };
        self.announce.send(heard).expect("a peer watch subscribed before the announcement");
    }

    /// Entries pushed to, in order
    pub fn pushes(&self) -> Vec<SocketAddr> {
        self.pushes.lock().expect("MockPeers mutex poisoned").clone()
    }
}

impl ValidatorP2pSource for MockPeers {
    fn heard(&self) -> BoxStream<'static, Heard> {
        let rx = self.announce.subscribe();
        futures::stream::unfold(rx, |mut rx| async move { Some((rx.recv().await.ok()?, rx)) })
            .boxed()
    }

    fn live(&self) -> Vec<SocketAddr> {
        self.live.clone()
    }

    fn entries(&self, _: Option<Height>) -> Vec<SocketAddr> {
        self.live.clone()
    }

    fn push(&self, entry: SocketAddr, _: Bytes) -> BoxFuture<'static, Result<(), NonDomainError>> {
        self.pushes.lock().expect("MockPeers mutex poisoned").push(entry);
        let refused = self.dead.contains(&entry);
        Box::pin(std::future::ready(match refused {
            true => Err(NonDomainError::new(FailureMode::Connection, "MockPeers: dead peer")),
            false => Ok(()),
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;
    use std::sync::Arc;
    use std::time::Duration;

    use tokio_util::sync::CancellationToken;
    use zaino_primitives::testing::MockChain;
    use zaino_primitives::types::ReorgDepth;
    use zaino_source::testing::{raw_transaction, MockValidator};
    use zaino_traffic::{Limits, TrafficBalancer, Trusted, ValidatorId};

    use super::*;
    use crate::{ChainView, Count};

    /// Paused clock, two validators on one chain, four live peers:
    /// - two announcements before any trusted listing: not held; listed by one = `peers: 2/4,
    ///   trusted: 1/2`
    /// - a submission enters through one peer, never a validator; an outsider's announcement + a
    ///   trusted listing answer the wallet
    #[tokio::test(start_paused = true)]
    async fn announcements_join_a_trusted_listing_and_a_submission_enters_through_a_peer() {
        let chain = MockChain::regtest();
        let validators: Vec<Arc<MockValidator>> =
            (0..2).map(|_| Arc::new(MockValidator::following(&chain, chain.genesis()))).collect();
        let peer = |b: u8| SocketAddr::from(([10, b, 0, 1], 8233));
        let peers = Arc::new(MockPeers::new((0..4).map(peer), [peer(9)]));
        let limits = Limits::new(8, None).expect("8 ≥ MIN_CONNECTIONS");
        let trusted = validators.iter().map(|validator| Trusted {
            source: Arc::clone(validator),
            priority: 0,
            limits,
        });
        let (balancer, driver) = TrafficBalancer::new(trusted.collect(), None);
        let addresses = vec!["a:8232".to_owned(), "b:8232".to_owned()];
        let depth = ReorgDepth::new(NonZeroU32::new(3).expect("non-zero"));
        let view = ChainView::new(addresses, balancer.clone(), depth).expect("two validators");
        let view = Arc::new(view.with_peers(peers.clone()));
        let cancel = CancellationToken::new();
        tokio::spawn(driver.run(cancel.clone()));
        tokio::spawn(view.observation_fold().run(cancel.clone()));
        tokio::spawn(view.peer_watch().expect("peers configured").run(cancel.clone()));
        tokio::task::yield_now().await;
        // each member polled again + folded (paused clock: the 1 ms timer fires once all idle)
        let poll_all = || async {
            let ids = (0..validators.len()).map(|at| ValidatorId::new(at).expect("small"));
            let mut watches: Vec<_> = ids.map(|id| balancer.observe(id)).collect();
            for watch in &mut watches {
                watch.borrow_and_update();
            }
            for watch in &mut watches {
                watch.changed().await.expect("driver running");
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        };
        let fold = || tokio::time::sleep(crate::peers::PEER_FOLD * 2);
        poll_all().await;

        let (gossiped, raw) = raw_transaction(1, 0);
        peers.announce(peer(1), vec![gossiped]);
        peers.announce(peer(2), vec![gossiped]);
        fold().await;
        assert_eq!(view.subscriber().current().spread(&gossiped), None, "peer-only = not held");
        validators[0].mempool_insert(raw, 1_000);
        poll_all().await;
        let spread = view.subscriber().current().spread(&gossiped).expect("held once listed");
        let (two_of_four, one_of_two) = (Count { seen: 2, of: 4 }, Count { seen: 1, of: 2 });
        assert_eq!((spread.peers, spread.trusted), (two_of_four, one_of_two));

        let (txid, sent) = raw_transaction(2, 0);
        let submitting = Arc::clone(&view);
        let answer = tokio::spawn(async move { submitting.submit(sent).await });
        tokio::task::yield_now().await;
        let entered = peers.pushes();
        let sends: usize = validators.iter().map(|validator| validator.calls().sends).sum();
        assert_eq!((entered.len(), sends), (1, 0), "one peer entry, no validator: {entered:?}");
        let outsider = (0..4).map(peer).find(|p| !entered.contains(p)).expect("three untried");
        peers.announce(outsider, vec![txid]);
        validators[1].mempool_insert(raw_transaction(2, 0).1, 1_000);
        fold().await;
        poll_all().await;
        assert_eq!(answer.await.expect("submission task").expect("relayed"), txid);
        cancel.cancel();
    }
}
