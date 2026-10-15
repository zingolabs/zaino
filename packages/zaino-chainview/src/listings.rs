//! Peers' mempool listings → which transactions a peer quorum holds (`docs/architecture.md` §5)
//!
//! - admit: > 2/3 of the fresh listings hold the id, its listers in > [`ECLIPSE_NETGROUPS`]
//!   netgroups (each a full node that validated it: policy + consensus, ours never re-run)
//! - retain: > 1/3 hold its txid (an `inv` taken as a `mempool` reply = one short listing, never
//!   a flap)
//! - fresh = listed within [`LISTING_FRESH`] of the last round (a busy peer keeps its last)
//! - conflicting transactions never both held while every peer is honest (> 2/3 and > 1/3 overlap,
//!   one mempool holds one)
//! - peer listing both → both may be held (`mempool_model.rs`)

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::time::Duration;

use imbl::{OrdMap, OrdSet};
use tokio::time::Instant;
use zaino_primitives::types::PeerTxId;
use zcash_protocol::TxId;

use crate::netgroup::{self, ECLIPSE_NETGROUPS};

/// Every live outbound peer's mempool asked this often
pub(crate) const PEER_MEMPOOL_POLL: Duration = Duration::from_secs(5);

/// Listing counted this long after it was read (three rounds)
pub(crate) const LISTING_FRESH: Duration = Duration::from_secs(15);

/// Each live peer's latest listing, `imbl` (published with the `ChainViewSnapshot`)
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Listings {
    by_peer: OrdMap<SocketAddr, Listed>,
    round: Option<Instant>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Listed {
    at: Instant,
    ids: OrdSet<PeerTxId>,
}

impl Listings {
    /// One round at `now`: each live peer's answer (`None` = failed: its last kept while fresh)
    ///
    /// - peer no longer live → dropped
    pub(crate) fn round(
        &mut self,
        answers: Vec<(SocketAddr, Option<Vec<PeerTxId>>)>,
        now: Instant,
    ) {
        let live: BTreeSet<SocketAddr> = answers.iter().map(|(peer, _)| *peer).collect();
        let by_peer = std::mem::take(&mut self.by_peer).into_iter();
        self.by_peer = by_peer.filter(|(peer, _)| live.contains(peer)).collect();
        for (peer, answer) in answers {
            if let Some(ids) = answer {
                self.by_peer.insert(peer, Listed { at: now, ids: ids.into_iter().collect() });
            }
        }
        self.round = Some(now);
    }

    fn fresh(&self) -> impl Iterator<Item = (&SocketAddr, &OrdSet<PeerTxId>)> + '_ {
        let fresh = move |at: Instant| self.round.is_some_and(|round| round - at < LISTING_FRESH);
        self.by_peer.iter().filter(move |(_, listed)| fresh(listed.at)).map(|(p, l)| (p, &l.ids))
    }

    /// Fresh peers listing `txid`, whatever its authorizing data
    pub(crate) fn listers(&self, txid: &TxId) -> OrdSet<SocketAddr> {
        let first = PeerTxId { txid: *txid, auth_digest: None };
        let last = PeerTxId { txid: *txid, auth_digest: Some([u8::MAX; 32]) };
        let lists = |ids: &OrdSet<PeerTxId>| ids.range(first..=last).next().is_some();
        self.fresh().filter(|(_, ids)| lists(ids)).map(|(peer, _)| *peer).collect()
    }

    /// Admission possible at all: the fresh listings span > [`ECLIPSE_NETGROUPS`] netgroups
    pub(crate) fn quorate(&self) -> bool {
        netgroup::count(self.fresh().map(|(peer, _)| *peer)) > ECLIPSE_NETGROUPS
    }

    /// Every id the quorum admits, in id order
    pub(crate) fn admitted(&self) -> Vec<PeerTxId> {
        let mut listers: BTreeMap<PeerTxId, Vec<SocketAddr>> = BTreeMap::new();
        let mut polled = 0;
        for (peer, ids) in self.fresh() {
            polled += 1;
            ids.iter().for_each(|id| listers.entry(*id).or_default().push(*peer));
        }
        let admitted = listers.into_iter().filter(|(_, peers)| quorum(peers, polled));
        admitted.map(|(id, _)| id).collect()
    }

    /// One id's admission (the merge re-asks under its lock: the round may have moved on)
    pub(crate) fn admits(&self, id: &PeerTxId) -> bool {
        let peers: Vec<SocketAddr> =
            self.fresh().filter(|(_, ids)| ids.contains(id)).map(|(peer, _)| *peer).collect();
        quorum(&peers, self.fresh().count())
    }

    /// > 1/3 of the fresh listings hold `txid`
    pub(crate) fn retains(&self, txid: &TxId) -> bool {
        self.listers(txid).len() * 3 > self.fresh().count()
    }
}

