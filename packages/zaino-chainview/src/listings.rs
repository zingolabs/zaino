//! Peers' mempool listings → which transactions a peer quorum holds (`docs/architecture.md` §5)
//!
//! - admit: > 2/3 of the fresh listings hold the id, its listers in > [`ECLIPSE_NETGROUPS`]
//!   netgroups (each a full node that validated it: policy + consensus, ours never re-run)
//! - retain: > 1/3 hold its txid (an `inv` taken as a `mempool` reply = one short listing, never
//!   a flap)
//! - fresh = listed within [`LISTING_FRESH`] of the last round (a busy peer keeps its last)
//! - conflicting transactions never both held while every peer is honest (> 2/3 and > 1/3 overlap,
//!   one mempool holds one)
//! - peer listing both → both may be held (`mempool_model_tests.rs`)

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::time::Duration;

use imbl::{OrdMap, OrdSet};
use tokio::time::Instant;
use zaino_primitives::types::{Netgroup, PeerTxId, ECLIPSE_NETGROUPS};
use zcash_protocol::TxId;

/// Listing counted this long after it was read (three `PEER_MEMPOOL_POLL` rounds)
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
        Netgroup::count(self.fresh().map(|(peer, _)| *peer)) > ECLIPSE_NETGROUPS
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
    listers.len() * 3 > polled * 2 && Netgroup::count(listers.iter().copied()) > ECLIPSE_NETGROUPS
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

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

    /// Ids the model draws from: `id(0..IDS)`
    const IDS: u8 = 8;

    /// One round: per peer, `None` = failed, else the ids (`id(n)`) it lists; then `wait`
    #[derive(Debug, Clone)]
    struct Round {
        answers: Vec<Option<BTreeSet<u8>>>,
        wait: Duration,
    }

    fn rounds(peers: usize) -> impl Strategy<Value = Vec<Round>> {
        let listed = prop::collection::btree_set(0..IDS, 0..=usize::from(IDS));
        let answer = proptest::option::weighted(0.9, listed);
        let wait = (0u64..12).prop_map(Duration::from_secs);
        let round = (prop::collection::vec(answer, peers), wait)
            .prop_map(|(answers, wait)| Round { answers, wait });
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
            let ids: Vec<PeerTxId> = (0..IDS).map(|n| id(n, witnessed.then_some(n))).collect();
            let start = Instant::now();
            let mut now = start;
            let mut real = Listings::default();
            let mut model: BTreeMap<SocketAddr, (Instant, BTreeSet<PeerTxId>)> = BTreeMap::new();
            for round in rounds {
                now += round.wait;
                let at = now - start;
                let answers: Vec<(SocketAddr, Option<Vec<PeerTxId>>)> = addrs
                    .iter()
                    .zip(&round.answers)
                    .map(|(peer, listed)| {
                        let listed = listed.as_ref().map(|listed| {
                            listed.iter().map(|n| ids[usize::from(*n)]).collect()
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
                prop_assert_eq!(&real_polled, &polled, "at {:?}: fresh peers, real vs model", at);
                let groups = |peers: &[SocketAddr]| {
                    peers.iter().map(|p| Netgroup::of(*p)).collect::<BTreeSet<_>>().len()
                };
                let all: Vec<SocketAddr> = polled.iter().copied().collect();
                let quorate = groups(&all) > ECLIPSE_NETGROUPS;
                prop_assert_eq!(
                    real.quorate(), quorate,
                    "at {:?}: quorate, real vs model ({} fresh netgroups)", at, groups(&all)
                );
                let mut admitted = Vec::new();
                for (n, id) in ids.iter().enumerate() {
                    let listers: Vec<SocketAddr> =
                        fresh.iter().filter(|(_, l)| l.contains(id)).map(|(p, _)| **p).collect();
                    let admits = listers.len() * 3 > fresh.len() * 2
                        && groups(&listers) > ECLIPSE_NETGROUPS;
                    prop_assert_eq!(
                        real.admits(id), admits,
                        "at {:?}: id({}) admitted, real vs model: {} of {} fresh list it, {} netgroups",
                        at, n, listers.len(), fresh.len(), groups(&listers)
                    );
                    if admits {
                        admitted.push(*id);
                    }
                    let by_txid: BTreeSet<SocketAddr> = fresh.iter()
                        .filter(|(_, l)| l.iter().any(|listed| listed.txid == id.txid))
                        .map(|(p, _)| **p).collect();
                    let real_listers: BTreeSet<SocketAddr> =
                        real.listers(&id.txid).into_iter().collect();
                    prop_assert_eq!(
                        &real_listers, &by_txid,
                        "at {:?}: id({})'s txid listers (any witness), real vs model", at, n
                    );
                    prop_assert_eq!(
                        real.retains(&id.txid), by_txid.len() * 3 > fresh.len(),
                        "at {:?}: id({})'s txid retained, real vs model: {} of {} fresh list it",
                        at, n, by_txid.len(), fresh.len()
                    );
                }
                let real_admitted = real.admitted();
                prop_assert_eq!(&real_admitted, &admitted, "at {:?}: every admitted id, real vs model", at);
            }
        }
    }

    /// 9 peers in 9 netgroups, the first `listing` of them listing `a`
    #[test]
    fn an_id_is_admitted_above_two_thirds_of_fresh_listings_and_retained_above_one_third() {
        let a = id(1, Some(1));
        let spread: Vec<SocketAddr> = (1..=9).map(|n| peer(n, true)).collect();
        #[rustfmt::skip]
        let cases = [
            // case,                   listing, admitted, retained
            ("7 of 9: > 2/3",          7,       true,     true),
            ("6 of 9: = 2/3",          6,       false,    true),
            ("4 of 9: > 1/3",          4,       false,    true),
            ("3 of 9: = 1/3",          3,       false,    false),
        ];
        for (case, listing, admitted, retained) in cases {
            let answers = spread.iter().enumerate().map(|(at, peer)| {
                let listed = if at < listing { vec![a] } else { Vec::new() };
                (*peer, Some(listed))
            });
            let mut listings = Listings::default();
            listings.round(answers.collect(), Instant::now());

            assert_eq!(listings.admits(&a), admitted, "{case}: admitted");
            assert_eq!(listings.retains(&a.txid), retained, "{case}: retained");
        }
    }

    /// Every peer listing `a`: 9/9, so netgroups alone decide (10.1.0.1… own, 10.0.0.x shared)
    #[test]
    fn admission_needs_fresh_listers_in_more_than_five_netgroups() {
        let a = id(1, Some(1));
        for (case, own_netgroups, quorate) in [("6 netgroups", 5, true), ("5 netgroups", 4, false)]
        {
            let peers = (1..=9).map(|n| peer(n, n <= own_netgroups));
            let mut listings = Listings::default();
            listings.round(peers.map(|peer| (peer, Some(vec![a]))).collect(), Instant::now());

            assert_eq!(listings.quorate(), quorate, "{case}: quorate");
            let expected = if quorate { vec![a] } else { vec![] };
            assert_eq!(listings.admitted(), expected, "{case}: admitted");
        }
    }

    #[test]
    fn another_witness_of_a_txid_lists_the_txid_but_is_not_the_admitted_id() {
        let (a, a_other_witness) = (id(1, Some(1)), id(1, Some(2)));
        let mut listings = Listings::default();
        let spread = (1..=9).map(|n| peer(n, true));
        listings.round(
            spread.map(|peer| (peer, Some(vec![a_other_witness]))).collect(),
            Instant::now(),
        );

        assert!(!listings.admits(&a), "{a:?} never listed: not admitted");
        assert_eq!(listings.listers(&a.txid).len(), 9, "any witness lists the txid");
        assert!(listings.retains(&a.txid), "retained by txid, whatever the witness");
    }

    /// Every peer lists `a` at `start`, then fails every round
    #[test]
    fn a_failed_peer_keeps_its_last_listing_for_15_s() {
        let a = id(1, Some(1));
        let spread: Vec<SocketAddr> = (1..=9).map(|n| peer(n, true)).collect();
        let start = Instant::now();
        let mut listings = Listings::default();
        listings.round(spread.iter().map(|peer| (*peer, Some(vec![a]))).collect(), start);
        let failed: Vec<_> = spread.iter().map(|peer| (*peer, None)).collect();

        listings.round(failed.clone(), start + LISTING_FRESH - Duration::from_millis(1));
        let (kept, kept_admitted) = (listings.fresh().count(), listings.admitted());
        listings.round(failed, start + LISTING_FRESH);
        let (stale, stale_admitted) = (listings.fresh().count(), listings.admitted());

        assert_eq!(kept, 9, "fresh listings 1 ms inside {LISTING_FRESH:?}");
        assert_eq!(kept_admitted, [a], "admitted 1 ms inside {LISTING_FRESH:?}");
        assert_eq!(stale, 0, "fresh listings at {LISTING_FRESH:?}");
        assert_eq!(stale_admitted, [], "admitted at {LISTING_FRESH:?}");
    }
}
