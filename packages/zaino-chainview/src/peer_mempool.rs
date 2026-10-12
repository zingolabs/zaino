//! Peer quorum over mempools: each live outbound peer's listing per round, what the quorum admits
//! checked here, admitted into the view (`chainview.md`)
//!
//! ```text
//!   each PEER_MEMPOOL_POLL ─▶ mempool(peer), every live peer ─▶ fold: listings, retention
//!     ─▶ candidates ─▶ bytes (held, else by wtxid from peers) ─▶ admission.rs ─▶ admitted
//! ```
//!
//! - a consensus refusal kept [`REFUSED_TTL`] (never re-fetched each round); an unknown prevout
//!   retried next round (the served tip, or its parent's admission, catching up)

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::future::join_all;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};
use zaino_primitives::types::{OutPoint, PeerTxId, UpgradeSchedule, Zatoshis};
use zaino_source::decode_transaction;

use crate::admission::{admissible, NextBlock};
use crate::fold::{Candidate, ChainViewCore};
use crate::listings::PEER_MEMPOOL_POLL;
use crate::ports::{UnspentOutputs, ValidatorP2pSource};
use crate::snapshot::ChainViewSnapshot;

/// A transaction one of our checks refused stays refused this long
pub(crate) const REFUSED_TTL: Duration = Duration::from_secs(600);

/// Polls the peers' mempools into the view until cancelled; spawn [`run`](Self::run)
pub struct PeerMempool {
    core: Arc<ChainViewCore>,
    peers: Arc<dyn ValidatorP2pSource>,
    utxos: Arc<dyn UnspentOutputs>,
    schedule: UpgradeSchedule,
}

impl PeerMempool {
    pub(crate) fn new(
        core: Arc<ChainViewCore>,
        peers: Arc<dyn ValidatorP2pSource>,
        utxos: Arc<dyn UnspentOutputs>,
        schedule: UpgradeSchedule,
    ) -> Self {
        Self { core, peers, utxos, schedule }
    }

    pub async fn run(self, cancel: CancellationToken) {
        let mut tick = tokio::time::interval(PEER_MEMPOOL_POLL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut refused: HashMap<PeerTxId, Instant> = HashMap::new();
        loop {
            tokio::select! {
                () = cancel.cancelled() => return,
                _ = tick.tick() => self.round(&mut refused).await,
            }
        }
    }

    async fn round(&self, refused: &mut HashMap<PeerTxId, Instant>) {
        let live = self.peers.live();
        let answers = join_all(live.iter().map(|peer| self.peers.mempool(*peer))).await;
        let answers = live.into_iter().zip(answers).map(|(peer, answer)| {
            if let Err(cause) = &answer {
                debug!(%peer, %cause, "Peer mempool unanswered");
            }
            (peer, answer.ok())
        });
        let now = Instant::now();
        refused.retain(|_, at| now.duration_since(*at) < REFUSED_TTL);
        let candidates = self.core.apply_listings(answers.collect(), now);
        let candidates: Vec<Candidate> =
            candidates.into_iter().filter(|(id, _)| !refused.contains_key(id)).collect();
        let view = self.core.current();
        let Some(best) = view.best() else { return };
        if candidates.is_empty() {
            return;
        }
        let next = NextBlock::above(best.height, &self.schedule);
        let mut admitted = Vec::new();
        for (id, raw) in self.bytes(candidates).await {
            match admissible(&raw, next, |at| self.prevout(&view, at)) {
                Ok((_, fee)) => admitted.push((id, raw, fee)),
                Err(why) if why.retried() => {
                    debug!(txid = %id.txid, %why, "Quorum transaction waits")
                }
                Err(why) => {
                    warn!(txid = %id.txid, %why, "Quorum transaction refused");
                    refused.insert(id, now);
                }
            }
        }
        self.core.admit_by_quorum(admitted);
    }

    /// Held bytes as they are; the rest asked of the peers, one request
    async fn bytes(&self, candidates: Vec<Candidate>) -> Vec<(PeerTxId, Bytes)> {
        let (held, missing): (Vec<Candidate>, Vec<Candidate>) =
            candidates.into_iter().partition(|(_, raw)| raw.is_some());
        let mut found: Vec<(PeerTxId, Bytes)> =
            held.into_iter().filter_map(|(id, raw)| Some((id, raw?))).collect();
        let ids: Vec<PeerTxId> = missing.into_iter().map(|(id, _)| id).collect();
        if ids.is_empty() {
            return found;
        }
        match self.peers.transactions(ids.clone()).await {
            Ok(raws) => {
                found.extend(ids.into_iter().zip(raws).filter_map(|(id, raw)| Some((id, raw?))))
            }
            Err(cause) => debug!(%cause, "Quorum transactions unanswered"),
        }
        found
    }

    /// Unspent value: the served UTXO set, else a servable held transaction's output
    fn prevout(&self, view: &ChainViewSnapshot, at: &OutPoint) -> Option<Zatoshis> {
        let served = self.utxos.values(std::slice::from_ref(at));
        match served.and_then(|values| values.into_iter().next().flatten()) {
            Some(value) => Some(value),
            None => held_output(view, at),
        }
    }
}

/// `at` among the servable held transactions' outputs (a parent still unmined)
fn held_output(view: &ChainViewSnapshot, at: &OutPoint) -> Option<Zatoshis> {
    let parent = view.mempool.get(&at.txid).filter(|sighting| sighting.servable())?;
    let tx = decode_transaction(parent.raw()).ok()?;
    let index = usize::try_from(at.vout).ok()?;
    tx.transparent.outputs.get(index).map(|output| output.value)
}
