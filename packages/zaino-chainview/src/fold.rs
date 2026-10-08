//! The fold: per-endpoint deltas in, one published snapshot out
//!
//! - Store, then signal (a woken reader must find what woke it)

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use bytes::Bytes;
use imbl::{OrdSet, Vector};
use tokio::sync::watch;
use tokio::time::Instant;
use zaino_header_chain::VerifiedChain;
use zaino_primitives::types::{
    BlockRef, BlockchainInfo, Height, NodeRelease, PeerInfo, ReorgDepth, TransactionId, Zatoshis,
};
use zaino_traffic::{Health, ValidatorId};

use crate::endpoints::{EndpointSet, ValidatorMetadata};
use crate::holders::{Holders, PollStamp};
use crate::ports::Heard;
use crate::snapshot::{ChainViewSnapshot, Sighting};
use crate::telemetry;

/// What a fold compares against once it has changed the state
struct Before {
    held: Option<BlockRef>,
    readers: EndpointSet,
    asked: Vec<Height>,
}

impl Before {
    fn of(state: &ChainViewSnapshot) -> Self {
        let readers = state.mempool_readers();
        Self { held: held(state), readers, asked: state.holders.asked() }
    }
}

/// Best block, once a trusted validator holds it
fn held(state: &ChainViewSnapshot) -> Option<BlockRef> {
    state.best().filter(|_| !state.held_by.is_empty())
}

/// Where the holders' next `getblockhash` heights go (the balancer's `ask_each_poll`)
pub(crate) type AskEachPoll = Box<dyn Fn(Vec<Height>) + Send + Sync>;

/// A validator whose best chain served a header run ending at that block, read under that poll
pub(crate) type Served = (ValidatorId, BlockRef, PollStamp);

/// Header sync → view
#[derive(Debug, Clone)]
pub(crate) struct HeaderReport {
    pub(crate) verified: Option<Arc<VerifiedChain>>,
    pub(crate) served: Option<Served>,
    pub(crate) finality_paused: bool,
}

/// One txid a member listed; `raw` = `None` when the view already held it (bytes fetched once,
/// never per member: §5)
#[derive(Debug, Clone)]
pub(crate) struct Sighted {
    pub(crate) txid: TransactionId,
    pub(crate) raw: Option<Bytes>,
    pub(crate) fee: Zatoshis,
}

/// What every answered poll read, mempool on or off
///
/// - `held` = its `getblockhash` answers at the asked heights (`holders.rs`)
/// - `peers`, `release`: `Some` only on a metadata poll whose half answered (else the last kept)
#[derive(Debug, Clone)]
pub(crate) struct Reading {
    pub(crate) held: Vec<BlockRef>,
    pub(crate) info: BlockchainInfo,
    pub(crate) peers: Option<Vec<PeerInfo>>,
    pub(crate) release: Option<NodeRelease>,
    pub(crate) streaming: bool,
}

/// One endpoint's poll-to-poll mempool change
#[derive(Debug, Clone)]
pub(crate) struct Listing {
    pub(crate) added: Vec<Sighted>,
    pub(crate) removed: Vec<TransactionId>,
}

/// One member's poll, as the fold takes it
#[derive(Debug, Clone)]
pub(crate) enum EndpointReport {
    Observed(Reading, Listing),
    /// Chain counted, mempool off: sightings retracted
    CatchingUp(Reading),
    /// Poll failed, member `Degraded` (sightings kept, holds no tip)
    Failed,
    /// Member `Down`: neither chain nor sightings
    Down,
}

/// The published cell + its watch, and the lock that serialises folds
///
/// - non-generic: the poll fold and readers hold it (the source type stays the balancer's)
/// - `state` = fold working copy (`imbl` throughout: cloning it to publish = `O(1)`)
pub(crate) struct ChainViewCore {
    ask_each_poll: AskEachPoll,
    state: Mutex<ChainViewSnapshot>,
    published: ArcSwap<ChainViewSnapshot>,
    published_tx: watch::Sender<()>,
}

