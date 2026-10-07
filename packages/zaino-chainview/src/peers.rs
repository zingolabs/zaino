//! Peers' transaction announcements into the view (§5): `peers: x/y`, and submission's watch (§6)
//!
//! ```text
//!   ValidatorP2pSource::heard ─▶ batch ─(each PEER_FOLD)─▶ fold: held txid → sighting's announcers
//!                                                            else → Overheard (bounded, expiring)
//!   sighting starts (trusted listing / ours) ◀── takes its Overheard announcers
//! ```
//!
//! - `inv` = an event, not a listing: an announcer never leaves a sighting; `seen` counts only
//!   announcers live now
//! - Overheard bounded per peer + by age (an announcement costs a peer nothing)

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use imbl::{OrdMap, OrdSet, Vector};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::warn;
use zaino_primitives::types::TransactionId;

use crate::fold::ChainViewCore;
use crate::ports::{Heard, ValidatorP2pSource};

/// Announcements folded at most this often (one publish per batch, not per `inv`)
pub(crate) const PEER_FOLD: Duration = Duration::from_millis(250);

/// Overheard txid kept this long (peer `inv` → trusted listing = seconds)
pub(crate) const OVERHEARD_TTL: Duration = Duration::from_secs(60);

/// Overheard txids one peer may hold at once (one peer cannot crowd out the rest)
pub(crate) const OVERHEARD_PER_PEER: usize = 2_000;

/// Announcers of txids the view does not hold yet; `imbl` (published with the snapshot)
///
/// - `order` = insertion = age order (expiry pops the front)
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Overheard {
    by_txid: OrdMap<TransactionId, Pending>,
    order: Vector<(Instant, TransactionId)>,
    per_peer: OrdMap<SocketAddr, usize>,
}

/// One overheard txid's first announcement and every announcer since
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Pending {
    pub(crate) first: Instant,
    pub(crate) peers: OrdSet<SocketAddr>,
}

impl Overheard {
    /// `false` = `peer` at its `OVERHEARD_PER_PEER` (dropped)
    pub(crate) fn hear(&mut self, txid: TransactionId, peer: SocketAddr, now: Instant) -> bool {
        let held = self.per_peer.get(&peer).copied().unwrap_or(0);
        let fresh = !self.by_txid.get(&txid).is_some_and(|pending| pending.peers.contains(&peer));
        if !fresh {
            return true;
        }
        if held >= OVERHEARD_PER_PEER {
            return false;
        }
        self.per_peer.insert(peer, held + 1);
        match self.by_txid.get_mut(&txid) {
            Some(pending) => drop(pending.peers.insert(peer)),
            None => {
                self.by_txid.insert(txid, Pending { first: now, peers: OrdSet::unit(peer) });
                self.order.push_back((now, txid));
            }
        }
        true
    }

    /// Its announcers so far, removed (a sighting starts)
    pub(crate) fn take(&mut self, txid: &TransactionId) -> Option<Pending> {
        let pending = self.by_txid.remove(txid)?;
        self.release(&pending);
        Some(pending)
    }

    pub(crate) fn get(&self, txid: &TransactionId) -> Option<&Pending> {
        self.by_txid.get(txid)
    }

    /// Drops every txid first heard over `OVERHEARD_TTL` before `now`
    pub(crate) fn expire(&mut self, now: Instant) {
        while let Some(&(first, txid)) = self.order.front() {
            if now.duration_since(first) < OVERHEARD_TTL {
                break;
            }
            self.order.pop_front();
            // taken earlier = gone already (or re-heard later under a newer order entry)
            if self.by_txid.get(&txid).is_some_and(|pending| pending.first == first) {
                if let Some(pending) = self.by_txid.remove(&txid) {
                    self.release(&pending);
                }
            }
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.by_txid.len()
    }

    fn release(&mut self, pending: &Pending) {
        for peer in &pending.peers {
            match self.per_peer.get(peer).copied() {
                Some(1) | None => drop(self.per_peer.remove(peer)),
                Some(held) => drop(self.per_peer.insert(*peer, held - 1)),
            }
        }
    }
}

/// Feeds the view from the peers' announcements until cancelled
pub struct PeerWatch {
    core: Arc<ChainViewCore>,
    peers: Arc<dyn ValidatorP2pSource>,
}

impl PeerWatch {
    pub(crate) fn new(core: Arc<ChainViewCore>, peers: Arc<dyn ValidatorP2pSource>) -> Self {
        Self { core, peers }
    }

