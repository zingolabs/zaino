//! The fold: per-endpoint deltas in, one published snapshot out.
//!
//! - Publish **before** waking tails (a woken tail must read what woke it)

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use bytes::Bytes;
use imbl::Vector;
use tokio::sync::watch;
use zaino_primitives::types::{BlockRef, TransactionId, Zatoshis};

use crate::chain::EndpointChain;
use crate::endpoints::{Agreement, EndpointIndex, EndpointSet, EndpointState, ValidatorMetadata};
use crate::quorum::{Quorum, QuorumTip};
use crate::snapshot::{ChainViewSnapshot, Sighting};

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
    pub(crate) latency: Duration,
    /// `Some` only on a peer-refresh tick
    pub(crate) peers: Option<Vec<String>>,
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
    /// Ejected. Contributes neither tip nor sightings.
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
    /// Level-triggered quorum tip
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
        let mut state = self.state.lock().expect("chainview fold mutex poisoned");
        let mut touched: Vec<TransactionId> = Vec::new();
        let mut unadmitted: Vec<TransactionId> = Vec::new();
        let was_servable = servable_set(&state, &self.quorum);
        let previous_tip = state.tip();

        let (tip, mempool, endpoints) = state.parts_mut();
        let Some(meta) = endpoints.get_mut(endpoint.get()) else {
            return unadmitted;
        };

        match report {
            EndpointReport::Observed(reading, listing) => {
                meta.state = EndpointState::Live;
                read(meta, reading);
                for txid in &listing.removed {
                    if let Some(sighting) = mempool.get_mut(txid) {
                        sighting.unsight(endpoint);
                        touched.push(*txid);
                    }
                }
                for sighted in listing.added {
                    match (mempool.get_mut(&sighted.txid), sighted.raw) {
                        (Some(sighting), _) => {
                            sighting.sight(endpoint);
                            sighting.listed_fee(sighted.fee);
                        }
                        (None, Some(raw)) => {
                            let mut sighting = Sighting::new(raw, Some(sighted.fee), false);
                            sighting.sight(endpoint);
                            mempool.insert(sighted.txid, sighting);
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
                touched.extend(retract(mempool, endpoint));
            }
            EndpointReport::CatchingUp(reading) => {
                meta.state = EndpointState::CatchingUp;
                read(meta, reading);
                touched.extend(retract(mempool, endpoint));
            }
            EndpointReport::Failed { consecutive } => {
                meta.state = EndpointState::Degraded;
                meta.failures = consecutive;
            }
            EndpointReport::Down => {
                meta.state = EndpointState::Down;
                meta.chain = None;
                meta.peers = Vector::new();
                touched.extend(retract(mempool, endpoint));
            }
        }

        *tip = quorum_tip(endpoints, self.quorum);
        for meta in endpoints.iter_mut() {
            meta.agreement = match (*tip, meta.tip()) {
                (Some(quorum), Some(theirs)) if quorum.block == theirs => Agreement::Agreed,
                (Some(_), Some(_)) => Agreement::Diverged,
                _ => Agreement::Unknown,
            };
        }

        let tip_moved = *tip != previous_tip;
        let new_tip = *tip;
        // Unsighted entries do not survive a tip move (an `ours` nobody lists after a block =
        // mined or gone).
        let dropped: Vec<TransactionId> = mempool
            .iter()
            .filter(|(_, sighting)| {
                sighting.seen_at().is_empty() && (tip_moved || !sighting.ours())
            })
            .map(|(txid, _)| *txid)
            .collect();
        for txid in &dropped {
            mempool.remove(txid);
        }

        if tip_moved {
            state.tip_moved();
        }
        let arrived = self.record_arrivals(&mut state, &touched, &was_servable);
        self.publish(state, tip_moved.then_some(new_tip), tip_moved || arrived);

        unadmitted
    }

    /// Mark a transaction as relayed by us, admitting it before it has propagated (§5).
    pub(crate) fn mark_ours(&self, txid: TransactionId, raw: Bytes) {
        let mut state = self.state.lock().expect("chainview fold mutex poisoned");
        let was_servable = servable_set(&state, &self.quorum);

        let (_, mempool, _) = state.parts_mut();
        match mempool.get_mut(&txid) {
            Some(sighting) => sighting.mark_ours(),
            None => {
                mempool.insert(txid, Sighting::new(raw, None, true));
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
    /// - `tip = Some(_)` = the quorum tip moved to it
    fn publish(
        &self,
        state: std::sync::MutexGuard<'_, ChainViewSnapshot>,
        tip: Option<Option<QuorumTip>>,
        wake_tails: bool,
    ) {
        let published = Arc::new(state.clone());
        drop(state);
        self.published.store(published);
        if let Some(tip) = tip {
            self.tip.send_replace(tip);
        }
        if wake_tails {
            self.tails.send_replace(());
        }
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

/// An answering tick: chain, latency, peers
fn read(meta: &mut ValidatorMetadata, reading: Reading) {
    meta.failures = 0;
    meta.chain = reading.chain;
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

/// Highest block ≥threshold *voting* endpoints report with the same hash.
///
/// Never the maximum height: one node claiming 999,999 agrees with nobody.
fn quorum_tip(endpoints: &Vector<ValidatorMetadata>, quorum: Quorum) -> Option<QuorumTip> {
    let mut votes: HashMap<BlockRef, EndpointSet> = HashMap::new();
    for (index, meta) in endpoints.iter().enumerate() {
        let (Some(block), Some(index)) = (meta.tip(), EndpointIndex::new(index)) else {
            continue;
        };
        if meta.state.votes() {
            votes.entry(block).or_default().insert(index);
        }
    }

    votes
        .into_iter()
        .filter(|(_, agreed_by)| quorum.met_by(*agreed_by))
        .max_by_key(|(block, _)| block.height)
        .map(|(block, agreed_by)| QuorumTip { block, agreed_by })
}

fn servable_set(state: &ChainViewSnapshot, quorum: &Quorum) -> imbl::OrdSet<TransactionId> {
    state
        .sightings()
        .filter(|(_, sighting)| sighting.servable(quorum))
        .map(|(txid, _)| *txid)
        .collect()
}
