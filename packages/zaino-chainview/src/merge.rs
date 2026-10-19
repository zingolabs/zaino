//! The merge: per-validator deltas in, one published `ChainViewSnapshot` out
//!
//! - Store, then signal (a woken reader must find what woke it)

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use bytes::Bytes;
use imbl::{OrdSet, Vector};
use tokio::sync::watch;
use tokio::time::Instant;
use zaino_header_chain::HeaderChainSnapshot;
use zaino_primitives::types::{Announced, BlockRef, FinalityDepth, PeerTxId};
use zaino_source::{BlockchainInfo, NodeRelease, PeerInfo};
use zaino_traffic::{Health, TrustedValidatorId};
use zcash_protocol::value::Zatoshis;
use zcash_protocol::TxId;

use crate::chain_view_snapshot::{ChainViewSnapshot, Sighting};
use crate::telemetry;
use crate::trusted_validators::{TrustedValidatorMeta, TrustedValidatorSet};

/// State before a merge's change (settle's baseline)
struct Before {
    best: Option<BlockRef>,
    readers: TrustedValidatorSet,
}

impl Before {
    fn of(state: &ChainViewSnapshot) -> Self {
        Self { best: state.best(), readers: state.mempool_readers() }
    }
}

/// One id the peer quorum admits, not yet admitted here (`raw` = its bytes when held already)
pub(crate) type Candidate = (PeerTxId, Option<Bytes>);

/// One txid a member listed
///
/// - `raw` = `None` when the view already held it (bytes fetched once, never per member)
#[derive(Debug, Clone)]
pub(crate) struct Sighted {
    pub(crate) txid: TxId,
    pub(crate) raw: Option<Bytes>,
    pub(crate) fee: Zatoshis,
}

/// What every answered poll read, mempool on or off
///
/// - `block_at_best` = its `getblockhash` answer at the best the poll started under
/// - `peers`, `release`: `Some` only on a metadata poll whose half answered (else the last kept)
#[derive(Debug, Clone)]
pub(crate) struct Reading {
    pub(crate) block_at_best: Option<BlockRef>,
    pub(crate) info: BlockchainInfo,
    pub(crate) peers: Option<Vec<PeerInfo>>,
    pub(crate) release: Option<NodeRelease>,
    pub(crate) streaming: bool,
}

/// One trusted validator's poll-to-poll mempool change
#[derive(Debug, Clone)]
pub(crate) struct Listing {
    pub(crate) added: Vec<Sighted>,
    pub(crate) removed: Vec<TxId>,
}

/// One member's poll, as the merge takes it: its reading first, then its listing (bytes fetched
/// between: holder facts never wait on them)
#[derive(Debug, Clone)]
pub(crate) enum TrustedValidatorReport {
    /// `listed` = mempool read (its listing follows), else sightings retracted
    Read {
        reading: Reading,
        health: Health,
        listed: bool,
    },
    Listed(Listing),
    /// `Down` = neither chain nor sightings, else sightings kept + holds no tip
    Failed(Health),
}

/// One merge's effect: `Settle(touched)` = settle (spreads re-judged for `touched`) + publish
enum Change {
    Unchanged,
    Settle(Vec<TxId>),
}

/// The published cell + its watch, and the lock that serialises merges
///
/// - non-generic: the poll merge and readers hold it (the source type stays the balancer's)
/// - `state` = merge working copy (`imbl` throughout: cloning it to publish = `O(1)`)
pub(crate) struct ChainViewCore {
    state: Mutex<ChainViewSnapshot>,
    published: ArcSwap<ChainViewSnapshot>,
    published_tx: watch::Sender<()>,
}

impl ChainViewCore {
    pub(crate) fn new(
        trusted_validators: Vector<TrustedValidatorMeta>,
        depth: FinalityDepth,
    ) -> Self {
        let empty = ChainViewSnapshot::empty(trusted_validators, depth);
        Self {
            state: Mutex::new(empty.clone()),
            published: ArcSwap::from_pointee(empty),
            published_tx: watch::Sender::new(()),
        }
    }

    pub(crate) fn current(&self) -> Arc<ChainViewSnapshot> {
        self.published.load_full()
    }

    pub(crate) fn subscribe_published(&self) -> watch::Receiver<()> {
        self.published_tx.subscribe()
    }

    /// Fetch-once gate: a member listing a held txid = reported without a round trip
    pub(crate) fn holds(&self, txid: &TxId) -> bool {
        self.published.load().sighting(txid).is_some()
    }

