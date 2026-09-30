//! The fold: per-endpoint deltas in, one published snapshot out.
//!
//! - Publish **before** waking tails (a woken tail must read what woke it)

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use bytes::Bytes;
use imbl::Vector;
use tokio::sync::watch;
use zaino_primitives::types::{Height, PeerInfo, TransactionId, Zatoshis};

use crate::chain::EndpointChain;
use crate::endpoints::{Agreement, EndpointIndex, EndpointState, ValidatorMetadata};
use crate::quorum::{tally, Quorum, QuorumTip};
use crate::snapshot::{ChainViewSnapshot, Sighting};
use crate::telemetry;

/// One txid a poller listed, with bytes iff this poller had to fetch them.
///
/// `raw` is `None` when the view already held the transaction — bytes cost a round trip and
/// paying it per endpoint per transaction would be N× waste (§3).
#[derive(Debug, Clone)]
pub(crate) struct Sighted {
    pub(crate) txid: TransactionId,
    pub(crate) raw: Option<Bytes>,
    pub(crate) fee: Zatoshis,
}

/// What every answering tick read, mempool on or off
#[derive(Debug, Clone)]
pub(crate) struct Reading {
    /// Poller's held chain after this tick's walk (`None` until one completes)
    pub(crate) chain: Option<EndpointChain>,
    pub(crate) estimated_height: Height,
    pub(crate) latency: Duration,
    /// `Some` only on a peer-refresh tick the validator answered
    pub(crate) peers: Option<Vec<PeerInfo>>,
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
    /// Node says it is not ready to report a tip: polled, never counted.
    Syncing,
    /// Chain counted, mempool off: sightings retracted
    CatchingUp(Reading),
    /// Transport failure, still on the ladder (last observation retained, no vote)
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
    /// Level-triggered quorum tip (`agreed_by` changes too: fetch routing reads it)
    tip: watch::Sender<Option<QuorumTip>>,
    /// Sent iff the epoch moved or an arrival landed (tails sleep through every other fold)
    tails: watch::Sender<()>,
    quorum: Quorum,
}

impl ChainViewCore {
    pub(crate) fn new(endpoints: Vector<ValidatorMetadata>, quorum: Quorum) -> Self {
        let empty = ChainViewSnapshot::empty(endpoints, quorum);
        Self {
            state: Mutex::new(empty.clone()),
            published: ArcSwap::from_pointee(empty),
            tip: watch::Sender::new(None),
            tails: watch::Sender::new(()),
            quorum,
        }
    }

    pub(crate) fn current(&self) -> Arc<ChainViewSnapshot> {
        self.published.load_full()
    }

    pub(crate) fn subscribe_tip(&self) -> watch::Receiver<Option<QuorumTip>> {
        self.tip.subscribe()
    }

    pub(crate) fn subscribe_tails(&self) -> watch::Receiver<()> {
        self.tails.subscribe()
    }

