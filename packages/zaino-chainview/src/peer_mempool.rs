//! Peer quorum over mempools: each live outbound peer's listing per round, what the quorum admits
//! checked here, admitted into the view (`docs/architecture.md` §5)
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
use zaino_primitives::types::PeerTxId;
use zaino_source::decode_transaction;
use zcash_protocol::local_consensus::LocalNetwork;
use zcash_protocol::value::Zatoshis;
use zcash_transparent::bundle::OutPoint;

use crate::admission::{admissible, NextBlock, Refused};
use crate::chain_view_snapshot::ChainViewSnapshot;
use crate::fold::{Candidate, ChainViewCore};
use crate::listings::PEER_MEMPOOL_POLL;
use crate::ports::{PeerSource, UnspentOutputs};

/// A transaction one of our checks refused stays refused this long
pub(crate) const REFUSED_TTL: Duration = Duration::from_secs(600);

/// Polls the peers' mempools into the view until cancelled; spawn [`run`](Self::run)
pub struct PeerMempool {
    core: Arc<ChainViewCore>,
    peers: Arc<dyn PeerSource>,
    utxos: Arc<dyn UnspentOutputs>,
    schedule: LocalNetwork,
}

impl PeerMempool {
    pub(crate) fn new(
        core: Arc<ChainViewCore>,
        peers: Arc<dyn PeerSource>,
        utxos: Arc<dyn UnspentOutputs>,
        schedule: LocalNetwork,
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
        for (id, raw, judged) in
            judge(&view, self.utxos.as_ref(), next, self.bytes(candidates).await)
        {
            match judged {
                Ok(fee) => admitted.push((id, raw, fee)),
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
}

/// Each candidate's bytes against `next`, `utxos` and `view`'s held transactions → its fee, or why
/// not (`Refused::retried` = maybe a later round)
pub(crate) fn judge(
    view: &ChainViewSnapshot,
    utxos: &dyn UnspentOutputs,
    next: NextBlock,
    fetched: Vec<(PeerTxId, Bytes)>,
) -> Vec<(PeerTxId, Bytes, Result<Zatoshis, Refused>)> {
    let prevouts = |inputs: &[OutPoint]| {
        let served = utxos.values(inputs).unwrap_or_else(|| vec![None; inputs.len()]);
        let value = |(at, served): (&OutPoint, Option<Zatoshis>)| served.or_else(|| held(view, at));
        inputs.iter().zip(served).map(value).collect()
    };
    let judged = |(id, raw): (PeerTxId, Bytes)| {
        let fee = admissible(&raw, next, prevouts).map(|(_, fee)| fee);
        (id, raw, fee)
    };
    fetched.into_iter().map(judged).collect()
}

/// `at` among the held transactions' outputs (a parent still unmined)
fn held(view: &ChainViewSnapshot, at: &OutPoint) -> Option<Zatoshis> {
    let parent = view.mempool.get(at.txid())?;
    let tx = decode_transaction(parent.raw()).ok()?;
    let index = usize::try_from(at.n()).ok()?;
    tx.transparent.outputs.get(index).map(|output| output.value)
}
