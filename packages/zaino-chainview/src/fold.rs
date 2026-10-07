//! The fold: per-endpoint deltas in, one published snapshot out.
//!
//! - Publish **before** waking tails (a woken tail must read what woke it)

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arc_swap::ArcSwap;
use bytes::Bytes;
use imbl::{OrdSet, Vector};
use tokio::sync::watch;
use tokio::time::Instant;
use zaino_header_chain::VerifiedChain;
use zaino_primitives::types::{
    BlockRef, BlockchainInfo, NodeRelease, PeerInfo, ReorgDepth, TransactionId, Zatoshis,
};

use crate::endpoints::{EndpointIndex, EndpointSet, EndpointState, ValidatorMetadata};
use crate::feed::Epoch;
use crate::holders::Holders;
use crate::ports::Heard;
use crate::snapshot::{ChainViewSnapshot, MempoolView, Sighting};
use crate::telemetry::{self, Alarms};
use crate::tip::{ChainTip, Unserved};

/// What a fold compares against once it has changed the state
struct Before {
    tip: Option<ChainTip>,
    alarms: Alarms,
    readers: EndpointSet,
}

impl Before {
    fn of(state: &ChainViewSnapshot) -> Self {
        Self { tip: state.tip, alarms: state.alarms, readers: state.mempool_readers() }
    }
}

/// Header sync → view: `served` = a validator whose best chain just served a header run ending at
/// that block
#[derive(Debug, Clone)]
pub(crate) struct HeaderReport {
    pub(crate) verified: Option<Arc<VerifiedChain>>,
    pub(crate) served: Option<(EndpointIndex, BlockRef)>,
    pub(crate) finality_paused: bool,
}

/// Tip (block or holders) carried by one publish
enum TipUpdate {
    Unchanged,
    Set(Option<ChainTip>),
}

/// One txid a poller listed, with bytes iff this poller had to fetch them.
///
/// `raw` is `None` when the view already held the transaction — bytes cost a round trip and
/// paying it per endpoint per transaction would be N× waste (§5).
#[derive(Debug, Clone)]
pub(crate) struct Sighted {
    pub(crate) txid: TransactionId,
    pub(crate) raw: Option<Bytes>,
    pub(crate) fee: Zatoshis,
}

/// What every answering tick read, mempool on or off
///
/// - `held` = its `getblockhash` answers at the asked heights (`holders.rs`)
#[derive(Debug, Clone)]
pub(crate) struct Reading {
    pub(crate) held: Vec<BlockRef>,
    pub(crate) info: BlockchainInfo,
    /// Poll batch round trip
    pub(crate) latency: Duration,
    /// `Some` only on a metadata tick whose half answered (else the last is kept)
    pub(crate) peers: Option<Vec<PeerInfo>>,
    pub(crate) release: Option<NodeRelease>,
    /// Push stream up as of this tick
    pub(crate) streaming: bool,
}

/// One endpoint's poll-to-poll mempool change
#[derive(Debug, Clone)]
pub(crate) struct Listing {
    pub(crate) added: Vec<Sighted>,
    pub(crate) removed: Vec<TransactionId>,
}

/// What one poller has to say this tick.
#[derive(Debug, Clone)]
pub(crate) enum EndpointReport {
    Observed(Reading, Listing),
    /// Chain counted, mempool off: sightings retracted
    CatchingUp(Reading),
    /// Transport failure, still on the ladder (last observation retained, holds no tip)
    Failed {
        consecutive: u32,
    },
    /// Ejected. Contributes neither chain nor sightings.
    Down,
}

/// The published cell, the tail wake, and the lock that serialises folds.
///
/// Non-generic on purpose: pollers and readers hold this, and only the broadcast fan-out needs
/// to know the endpoint source type.
pub(crate) struct ChainViewCore {
    /// Fold working copy. `imbl` throughout, so cloning it to publish is `O(1)`.
    state: Mutex<ChainViewSnapshot>,
    published: ArcSwap<ChainViewSnapshot>,
    /// Level-triggered tip (`held_by` changes too: fetch routing reads it)
    tip: watch::Sender<Option<ChainTip>>,
    /// Sent iff the epoch moved or an arrival landed (tails sleep through every other fold)
    tails: watch::Sender<()>,
    /// Sent on every publish (a submission watching for its transaction's spread)
    published_tx: watch::Sender<()>,
    /// Feed for the current tip block, or why there is none; written only under `state`'s lock
    epoch: ArcSwap<Result<Arc<Epoch>, Unserved>>,
}

impl ChainViewCore {
    pub(crate) fn new(endpoints: Vector<ValidatorMetadata>, depth: ReorgDepth) -> Self {
        let empty = ChainViewSnapshot::empty(endpoints, depth);
        let unserved = empty.mempool().expect_err("an empty view has no tip");
        Self {
            state: Mutex::new(empty.clone()),
            published: ArcSwap::from_pointee(empty),
            tip: watch::Sender::new(None),
            tails: watch::Sender::new(()),
            published_tx: watch::Sender::new(()),
            epoch: ArcSwap::from_pointee(Err(unserved)),
        }
    }

    pub(crate) fn current(&self) -> Arc<ChainViewSnapshot> {
        self.published.load_full()
    }

    pub(crate) fn subscribe_tip(&self) -> watch::Receiver<Option<ChainTip>> {
        self.tip.subscribe()
    }

    pub(crate) fn subscribe_tails(&self) -> watch::Receiver<()> {
        self.tails.subscribe()
    }

    pub(crate) fn subscribe_published(&self) -> watch::Receiver<()> {
        self.published_tx.subscribe()
    }

    /// Does the view already hold this transaction's bytes?
    ///
    /// The fetch-once gate: a poller listing a known txid reports it without a round trip.
    pub(crate) fn holds(&self, txid: &TransactionId) -> bool {
        self.published.load().sighting(txid).is_some()
    }