    pub async fn run(self, cancel: CancellationToken) {
        let mut heard = self.peers.heard();
        let mut fold = tokio::time::interval(PEER_FOLD);
        fold.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // stamped on arrival, not at the fold (first_seen to the `inv`, not to the batch)
        let mut batch: Vec<(Instant, Heard)> = Vec::new();
        let mut ended = false;
        loop {
            tokio::select! {
                () = cancel.cancelled() => return,
                next = heard.next(), if !ended => match next {
                    Some(announced) => batch.push((Instant::now(), announced)),
                    None => {
                        warn!("Peer announcements ended, peer sightings frozen");
                        ended = true;
                    }
                },
                _ = fold.tick() => {
                    let live = self.peers.live();
                    self.core.apply_heard(std::mem::take(&mut batch), live, Instant::now());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use proptest::prelude::*;

    use super::*;

    #[derive(Debug, Clone)]
    enum Op {
        Hear { txid: u8, peer: u8 },
        Take { txid: u8 },
        Wait { secs: u8 },
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            8 => (0u8..32, 0u8..4).prop_map(|(txid, peer)| Op::Hear { txid, peer }),
            1 => (0u8..32).prop_map(|txid| Op::Take { txid }),
            1 => (0u8..90).prop_map(|secs| Op::Wait { secs }),
        ]
    }

    proptest! {
        /// Naive model (txid → first + announcers, per-peer cap by live count, expiry by age):
        /// same txids held, same announcers, same takes, counters never drift
        #[test]
        fn overheard_answers_like_a_naive_map(ops in proptest::collection::vec(op(), 1..400)) {
            const CAP: usize = OVERHEARD_PER_PEER;
            let txid = |n: u8| TransactionId::from([n; 32]);
            let peer = |n: u8| SocketAddr::from(([10, 0, 0, n], 8233));
            let mut now = Instant::now();
            let mut real = Overheard::default();
            let mut model: BTreeMap<u8, (Instant, BTreeSet<u8>)> = BTreeMap::new();
            for op in ops {
                match op {
                    Op::Hear { txid: t, peer: p } => {
                        let held = model.values().filter(|(_, peers)| peers.contains(&p)).count();
                        let known = model.get(&t).is_some_and(|(_, peers)| peers.contains(&p));
                        let kept = known || held < CAP;
                        if !known && kept {
                            model.entry(t).or_insert((now, BTreeSet::new())).1.insert(p);
                        }
                        prop_assert_eq!(real.hear(txid(t), peer(p), now), kept);
                    }
                    Op::Take { txid: t } => {
                        let want = model.remove(&t).map(|(first, peers)| {
                            (first, peers.into_iter().map(peer).collect::<BTreeSet<_>>())
                        });
                        let got = real
                            .take(&txid(t))
                            .map(|pending| (pending.first, pending.peers.into_iter().collect()));
                        prop_assert_eq!(got, want);
                    }
                    Op::Wait { secs } => {
                        now += Duration::from_secs(u64::from(secs));
                        model.retain(|_, (first, _)| now.duration_since(*first) < OVERHEARD_TTL);
                        real.expire(now);
                    }
                }
                prop_assert_eq!(real.len(), model.len());
                for (t, (first, peers)) in &model {
                    let held = real.get(&txid(*t)).expect("model txid held");
                    let want: BTreeSet<SocketAddr> = peers.iter().copied().map(peer).collect();
                    prop_assert_eq!(held.first, *first);
                    prop_assert_eq!(held.peers.iter().copied().collect::<BTreeSet<_>>(), want);
                }
                for p in 0u8..4 {
                    let held = model.values().filter(|(_, peers)| peers.contains(&p)).count();
                    prop_assert_eq!(real.per_peer.get(&peer(p)).copied().unwrap_or(0), held);
                }
            }
        }
    }

    /// Flooding peer fills its own budget, no more (another peer's txids still land)
    #[test]
    fn a_flooding_peer_cannot_crowd_out_another() {
        let now = Instant::now();
        let flood = SocketAddr::from(([10, 0, 0, 1], 8233));
        let honest = SocketAddr::from(([10, 0, 0, 2], 8233));
        let mut overheard = Overheard::default();
        let txid = |n: u32| {
            let mut bytes = [0u8; 32];
            bytes[..4].copy_from_slice(&n.to_le_bytes());
            TransactionId::from(bytes)
        };
        let kept =
            (0..OVERHEARD_PER_PEER as u32 * 2).filter(|&n| overheard.hear(txid(n), flood, now));
        assert_eq!(kept.count(), OVERHEARD_PER_PEER, "flooder capped at its budget");
        assert!(overheard.hear(txid(u32::MAX), honest, now), "another peer still heard");
        assert!(
            overheard.hear(txid(0), honest, now),
            "a flooded txid still takes a second announcer"
        );
        assert_eq!(overheard.get(&txid(0)).map(|p| p.peers.len()), Some(2));
        overheard.expire(now + OVERHEARD_TTL);
        assert_eq!(
            (overheard.len(), overheard.per_peer.len()),
            (0, 0),
            "all aged out, budgets freed"
        );
    }
}