    /// One member's poll merged, published → txids not admitted
    ///
    /// - not admitted = listed, bytes nowhere (re-listed + re-fetched next poll, never lost)
    pub(crate) fn apply(
        &self,
        validator: TrustedValidatorId,
        report: TrustedValidatorReport,
    ) -> Vec<TxId> {
        self.merge(|state| {
            let mut touched: Vec<TxId> = Vec::new();
            let mut unadmitted: Vec<TxId> = Vec::new();
            let Some(meta) = state.trusted_validators.get_mut(validator.get()) else {
                return (Change::Unchanged, unadmitted);
            };
            match report {
                TrustedValidatorReport::Read { reading, health, listed } => {
                    meta.health = health;
                    read(meta, reading);
                    if !listed {
                        touched.extend(retract(&mut state.mempool, validator));
                    }
                }
                TrustedValidatorReport::Listed(listing) => {
                    for txid in &listing.removed {
                        if let Some(sighting) = state.mempool.get_mut(txid) {
                            sighting.unsight(validator);
                            touched.push(*txid);
                        }
                    }
                    for sighted in listing.added {
                        match (state.mempool.get_mut(&sighted.txid), sighted.raw) {
                            (Some(sighting), _) => {
                                let verified = sighting.timeline().first_trusted.is_some();
                                sighting.sight(validator);
                                sighting.listed_fee(sighted.fee);
                                if !verified {
                                    telemetry::first_trusted(sighting);
                                }
                            }
                            (None, Some(raw)) => {
                                let mut sighting = Sighting::new(raw, Some(sighted.fee), false);
                                sighting.sight(validator);
                                telemetry::first_trusted(&sighting);
                                state.mempool.insert(sighted.txid, sighting);
                            }
                            // held at the fetch-once check, gone before the merge
                            (None, None) => {
                                unadmitted.push(sighted.txid);
                                continue;
                            }
                        }
                        touched.push(sighted.txid);
                    }
                }
                TrustedValidatorReport::Failed(health) => {
                    meta.health = health;
                    if health == Health::Down {
                        (meta.info, meta.block_at_best) = (None, None);
                        meta.peers = Vector::new();
                        touched.extend(retract(&mut state.mempool, validator));
                    }
                }
            }
            (Change::Settle(touched), unadmitted)
        })
    }

    /// One batch of block `inv`s + the live peer set (publishes only on a change)
    ///
    /// - only live (outbound) peers carry the chain (inbound = anyone who dialed us)
    pub(crate) fn apply_heard(&self, batch: Vec<Announced>, live: Vec<SocketAddr>) {
        self.merge(|state| {
            let live: OrdSet<SocketAddr> = live.into_iter().collect();
            let mut changed = live != state.peers_live;
            let carrying = std::mem::take(&mut state.peers_carrying);
            let carriers = carrying.len();
            state.peers_carrying =
                carrying.into_iter().filter(|(peer, _)| live.contains(peer)).collect();
            changed |= state.peers_carrying.len() != carriers;
            state.peers_live = live;
            for Announced { peer, hash } in batch {
                if state.peers_live.contains(&peer) {
                    changed |= state.peers_carrying.insert(peer, hash) != Some(hash);
                }
            }
            let change = match changed {
                true => Change::Settle(Vec::new()),
                false => Change::Unchanged,
            };
            (change, ())
        })
    }

    /// One mempool poll round (each live peer's answer, `None` = failed) merged, published:
    /// retention re-judged → what the quorum admits that is not admitted here yet
    pub(crate) fn apply_listings(
        &self,
        answers: Vec<(SocketAddr, Option<Vec<PeerTxId>>)>,
        now: Instant,
    ) -> Vec<Candidate> {
        self.merge(|state| {
            state.listings.round(answers, now);
            let lost: Vec<TxId> = state
                .mempool
                .iter()
                .filter(|(txid, sighting)| sighting.quorum() && !state.listings.retains(txid))
                .map(|(txid, _)| *txid)
                .collect();
            for txid in &lost {
                if let Some(sighting) = state.mempool.get_mut(txid) {
                    sighting.set_quorum(false);
                }
            }
            let candidates = state.listings.admitted().into_iter().filter_map(|id| {
                match state.mempool.get(&id.txid) {
                    Some(sighting) if sighting.quorum() => None,
                    Some(sighting) => Some((id, Some(sighting.raw().clone()))),
                    None => Some((id, None)),
                }
            });
            let candidates = candidates.collect();
            (Change::Settle(lost), candidates)
        })
    }

