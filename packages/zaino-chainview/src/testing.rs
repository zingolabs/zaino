//! [`MockPeers`]: the Zcash p2p network as a script (mempools and block `inv`s the test sets,
//! pushes recorded); a `MockChain` [`Ledger`] as the UTXO set

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::Mutex;

use bytes::Bytes;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use futures::StreamExt;
use tokio::sync::broadcast;
use zaino_primitives::testing::Ledger;
use zaino_primitives::types::{Announced, PeerTxId};
use zaino_source::{FailureMode, NonDomainError};
use zcash_protocol::value::Zatoshis;
use zcash_protocol::BlockHash;
use zcash_transparent::bundle::OutPoint;

use crate::{PeerSource, UnspentOutputs};

/// `live` list their mempools, announce and take relays (answering nothing, as a peer does);
/// `dead` refuse a connection
///
/// - a relayed transaction lands in no mempool by itself: the test lists it where it spread
/// - no live peer = a relay fails (zebra's peer set, nobody ready)
pub struct MockPeers {
    live: Vec<SocketAddr>,
    dead: BTreeSet<SocketAddr>,
    pushes: Mutex<usize>,
    mempools: Mutex<BTreeMap<SocketAddr, Vec<PeerTxId>>>,
    bytes: Mutex<BTreeMap<PeerTxId, Bytes>>,
    fetched: Mutex<Vec<PeerTxId>>,
    announce: broadcast::Sender<Announced>,
}

impl MockPeers {
    pub fn new(
        live: impl IntoIterator<Item = SocketAddr>,
        dead: impl IntoIterator<Item = SocketAddr>,
    ) -> Self {
        Self {
            live: live.into_iter().collect(),
            dead: dead.into_iter().collect(),
            pushes: Mutex::new(0),
            mempools: Mutex::new(BTreeMap::new()),
            bytes: Mutex::new(BTreeMap::new()),
            fetched: Mutex::new(Vec::new()),
            announce: broadcast::channel(64).0,
        }
    }

    /// `peer`'s mempool from its next listing on: these ids (each one's bytes = [`hold`](Self::hold))
    pub fn list(&self, peer: SocketAddr, ids: Vec<PeerTxId>) {
        self.mempools.lock().expect("MockPeers mutex poisoned").insert(peer, ids);
    }

    /// `raw` served for `id` by whoever lists it
    pub fn hold(&self, id: PeerTxId, raw: Bytes) {
        self.bytes.lock().expect("MockPeers mutex poisoned").insert(id, raw);
    }

    /// One block `inv` from `peer`: its new tip (a watch subscribed first: `heard()`)
    pub fn announce_block(&self, peer: SocketAddr, hash: BlockHash) {
        let heard = Announced { peer, hash };
        self.announce.send(heard).expect("a peer watch subscribed before the announcement");
    }

    /// Relays asked for so far
    pub fn pushes(&self) -> usize {
        *self.pushes.lock().expect("MockPeers mutex poisoned")
    }

    /// Ids whose bytes were asked for, each time, in order
    pub fn fetched(&self) -> Vec<PeerTxId> {
        self.fetched.lock().expect("MockPeers mutex poisoned").clone()
    }
}

impl PeerSource for MockPeers {
    fn heard(&self) -> BoxStream<'static, Announced> {
        let rx = self.announce.subscribe();
        futures::stream::unfold(rx, |mut rx| async move { Some((rx.recv().await.ok()?, rx)) })
            .boxed()
    }

    fn live(&self) -> Vec<SocketAddr> {
        self.live.clone()
    }

    /// Counted, never placed: the test lists it where it spread (`list`)
    fn push(&self, _: Bytes) -> BoxFuture<'static, Result<(), NonDomainError>> {
        *self.pushes.lock().expect("MockPeers mutex poisoned") += 1;
        let refused = self.live.is_empty();
        Box::pin(std::future::ready(match refused {
            true => Err(NonDomainError::new(FailureMode::Timeout, "MockPeers: no ready peer")),
            false => Ok(()),
        }))
    }

    fn mempool(
        &self,
        peer: SocketAddr,
    ) -> BoxFuture<'static, Result<Vec<PeerTxId>, NonDomainError>> {
        let listed = match self.dead.contains(&peer) {
            true => Err(NonDomainError::new(FailureMode::Connection, "MockPeers: dead peer")),
            false => {
                Ok(self.mempools.lock().expect("MockPeers mutex poisoned").get(&peer).cloned())
            }
        };
        Box::pin(std::future::ready(listed.map(Option::unwrap_or_default)))
    }

    fn transactions(
        &self,
        ids: Vec<PeerTxId>,
    ) -> BoxFuture<'static, Result<Vec<Option<Bytes>>, NonDomainError>> {
        self.fetched.lock().expect("MockPeers mutex poisoned").extend(ids.iter().copied());
        let bytes = self.bytes.lock().expect("MockPeers mutex poisoned");
        let found = ids.iter().map(|id| bytes.get(id).cloned()).collect();
        Box::pin(std::future::ready(Ok(found)))
    }
}

/// One `MockChain` tip's UTXO set (`chain.ledger(tip)`)
impl UnspentOutputs for Ledger {
    fn values(&self, outpoints: &[OutPoint]) -> Option<Vec<Option<Zatoshis>>> {
        Some(outpoints.iter().map(|at| self.unspent(at)).collect())
    }
}
