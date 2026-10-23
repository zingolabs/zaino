//! [`ChainViewCore`]: published `ChainViewSnapshot` cell + merge lock (every input → one merge)
//!
//! - Store, then signal (a woken reader must find what woke it)

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use bytes::Bytes;
use imbl::{OrdMap, OrdSet, Vector};
use tokio::sync::watch;
use tokio::time::Instant;
use zaino_header_chain::HeaderChainSnapshot;
use zaino_primitives::types::{Announced, BlockRef, FinalityDepth, PeerTxId};
use zaino_traffic::{Health, TrustedValidatorId};
use zcash_protocol::value::Zatoshis;
use zcash_protocol::TxId;

use crate::chain_view_snapshot::{ChainViewSnapshot, Sighting};
use crate::observation_merge::{Reading, Sighted, TrustedValidatorReport};
use crate::telemetry;
use crate::trusted_validators::TrustedValidatorMeta;

/// One id the peer quorum admits, not yet admitted here (`raw` = its bytes when held already)
pub(crate) type Candidate = (PeerTxId, Option<Bytes>);

/// One merge's effect: `Settle` = alarms, drops, publish
enum Change {
    Unchanged,
    Settle,
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

    /// One member's poll merged, published
    pub(crate) fn apply(&self, validator: TrustedValidatorId, report: TrustedValidatorReport) {
        self.merge(|state| {
            let Some(meta) = state.trusted_validators.get_mut(validator.get()) else {
                return (Change::Unchanged, ());
            };
            match report {
                TrustedValidatorReport::Read { reading, health, listed } => {
                    meta.health = health;
                    read(meta, reading);
                    if !listed {
                        retract(&mut state.mempool, validator, |_| true);
                    }
                }
                TrustedValidatorReport::Listed(listing) => {
                    let listed: BTreeSet<TxId> =
                        listing.iter().map(|sighted| sighted.txid).collect();
                    retract(&mut state.mempool, validator, |txid| !listed.contains(txid));
                    for sighted in listing {
                        sight(&mut state.mempool, validator, sighted);
                    }
                }
                TrustedValidatorReport::Failed(health) => {
                    meta.health = health;
                    if health == Health::Down {
                        (meta.info, meta.block_at_best) = (None, None);
                        meta.telemetry.peers = None;
                        retract(&mut state.mempool, validator, |_| true);
                    }
                }
            }
            (Change::Settle, ())
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
            (if changed { Change::Settle } else { Change::Unchanged }, ())
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
            (Change::Settle, candidates.collect())
        })
    }

    /// Candidates that passed our checks (`admission.rs`), each still admitted by the quorum now
    pub(crate) fn admit_by_quorum(&self, admitted: Vec<(PeerTxId, Bytes, Zatoshis)>) {
        self.merge(|state| {
            let mut changed = false;
            for (id, raw, fee) in admitted {
                if !state.listings.admits(&id) {
                    continue;
                }
                if !state.mempool.contains_key(&id.txid) {
                    state.mempool.insert(id.txid, Sighting::new(raw, Some(fee), false));
                }
                if let Some(sighting) = state.mempool.get_mut(&id.txid) {
                    sighting.listed_fee(fee);
                    changed |= sighting.set_quorum(true);
                }
            }
            (if changed { Change::Settle } else { Change::Unchanged }, ())
        })
    }

    /// Header sync's word: the verified chain (`None` = nothing verified yet)
    pub(crate) fn apply_headers(&self, chain: Option<Arc<HeaderChainSnapshot>>) {
        self.merge(|state| {
            state.set_chain(chain, Instant::now());
            (Change::Settle, ())
        })
    }

    /// Under the merge lock: `change`, then [`settle`](Self::settle) unless [`Change::Unchanged`]
    fn merge<T>(&self, change: impl FnOnce(&mut ChainViewSnapshot) -> (Change, T)) -> T {
        let mut guard = self.state.lock().expect("chainview merge mutex poisoned");
        let best_before = guard.best();
        let (change, out) = change(&mut guard);
        if let Change::Settle = change {
            self.settle(guard, best_before);
        }
        out
    }

    /// After any change: alarms, drops, publish
    fn settle(
        &self,
        mut guard: std::sync::MutexGuard<'_, ChainViewSnapshot>,
        best_before: Option<BlockRef>,
    ) {
        let state = &mut *guard;
        let stalled = state.finality_stalled(Instant::now());
        state.alarms = telemetry::alarms(&state.trusted_validators, stalled);

        // unlisted dropped on a new best block (`ours` nobody lists after a block = mined or gone)
        let moved = state.best() != best_before;
        let dropped: Vec<TxId> = state
            .mempool
            .iter()
            .filter(|(_, sighting)| !sighting.state().listed() && (moved || !sighting.ours()))
            .map(|(txid, _)| *txid)
            .collect();
        for txid in &dropped {
            state.mempool.remove(txid);
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
            (Change::Settle, ())
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

/// An answering poll: claim + block at best, clock estimate, telemetry
fn read(meta: &mut TrustedValidatorMeta, reading: Reading) {
    meta.block_at_best = reading.block_at_best;
    meta.info = Some(reading.info);
    meta.streaming = reading.streaming;
    meta.telemetry.absorb(reading.telemetry);
}

/// `validator`'s bit cleared from every sighting `unlisted` names
fn retract(
    mempool: &mut OrdMap<TxId, Sighting>,
    validator: TrustedValidatorId,
    unlisted: impl Fn(&TxId) -> bool,
) {
    let held: Vec<TxId> = mempool
        .iter()
        .filter(|(txid, sighting)| sighting.trusted().contains(validator) && unlisted(txid))
        .map(|(txid, _)| *txid)
        .collect();
    for txid in &held {
        if let Some(sighting) = mempool.get_mut(txid) {
            sighting.unsight(validator);
        }
    }
}

/// One listed txid sighted by `validator`
///
/// - already sighted = untouched (its `imbl` nodes stay shared with the last publish)
fn sight(mempool: &mut OrdMap<TxId, Sighting>, validator: TrustedValidatorId, sighted: Sighted) {
    let Sighted { txid, raw, fee } = sighted;
    match (mempool.get(&txid).map(|sighting| sighting.trusted().contains(validator)), raw) {
        (Some(true), _) => {}
        (Some(false), _) => {
            if let Some(sighting) = mempool.get_mut(&txid) {
                sighting.sight(validator);
                sighting.listed_fee(fee);
            }
        }
        (None, Some(raw)) => {
            let mut sighting = Sighting::new(raw, Some(fee), false);
            sighting.sight(validator);
            mempool.insert(txid, sighting);
        }
        // held at the fetch-once check, gone before the merge (fetched next poll)
        (None, None) => {}
    }
}
