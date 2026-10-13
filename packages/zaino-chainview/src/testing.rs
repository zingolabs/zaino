//! [`MockPeers`]: the Zcash p2p network as a script (mempools and block `inv`s the test sets,
//! pushes recorded); [`MockUtxos`]: a UTXO set the test names

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::Mutex;

use bytes::Bytes;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use futures::StreamExt;
use tokio::sync::broadcast;
use zaino_primitives::types::{BlockHash, OutPoint, PeerTxId, Zatoshis};
use zaino_source::{FailureMode, NonDomainError};

use crate::{Heard, UnspentOutputs, ValidatorP2pSource};

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
        let heard = Heard { peer, hash };
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

impl ValidatorP2pSource for MockPeers {
    fn heard(&self) -> BoxStream<'static, Heard> {
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

/// UTXO set of exactly these outpoints (`None` = no set yet: nothing served)
#[derive(Debug, Default)]
pub struct MockUtxos(Mutex<Option<BTreeMap<OutPoint, Zatoshis>>>);

impl MockUtxos {
    pub fn of(unspent: impl IntoIterator<Item = (OutPoint, Zatoshis)>) -> Self {
        Self(Mutex::new(Some(unspent.into_iter().collect())))
    }
}

impl UnspentOutputs for MockUtxos {
    fn values(&self, outpoints: &[OutPoint]) -> Option<Vec<Option<Zatoshis>>> {
        let set = self.0.lock().expect("MockUtxos mutex poisoned");
        let set = set.as_ref()?;
        Some(outpoints.iter().map(|at| set.get(at).copied()).collect())
    }
}