impl ChainViewCore {
    pub(crate) fn new(
        endpoints: Vector<ValidatorMetadata>,
        depth: ReorgDepth,
        ask_each_poll: AskEachPoll,
    ) -> Self {
        let empty = ChainViewSnapshot::empty(endpoints, depth);
        Self {
            ask_each_poll,
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
    pub(crate) fn holds(&self, txid: &TransactionId) -> bool {
        self.published.load().sighting(txid).is_some()
    }

    /// One member's poll folded, published; → txids not admitted (listed, bytes
    /// nowhere: re-listed + re-fetched next poll, never lost)
    pub(crate) fn apply(
        &self,
        endpoint: ValidatorId,
        report: EndpointReport,
    ) -> Vec<TransactionId> {
        let mut guard = self.state.lock().expect("chainview fold mutex poisoned");
        let mut touched: Vec<TransactionId> = Vec::new();
        let mut unadmitted: Vec<TransactionId> = Vec::new();
        let before = Before::of(&guard);

        let state = &mut *guard;
        let Some(meta) = state.endpoints.get_mut(endpoint.get()) else {
            return unadmitted;
        };

        match report {
            EndpointReport::Observed(reading, listing) => {
                meta.health = Health::Live;
                read(meta, &mut state.holders, endpoint, reading);
                for txid in &listing.removed {
                    if let Some(sighting) = state.mempool.get_mut(txid) {
                        sighting.unsight(endpoint);
                        touched.push(*txid);
                    }
                }
                for sighted in listing.added {
                    match (state.mempool.get_mut(&sighted.txid), sighted.raw) {
                        (Some(sighting), _) => {
                            let verified = sighting.timeline().first_trusted.is_some();
                            sighting.sight(endpoint);
                            sighting.listed_fee(sighted.fee);
                            if !verified {
                                telemetry::first_trusted(sighting);
                            }
                        }
                        (None, Some(raw)) => {
                            let overheard = state.overheard.take(&sighted.txid);
                            let mut sighting =
                                Sighting::new(raw, Some(sighted.fee), false, overheard);
                            sighting.sight(endpoint);
                            telemetry::first_trusted(&sighting);
                            state.mempool.insert(sighted.txid, sighting);
                        }
                        // Held at the fetch-once check, gone by the time the fold ran.
                        (None, None) => {
                            unadmitted.push(sighted.txid);
                            continue;
                        }
                    }
                    touched.push(sighted.txid);
                }
            }
            EndpointReport::CatchingUp(reading) => {
                meta.health = Health::CatchingUp;
                read(meta, &mut state.holders, endpoint, reading);
                touched.extend(retract(&mut state.mempool, endpoint));
            }
            EndpointReport::Failed => {
                meta.health = Health::Degraded;
                state.holders.lost(endpoint);
            }
            EndpointReport::Down => {
                meta.health = Health::Down;
                meta.info = None;
                meta.peers = Vector::new();
                state.holders.lost(endpoint);
                touched.extend(retract(&mut state.mempool, endpoint));
            }
        }

        self.settle(guard, before, touched);
        unadmitted
    }

    /// One batch of peer announcements (each stamped on arrival) + the live peer set; publishes
    /// only on a change
    ///
    /// - held txid → its sighting's announcers; else overheard (bounded: `peers.rs`)
    pub(crate) fn apply_heard(
        &self,
        batch: Vec<(Instant, Heard)>,
        live: Vec<SocketAddr>,
        now: Instant,
    ) {
        let mut guard = self.state.lock().expect("chainview fold mutex poisoned");
        let before = Before::of(&guard);
        let state = &mut *guard;
        let live: OrdSet<SocketAddr> = live.into_iter().collect();
        let mut changed = live != state.peers_live;
        state.peers_live = live;
        let overheard = state.overheard.len();
        let mut touched = Vec::new();
        for (at, heard) in batch {
            for txid in heard.txids {
                match state.mempool.get_mut(&txid) {
                    Some(sighting) => {
                        if sighting.hear(heard.peer) {
                            touched.push(txid);
                        }
                    }
                    None => changed |= state.overheard.hear(txid, heard.peer, at),
                }
            }
        }
        state.overheard.expire(now);
        changed |= !touched.is_empty() || state.overheard.len() != overheard;
        if changed {
            self.settle(guard, before, touched);
        }
    }

    /// Header sync's word: the verified chain, who just served a run of it, finality paused
    pub(crate) fn apply_headers(&self, report: HeaderReport) {
        let mut guard = self.state.lock().expect("chainview fold mutex poisoned");
        let before = Before::of(&guard);
        guard.holders.verified(report.verified);
        if let Some((endpoint, block, under)) = report.served {
            guard.holders.served(endpoint, block, under);
        }
        guard.finality_paused = report.finality_paused;
        self.settle(guard, before, Vec::new());
    }

    /// After any change: holders, agreement, alarms, spreads, drops, publish
    fn settle(
        &self,
        mut guard: std::sync::MutexGuard<'_, ChainViewSnapshot>,
        before: Before,
        touched: Vec<TransactionId>,
    ) {
        let state = &mut *guard;
        if cfg!(debug_assertions) {
            state.holders.check();
        }
        state.held_by = state.best().map(|best| state.holders.holders(best)).unwrap_or_default();
        for (index, meta) in state.endpoints.iter_mut().enumerate() {
            let index = ValidatorId::new(index).expect("configured below EndpointSet::MAX");
            meta.agreement = state.holders.agreement(index);
        }
        state.alarms = telemetry::alarms(&state.endpoints, state.finality_paused);

        // a reader joining or leaving can complete anyone's spread; otherwise only `touched`
        let readers = state.mempool_readers();
        let spreading: Vec<TransactionId> = match readers == before.readers {
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

        // unsighted entries do not survive a held-tip move (an `ours` nobody lists after a block =
        // mined or gone)
        let moved = held(state) != before.held;
        let dropped: Vec<TransactionId> = state
            .mempool
            .iter()
            .filter(|(_, sighting)| sighting.trusted().is_empty() && (moved || !sighting.ours()))
            .map(|(txid, _)| *txid)
            .collect();
        for txid in &dropped {
            if let Some(gone) = state.mempool.remove(txid) {
                telemetry::left(&gone, moved);
            }
        }

        let asked = state.holders.asked();
        self.publish(guard);
        if asked != before.asked {
            (self.ask_each_poll)(asked);
        }
    }

    /// Mark a transaction as relayed by us, admitting it before it has propagated (§6).
    pub(crate) fn mark_ours(&self, txid: TransactionId, raw: Bytes) {
        let mut state = self.state.lock().expect("chainview fold mutex poisoned");

        match state.mempool.get_mut(&txid) {
            Some(sighting) => sighting.mark_ours(),
            None => {
                let overheard = state.overheard.take(&txid);
                state.mempool.insert(txid, Sighting::new(raw, None, true, overheard));
            }
        }

        self.publish(state);
    }

    /// Stored under the fold lock (publishes in fold order), then signalled
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

/// An answering poll: claim + held (→ `holders`), clock estimate, peers (a failed peer read
/// keeps the last)
fn read(meta: &mut ValidatorMetadata, holders: &mut Holders, at: ValidatorId, reading: Reading) {
    let claim = BlockRef { hash: reading.info.best_block_hash, height: reading.info.blocks };
    holders.polled(at, claim, reading.held);
    meta.info = Some(reading.info);
    meta.observed_at = Some(std::time::Instant::now());
    meta.streaming = reading.streaming;
    if let Some(peers) = reading.peers {
        meta.peers = peers.into_iter().collect();
    }
    if let Some(release) = reading.release {
        meta.release = Some(release);
    }
}

/// Clear one endpoint's bit from every sighting, returning the txids it held.
fn retract(
    mempool: &mut imbl::OrdMap<TransactionId, Sighting>,
    endpoint: ValidatorId,
) -> Vec<TransactionId> {
    let held: Vec<TransactionId> = mempool
        .iter()
        .filter(|(_, sighting)| sighting.trusted().contains(endpoint))
        .map(|(txid, _)| *txid)
        .collect();
    for txid in &held {
        if let Some(sighting) = mempool.get_mut(txid) {
            sighting.unsight(endpoint);
        }
    }
    held
}