    /// Candidates that passed our checks (`admission.rs`), each still admitted by the quorum now
    pub(crate) fn admit_by_quorum(&self, admitted: Vec<(PeerTxId, Bytes, Zatoshis)>) {
        self.merge(|state| {
            let mut touched = Vec::new();
            for (id, raw, fee) in admitted {
                if !state.listings.admits(&id) {
                    continue;
                }
                if !state.mempool.contains_key(&id.txid) {
                    state.mempool.insert(id.txid, Sighting::new(raw, Some(fee), false));
                }
                if let Some(sighting) = state.mempool.get_mut(&id.txid) {
                    sighting.listed_fee(fee);
                    if sighting.set_quorum(true) {
                        touched.push(id.txid);
                    }
                }
            }
            let change = match touched.is_empty() {
                true => Change::Unchanged,
                false => Change::Settle(touched),
            };
            (change, ())
        })
    }

    /// Header sync's word: the verified chain (`None` = nothing verified yet)
    pub(crate) fn apply_headers(&self, chain: Option<Arc<HeaderChainSnapshot>>) {
        self.merge(|state| {
            state.set_chain(chain, Instant::now());
            (Change::Settle(Vec::new()), ())
        })
    }

    /// Under the merge lock: `change`, then [`settle`](Self::settle) unless [`Change::Unchanged`]
    fn merge<T>(&self, change: impl FnOnce(&mut ChainViewSnapshot) -> (Change, T)) -> T {
        let mut guard = self.state.lock().expect("chainview merge mutex poisoned");
        let before = Before::of(&guard);
        let (change, out) = change(&mut guard);
        if let Change::Settle(touched) = change {
            self.settle(guard, before, touched);
        }
        out
    }

    /// After any change: alarms, spreads, drops, publish
    fn settle(
        &self,
        mut guard: std::sync::MutexGuard<'_, ChainViewSnapshot>,
        before: Before,
        touched: Vec<TxId>,
    ) {
        let state = &mut *guard;
        let stalled = state.finality_stalled(Instant::now());
        state.alarms = telemetry::alarms(&state.trusted_validators, stalled);

        // reader joined / left → any spread may complete (else only `touched`)
        let readers = state.mempool_readers();
        let spreading: Vec<TxId> = match readers == before.readers {
            true => touched.clone(),
            false => state.mempool.keys().copied().collect(),
        };
        for txid in spreading {
            if let Some(sighting) = state.mempool.get_mut(&txid) {
                if sighting.reached_all(readers) {
                    telemetry::all_trusted(sighting);
                }
            }
        }

        // unlisted dropped on a new best block (`ours` nobody lists after a block = mined or gone)
        let moved = state.best() != before.best;
        let dropped: Vec<TxId> = state
            .mempool
            .iter()
            .filter(|(_, sighting)| !sighting.state().listed() && (moved || !sighting.ours()))
            .map(|(txid, _)| *txid)
            .collect();
        for txid in &dropped {
            if let Some(gone) = state.mempool.remove(txid) {
                telemetry::left(&gone, moved);
            }
        }

        self.publish(guard);
    }

    /// Relayed by us → served before it propagates
    pub(crate) fn mark_ours(&self, txid: TxId, raw: Bytes) {
        self.merge(|state| {
            match state.mempool.get_mut(&txid) {
                Some(sighting) => sighting.mark_ours(),
                None => drop(state.mempool.insert(txid, Sighting::new(raw, None, true))),
            }
            (Change::Settle(vec![txid]), ())
        })
    }

    /// Stored under the merge lock (publishes in merge order), then signalled
    fn publish(&self, state: std::sync::MutexGuard<'_, ChainViewSnapshot>) {
        self.published.store(Arc::new(state.clone()));
        drop(state);
        self.published_tx.send_replace(());
    }
}

impl std::fmt::Debug for ChainViewCore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pinned = self.published.load();
        f.debug_struct("ChainViewCore").field("best", &pinned.best()).finish_non_exhaustive()
    }
}

/// An answering poll: claim + block at best, clock estimate, peers (a failed peer read keeps the
/// last)
fn read(meta: &mut TrustedValidatorMeta, reading: Reading) {
    meta.block_at_best = reading.block_at_best;
    meta.info = Some(reading.info);
    meta.streaming = reading.streaming;
    if let Some(peers) = reading.peers {
        meta.peers = peers.into_iter().collect();
    }
    if let Some(release) = reading.release {
        meta.release = Some(release);
    }
}

/// `validator`'s bit cleared from every sighting → the txids it held
fn retract(mempool: &mut imbl::OrdMap<TxId, Sighting>, validator: TrustedValidatorId) -> Vec<TxId> {
    let held: Vec<TxId> = mempool
        .iter()
        .filter(|(_, sighting)| sighting.trusted().contains(validator))
        .map(|(txid, _)| *txid)
        .collect();
    for txid in &held {
        if let Some(sighting) = mempool.get_mut(txid) {
            sighting.unsight(validator);
        }
    }
    held
}