    pub(crate) fn quorum(&self) -> Quorum {
        self.quorum
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
        let was_servable = servable_set(&guard, &self.quorum);
        let (previous_tip, previous_alarms) = (guard.tip, guard.alarms);

        let state = &mut *guard;
        let Some(meta) = state.endpoints.get_mut(endpoint.get()) else {
            return unadmitted;
        };

        match report {
            EndpointReport::Observed(reading, listing) => {
                meta.state = EndpointState::Live;
                read(meta, reading);
                for txid in &listing.removed {
                    if let Some(sighting) = state.mempool.get_mut(txid) {
                        sighting.unsight(endpoint);
                        touched.push(*txid);
                    }
                }
                for sighted in listing.added {
                    match (state.mempool.get_mut(&sighted.txid), sighted.raw) {
                        (Some(sighting), _) => {
                            sighting.sight(endpoint);
                            sighting.listed_fee(sighted.fee);
                        }
                        (None, Some(raw)) => {
                            let mut sighting = Sighting::new(raw, Some(sighted.fee), false);
                            sighting.sight(endpoint);
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
            EndpointReport::Syncing => {
                meta.state = EndpointState::Syncing;
                touched.extend(retract(&mut state.mempool, endpoint));
            }
            EndpointReport::CatchingUp(reading) => {
                meta.state = EndpointState::CatchingUp;
                read(meta, reading);
                touched.extend(retract(&mut state.mempool, endpoint));
            }
            EndpointReport::Failed { consecutive } => {
                meta.state = EndpointState::Degraded;
                meta.failures = consecutive;
            }
            EndpointReport::Down => {
                meta.state = EndpointState::Down;
                meta.chain = None;
                meta.peers = Vector::new();
                touched.extend(retract(&mut state.mempool, endpoint));
            }
        }

        let voters = state.endpoints.iter().enumerate().filter(|(_, meta)| meta.state.votes());
        let counted = tally(
            self.quorum,
            voters.filter_map(|(index, meta)| {
                Some((EndpointIndex::new(index)?, meta.chain.as_ref()?))
            }),
        );
        (state.tip, state.agreeing) = (counted.tip, counted.agreeing);
        let agreer = counted
            .tip
            .and_then(|tip| tip.agreed_by.positions().next())
            .and_then(|position| state.endpoints.get(position)?.chain.clone());
        for meta in state.endpoints.iter_mut() {
            meta.agreement = match (counted.tip, &meta.chain, &agreer) {
                (Some(quorum), Some(theirs), Some(agreer)) => {
                    Agreement::of(theirs, quorum.block, agreer)
                }
                _ => Agreement::Unknown,
            };
        }
        state.alarms = telemetry::alarms(&state.endpoints);

        let tip_changed = state.tip != previous_tip;
        let tip_moved = state.tip.map(|tip| tip.block) != previous_tip.map(|tip| tip.block);
        // Unsighted entries do not survive a tip move (an `ours` nobody lists after a block =
        // mined or gone).
        let dropped: Vec<TransactionId> = state
            .mempool
            .iter()
            .filter(|(_, sighting)| {
                sighting.seen_at().is_empty() && (tip_moved || !sighting.ours())
            })
            .map(|(txid, _)| *txid)
            .collect();
        for txid in &dropped {
            state.mempool.remove(txid);
        }

        if tip_moved {
            state.tip_moved();
        }
        let new_tip = state.tip;
        let arrived = self.record_arrivals(state, &touched, &was_servable);
        let published = self.publish(guard, tip_changed.then_some(new_tip), tip_moved || arrived);
        telemetry::emit(&published, previous_alarms);

        unadmitted
    }

    /// Mark a transaction as relayed by us, admitting it before it has propagated (§5).
    pub(crate) fn mark_ours(&self, txid: TransactionId, raw: Bytes) {
        let mut state = self.state.lock().expect("chainview fold mutex poisoned");
        let was_servable = servable_set(&state, &self.quorum);

        match state.mempool.get_mut(&txid) {
            Some(sighting) => sighting.mark_ours(),
            None => {
                state.mempool.insert(txid, Sighting::new(raw, None, true));
            }
        }

        let arrived = self.record_arrivals(&mut state, &[txid], &was_servable);
        self.publish(state, None, arrived);
    }

    /// Appends every touched txid that crossed into servable; `true` = any did
    fn record_arrivals(
        &self,
        state: &mut ChainViewSnapshot,
        touched: &[TransactionId],
        was_servable: &imbl::OrdSet<TransactionId>,
    ) -> bool {
        let crossed: Vec<TransactionId> = touched
            .iter()
            .filter(|txid| {
                !was_servable.contains(*txid)
                    && state.sighting(txid).is_some_and(|sighting| sighting.servable(&self.quorum))
            })
            .copied()
            .collect();
        for txid in &crossed {
            state.arrived(*txid);
        }
        !crossed.is_empty()
    }

    /// Store, then signal (a woken reader must find what woke it)
    ///
    /// - `tip = Some(_)` = the quorum tip (block or agreers) changed to it
    fn publish(
        &self,
        state: std::sync::MutexGuard<'_, ChainViewSnapshot>,
        tip: Option<Option<QuorumTip>>,
        wake_tails: bool,
    ) -> Arc<ChainViewSnapshot> {
        let published = Arc::new(state.clone());
        drop(state);
        self.published.store(Arc::clone(&published));
        if let Some(tip) = tip {
            self.tip.send_replace(tip);
        }
        if wake_tails {
            self.tails.send_replace(());
        }
        published
    }
}

impl std::fmt::Debug for ChainViewCore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pinned = self.published.load();
        f.debug_struct("ChainViewCore")
            .field("tip", &pinned.tip())
            .field("quorum", &self.quorum)
            .finish_non_exhaustive()
    }
}

/// An answering tick: chain, clock estimate, latency, peers (a failed peer read keeps the last)
fn read(meta: &mut ValidatorMetadata, reading: Reading) {
    meta.failures = 0;
    meta.chain = reading.chain;
    meta.estimated_height = Some(reading.estimated_height);
    meta.observed_at = Some(Instant::now());
    meta.latency.observe(reading.latency);
    if let Some(peers) = reading.peers {
        meta.peers = peers.into_iter().collect();
    }
}

/// Clear one endpoint's bit from every sighting, returning the txids it held.
fn retract(
    mempool: &mut imbl::OrdMap<TransactionId, Sighting>,
    endpoint: EndpointIndex,
) -> Vec<TransactionId> {
    let held: Vec<TransactionId> = mempool
        .iter()
        .filter(|(_, sighting)| sighting.seen_at().contains(endpoint))
        .map(|(txid, _)| *txid)
        .collect();
    for txid in &held {
        if let Some(sighting) = mempool.get_mut(txid) {
            sighting.unsight(endpoint);
        }
    }
    held
}

fn servable_set(state: &ChainViewSnapshot, quorum: &Quorum) -> imbl::OrdSet<TransactionId> {
    state
        .sightings()
        .filter(|(_, sighting)| sighting.servable(quorum))
        .map(|(txid, _)| *txid)
        .collect()
}