/// `listers` > 2/3 of `polled`, in > [`ECLIPSE_NETGROUPS`] netgroups
fn quorum(listers: &[SocketAddr], polled: usize) -> bool {
    listers.len() * 3 > polled * 2 && netgroup::count(listers.iter().copied()) > ECLIPSE_NETGROUPS
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use crate::netgroup::Netgroup;

    use super::*;

    /// Peer `n` at 10.n.0.1 (its own netgroup) or 10.0.0.n (all in one)
    fn peer(n: u8, own_netgroup: bool) -> SocketAddr {
        match own_netgroup {
            true => SocketAddr::from(([10, n, 0, 1], 8233)),
            false => SocketAddr::from(([10, 0, 0, n], 8233)),
        }
    }

    fn id(n: u8, witness: Option<u8>) -> PeerTxId {
        PeerTxId { txid: TxId::from_bytes([n; 32]), auth_digest: witness.map(|w| [w; 32]) }
    }

    /// One round: per peer, `None` = failed, else the ids it lists (bit `i` = id `i`)
    #[derive(Debug, Clone)]
    struct Round {
        answers: Vec<Option<u8>>,
        wait_secs: u8,
    }

    fn rounds(peers: usize) -> impl Strategy<Value = Vec<Round>> {
        let answer = proptest::option::weighted(0.9, any::<u8>());
        let round = (prop::collection::vec(answer, peers), 0u8..12)
            .prop_map(|(answers, wait_secs)| Round { answers, wait_secs });
        prop::collection::vec(round, 1..24)
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

        /// Naive model (each peer's last answer + when, fresh = < 15 s old): same polled set,
        /// same listers, same admitted ids, same retention, quorate iff > 5 netgroups fresh
        #[test]
        fn listings_answer_like_a_naive_map_of_each_peers_last_answer(
            peers in 1usize..10,
            own_netgroups in any::<bool>(),
            witnessed in any::<bool>(),
            rounds in rounds(9),
        ) {
            let addrs: Vec<SocketAddr> =
                (0..peers).map(|n| peer(n as u8 + 1, own_netgroups || n % 2 == 0)).collect();
            let ids: Vec<PeerTxId> =
                (0..8u8).map(|n| id(n, witnessed.then_some(n))).collect();
            let start = Instant::now();
            let mut now = start;
            let mut real = Listings::default();
            let mut model: BTreeMap<SocketAddr, (Instant, BTreeSet<PeerTxId>)> = BTreeMap::new();
            for round in rounds {
                now += Duration::from_secs(u64::from(round.wait_secs));
                let answers: Vec<(SocketAddr, Option<Vec<PeerTxId>>)> = addrs
                    .iter()
                    .zip(&round.answers)
                    .map(|(peer, bits)| {
                        let listed = bits.map(|bits| {
                            ids.iter().enumerate().filter(|(i, _)| bits & (1 << i) != 0)
                                .map(|(_, id)| *id).collect()
                        });
                        (*peer, listed)
                    })
                    .collect();
                for (peer, listed) in &answers {
                    if let Some(listed) = listed {
                        model.insert(*peer, (now, listed.iter().copied().collect()));
                    }
                }
                real.round(answers, now);

                let fresh: Vec<(&SocketAddr, &BTreeSet<PeerTxId>)> = model
                    .iter()
                    .filter(|(_, (at, _))| now - *at < LISTING_FRESH)
                    .map(|(peer, (_, listed))| (peer, listed))
                    .collect();
                let polled: BTreeSet<SocketAddr> = fresh.iter().map(|(p, _)| **p).collect();
                let real_polled: BTreeSet<SocketAddr> = real.fresh().map(|(p, _)| *p).collect();
                prop_assert_eq!(real_polled, polled.clone());
                let groups = |peers: &[SocketAddr]| {
                    peers.iter().map(|p| Netgroup::of(*p)).collect::<BTreeSet<_>>().len()
                };
                let all: Vec<SocketAddr> = polled.iter().copied().collect();
                prop_assert_eq!(real.quorate(), groups(&all) > ECLIPSE_NETGROUPS);
                let mut admitted = Vec::new();
                for id in &ids {
                    let listers: Vec<SocketAddr> =
                        fresh.iter().filter(|(_, l)| l.contains(id)).map(|(p, _)| **p).collect();
                    let admits = listers.len() * 3 > fresh.len() * 2
                        && groups(&listers) > ECLIPSE_NETGROUPS;
                    prop_assert_eq!(real.admits(id), admits, "{:?}", id);
                    if admits {
                        admitted.push(*id);
                    }
                    let by_txid: BTreeSet<SocketAddr> = fresh.iter()
                        .filter(|(_, l)| l.iter().any(|listed| listed.txid == id.txid))
                        .map(|(p, _)| **p).collect();
                    let real_listers: BTreeSet<SocketAddr> =
                        real.listers(&id.txid).into_iter().collect();
                    prop_assert_eq!(&real_listers, &by_txid);
                    prop_assert_eq!(real.retains(&id.txid), by_txid.len() * 3 > fresh.len());
                }
                prop_assert_eq!(real.admitted(), admitted);
            }
        }
    }

    /// - 9 peers, 9 netgroups: 7 listing = > 2/3 → admitted, retained down to 4 (> 1/3), gone at 3
    /// - same 7 inside 5 netgroups = never admitted
    /// - another witness of the txid = not this id, still a lister of the txid
    #[test]
    fn admission_needs_two_thirds_in_six_netgroups_and_retention_one_third() {
        let start = Instant::now();
        let a = id(1, Some(1));
        let a_other_witness = id(1, Some(2));
        let answers = |peers: &[SocketAddr], listing: usize, with: PeerTxId| {
            let listed = |at: usize| if at < listing { vec![with] } else { Vec::new() };
            peers.iter().enumerate().map(|(at, p)| (*p, Some(listed(at)))).collect::<Vec<_>>()
        };
        let spread: Vec<SocketAddr> = (1..=9).map(|n| peer(n, true)).collect();
        let mut listings = Listings::default();
        listings.round(answers(&spread, 7, a), start);
        assert_eq!((listings.quorate(), listings.admitted()), (true, vec![a]));
        assert!(!listings.admits(&a_other_witness), "an id the quorum never listed");
        for (listing, retained) in [(6, true), (4, true), (3, false)] {
            listings.round(answers(&spread, listing, a), start);
            assert_eq!(listings.retains(&a.txid), retained, "{listing} of 9 list it");
        }
        listings.round(answers(&spread, 4, a_other_witness), start);
        assert_eq!(listings.listers(&a.txid).len(), 4, "any witness lists the txid");

        // 9 peers over 5 netgroups (10.1–10.4 + 10.0.0.x): 7 listing never admits
        let clustered: Vec<SocketAddr> =
            (1..=9).map(|n| if n <= 4 { peer(n, true) } else { peer(n, false) }).collect();
        let mut listings = Listings::default();
        listings.round(answers(&clustered, 9, a), start);
        assert_eq!((listings.quorate(), listings.admitted()), (false, vec![]), "5 netgroups");

        // a failed peer keeps its last listing for 15 s, then stops counting
        let mut listings = Listings::default();
        listings.round(answers(&spread, 9, a), start);
        let failed: Vec<_> = spread.iter().map(|p| (*p, None)).collect();
        listings.round(failed.clone(), start + LISTING_FRESH - Duration::from_secs(1));
        assert_eq!(listings.fresh().count(), 9, "kept while fresh");
        listings.round(failed, start + LISTING_FRESH);
        assert_eq!((listings.fresh().count(), listings.admitted()), (0, vec![]), "stale");
    }
}
