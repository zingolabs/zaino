//! - [`MockPeers`] = Zcash p2p network as a script (mempools and block `inv`s the test sets,
//!   pushes recorded)
//! - `MockChain` [`Ledger`] = UTXO set
//! - [`ChainViewSnapshot`] fixtures: `fixed`, then `trusted_validator`, `ahead`, `behind`

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use futures::StreamExt;
use tokio::sync::broadcast;
use zaino_header_chain::HeaderChainSnapshot;
use zaino_primitives::types::{Announced, BlockRef, FinalityDepth, PeerTxId};
use zaino_source::testing::Ledger;
use zaino_source::{BlockchainInfo, FailureMode, NonDomainError};
use zaino_traffic::{Health, TrustedValidatorId};
use zcash_protocol::value::Zatoshis;
use zcash_protocol::{BlockHash, TxId};
use zcash_transparent::bundle::OutPoint;

use crate::chain_view_snapshot::Sighting;
use crate::{
    ChainViewSnapshot, PeerSource, TrustedValidatorMeta, TrustedValidatorSet, UnspentOutputs,
};

/// - `live` list their mempools, announce and take relays (answering nothing, as a peer does)
/// - `dead` refuse a connection
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

impl ChainViewSnapshot {
    /// Consumers' tests: `chain` as header sync's word, `held_by` holding its best
    ///
    /// - one fresh trusted validator per address
    /// - `ours` served (our relay)
    /// - each holder `Live` + `Agreed`, its `getblockchaininfo` = the best block, no upgrade,
    ///   branch id 0
    pub fn fixed(
        chain: Option<Arc<HeaderChainSnapshot>>,
        held_by: TrustedValidatorSet,
        addresses: &[&str],
        ours: &[(TxId, Bytes)],
    ) -> Self {
        let trusted_validators =
            addresses.iter().map(|at| TrustedValidatorMeta::new((*at).to_owned()));
        let mut view = Self::empty(trusted_validators.collect(), FinalityDepth::CONSENSUS);
        view.chain = chain;
        if let Some(best) = view.best() {
            let info = BlockchainInfo {
                tip: best,
                estimated_height: best.height,
                activations: Vec::new(),
            };
            for at in held_by.positions() {
                if let Some(meta) = view.trusted_validators.get_mut(at) {
                    meta.info = Some(info.clone());
                    meta.health = Health::Live;
                }
            }
        }
        for (txid, raw) in ours {
            view.mempool.insert(*txid, Sighting::new(raw.clone(), None, true));
        }
        view
    }

    /// Consumers' tests: trusted validator `at`'s health, and its clock estimate `behind` blocks
    /// past its tip (a [`fixed`](Self::fixed) holder's tip = best)
    /// - alarms re-derived as a merge would
    pub fn trusted_validator(mut self, at: usize, health: Health, behind: u32) -> Self {
        if let Some(meta) = self.trusted_validators.get_mut(at) {
            meta.health = health;
            if let Some(info) = meta.info.as_mut() {
                info.estimated_height = info.tip.height + behind;
            }
        }
        let paused = self.alarms.raised(crate::telemetry::Alarm::FinalityPaused);
        self.alarms = crate::telemetry::alarms(&self.trusted_validators, paused);
        self
    }

    /// Consumers' tests: holder `at` claims `by` blocks past best (header sync short of its tip)
    ///
    /// - `read` = its hash at best polled → `Ahead`, still a holder
    /// - `!read` = no poll since the chain moved → `Diverged`, not a holder (as a merge would)
    pub fn ahead(mut self, at: usize, by: u32, read: bool) -> Self {
        let best = self.best().expect("ahead of a header chain best");
        let id = TrustedValidatorId::new(at).expect("a small position");
        assert!(self.held_by().contains(id), "{at}: not a holder");
        let meta =
            self.trusted_validators.get_mut(at).expect("a holder = a configured trusted validator");
        let info = meta.info.as_mut().expect("a holder answered");
        info.tip = BlockRef { hash: BlockHash([0xA0; 32]), height: best.height + by };
        info.estimated_height = info.tip.height;
        meta.block_at_best = read.then_some(best);
        self
    }

    /// Consumers' tests: holder `at` lags `by` blocks below best on our chain (`Behind`, not a
    /// holder: header sync past it, e.g. peers ahead of a validator catching up)
    pub fn behind(mut self, at: usize, by: u32) -> Self {
        let chain = self.chain.clone().expect("behind a header chain best");
        let height = chain.best().height - by;
        let hash = chain.hash_at(height).expect("a height on the header chain");
        let id = TrustedValidatorId::new(at).expect("a small position");
        assert!(self.held_by().contains(id), "{at}: not a holder");
        let meta =
            self.trusted_validators.get_mut(at).expect("a holder = a configured trusted validator");
        let info = meta.info.as_mut().expect("a holder answered");
        info.tip = BlockRef { hash, height };
        info.estimated_height = height;
        self
    }
}

/// One `MockChain` tip's UTXO set (`chain.ledger(tip)`)
impl UnspentOutputs for Ledger {
    fn values(&self, outpoints: &[OutPoint]) -> Option<Vec<Option<Zatoshis>>> {
        Some(outpoints.iter().map(|at| self.unspent(at)).collect())
    }
}