    /// Fold one endpoint's report, publish, then wake tails.
    ///
    /// Returns the txids it could not admit — listed with no bytes anywhere — so the poller
    /// re-lists and re-fetches them next tick rather than losing them.
    pub(crate) fn apply(
        &self,
        endpoint: EndpointIndex,
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
                meta.state = EndpointState::Live;
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
                meta.state = EndpointState::CatchingUp;
                read(meta, &mut state.holders, endpoint, reading);
                touched.extend(retract(&mut state.mempool, endpoint));
            }
            EndpointReport::Failed { consecutive } => {
                meta.state = EndpointState::Degraded;
                meta.failures = consecutive;
                state.holders.lost(endpoint);
            }
            EndpointReport::Down => {
                meta.state = EndpointState::Down;
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
        if let Some((endpoint, block)) = report.served {
            guard.holders.served(endpoint, block);
        }
        guard.finality_paused = report.finality_paused;
        self.settle(guard, before, Vec::new());
    }

    /// After any change: tip + holders, agreement, alarms, spreads, drops, epoch, publish
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
        let best = state.holders.best();
        let held_by = best.map(|best| state.holders.holders(best)).unwrap_or_default();
        state.tip = best.filter(|_| !held_by.is_empty()).map(|block| ChainTip { block, held_by });
        for (index, meta) in state.endpoints.iter_mut().enumerate() {
            let index = EndpointIndex::new(index).expect("configured below EndpointSet::MAX");
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

        let tip_changed = state.tip != before.tip;
        let tip_moved = state.tip.map(|tip| tip.block) != before.tip.map(|tip| tip.block);
        // Unsighted entries do not survive a tip move (an `ours` nobody lists after a block =
        // mined or gone).
        let dropped: Vec<TransactionId> = state
            .mempool
            .iter()
            .filter(|(_, sighting)| {
                sighting.trusted().is_empty() && (tip_moved || !sighting.ours())
            })
            .map(|(txid, _)| *txid)
            .collect();
        for txid in &dropped {
            if let Some(gone) = state.mempool.remove(txid) {
                telemetry::left(&gone, tip_moved);
            }
        }

        if tip_moved {
            self.rotate(state);
        } else if let Err(unserved) = state.mempool() {
            // the refusal stays current as holders come and go
            self.epoch.store(Arc::new(Err(unserved)));
        }
        let new_tip = state.tip;
        let arrived = self.record_arrivals(state, &touched);
        let tip = match tip_changed {
            true => TipUpdate::Set(new_tip),
            false => TipUpdate::Unchanged,
        };
        let published = self.publish(guard, tip, tip_moved || arrived);
        telemetry::emit(&published, before.alarms);
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

        let arrived = self.record_arrivals(&mut state, &[txid]);
        self.publish(state, TipUpdate::Unchanged, arrived);
    }

    /// New tip block: the old epoch sealed (its tails drain, then end), a new one opened on the
    /// servable mempool (none while unserved)
    fn rotate(&self, state: &ChainViewSnapshot) {
        let opened =
            state.mempool().map(|mempool| Arc::new(Epoch::open(mempool.entries().collect())));
        if let Ok(sealed) = self.epoch.swap(Arc::new(opened)).as_ref() {
            sealed.seal();
        }
    }

    /// Logs every touched txid that crossed into servable; `true` = any did
    ///
    /// - before = the last published view (every fold publishes under this lock): a point
    ///   lookup per touched txid, never a pass over the mempool
    fn record_arrivals(&self, state: &mut ChainViewSnapshot, touched: &[TransactionId]) -> bool {
        let before = self.published.load();
        let epoch = self.epoch.load();
        let mut arrived = false;
        for txid in touched {
            let was = before.sighting(txid).is_some_and(|sighting| sighting.servable());
            let now = MempoolView::of(state).get(txid);
            if let (false, Some(entry), Ok(epoch)) = (was, now, epoch.as_ref()) {
                epoch.append(entry);
                arrived = true;
            }
        }
        arrived
    }

    /// Feed for the current tip block, or why there is none
    pub(crate) fn epoch(&self) -> Result<Arc<Epoch>, Unserved> {
        self.epoch.load().as_ref().clone()
    }

    /// Store, then signal (a woken reader must find what woke it)
    fn publish(
        &self,
        state: std::sync::MutexGuard<'_, ChainViewSnapshot>,
        tip: TipUpdate,
        wake_tails: bool,
    ) -> Arc<ChainViewSnapshot> {
        let published = Arc::new(state.clone());
        drop(state);
        self.published.store(Arc::clone(&published));
        if let TipUpdate::Set(tip) = tip {
            self.tip.send_replace(tip);
        }
        if wake_tails {
            self.tails.send_replace(());
        }
        self.published_tx.send_replace(());
        published
    }
}

impl std::fmt::Debug for ChainViewCore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pinned = self.published.load();
        f.debug_struct("ChainViewCore").field("tip", &pinned.tip()).finish_non_exhaustive()
    }
}

/// An answering tick: claim + held (→ `holders`), clock estimate, latency, peers (a failed peer
/// read keeps the last)
fn read(meta: &mut ValidatorMetadata, holders: &mut Holders, at: EndpointIndex, reading: Reading) {
    let claim = BlockRef { hash: reading.info.best_block_hash, height: reading.info.blocks };
    holders.polled(at, claim, reading.held);
    meta.failures = 0;
    meta.info = Some(reading.info);
    meta.observed_at = Some(std::time::Instant::now());
    meta.streaming = reading.streaming;
    meta.latency.observe(reading.latency);
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
    endpoint: EndpointIndex,
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
