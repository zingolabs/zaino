//! Mempool model: trusted validators polled / failing / killed, peers' listings churning (some
//! peers byzantine), blocks, our sends → real merge (`ChainViewCore`) + real checks
//! (`peer_mempool::judge`) vs a naive oracle after every move (`docs/architecture.md` §5)
//!
//! - held = exactly: listed by a trusted validator not `Down` | kept by the peer quorum | ours
//!   (an unlisted `ours` gone at the next block), each at the first fee known
//! - quorum admit iff > 2/3 of the fresh listings hold it, from > 5 netgroups, + passes the checks
//!   (a child once its parent is held)
//! - quorum retain while > 1/3 list it
//! - gate: open iff a `Live` validator polled since the last block, or the peers quorate
//! - negative fee / unknown prevout → never held
//! - two conflicting spends: never both quorum-held among honest peers (only while a fresh peer
//!   lists both)

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use imbl::Vector;
use proptest::prelude::*;
use tokio::time::Instant;
use zaino_header_chain::testing::HeaderViews;
use zaino_primitives::types::{BlockRef, FinalityDepth, PeerTxId};
use zaino_source::testing::{p2pkh, raw_transparent, Ledger, MockChain, ValidatorViews};
use zaino_traffic::TrustedValidatorId;
use zcash_protocol::value::Zatoshis;
use zcash_protocol::TxId;
use zcash_transparent::bundle::OutPoint;

use crate::admission::NextBlock;
use crate::listings::LISTING_FRESH;
use crate::merge::{ChainViewCore, Listing, Reading, Sighted, TrustedValidatorReport};
use crate::netgroup::Netgroup;
use crate::netgroup::ECLIPSE_NETGROUPS;
use crate::peer_mempool::judge;
use crate::trusted_validators::TrustedValidatorMeta;

/// `a`, `b` and `rival` (spends `b`'s prevout) fund from coinbases at 1..=3, `child` spends `a`'s
/// output, `bad` pays out more than it spends, `orphan` spends an outpoint nobody holds
const A: usize = 0;
const B: usize = 1;
const RIVAL: usize = 2;
const CHILD: usize = 3;
const BAD: usize = 4;
const ORPHAN: usize = 5;
const TXS: usize = 6;
/// Fee a valid one leaves (a trusted listing names the same)
const FEES: [u64; 4] = [1_000, 500, 1_000, 1_000];

#[derive(Debug, Clone)]
enum Move {
    /// Validator answers, listing these (valid ones only: it validated them)
    Poll {
        validator: u8,
        listed: u8,
    },
    Fail {
        validator: u8,
    },
    Kill {
        validator: u8,
    },
    /// Per peer: `None` = unanswered, else the txs its mempool holds (then `wait` seconds)
    Round {
        answers: Vec<Option<u8>>,
        wait: u8,
    },
    Block,
    Send {
        tx: u8,
    },
}

fn moves(peers: usize) -> impl Strategy<Value = Vec<Move>> {
    let answer = proptest::option::weighted(0.85, any::<u8>());
    let step = prop_oneof![
        3 => (any::<u8>(), any::<u8>()).prop_map(|(validator, listed)| Move::Poll { validator, listed }),
        1 => any::<u8>().prop_map(|validator| Move::Fail { validator }),
        1 => any::<u8>().prop_map(|validator| Move::Kill { validator }),
        5 => (prop::collection::vec(answer, peers), 0u8..=20)
            .prop_map(|(answers, wait)| Move::Round { answers, wait }),
        1 => Just(Move::Block),
        1 => (0u8..4).prop_map(|tx| Move::Send { tx }),
    ];
    prop::collection::vec(step, 1..48)
}

/// What an honest node's mempool can hold: valid only, one of two rivals, a child with its parent
fn honest(mask: u8) -> BTreeSet<usize> {
    let mut held: BTreeSet<usize> = (A..=CHILD).filter(|tx| mask & (1 << tx) != 0).collect();
    if held.contains(&B) {
        held.remove(&RIVAL);
    }
    if !held.contains(&A) {
        held.remove(&CHILD);
    }
    held
}

/// Anything at all (a byzantine peer's listing)
fn any_of(mask: u8) -> BTreeSet<usize> {
    (0..TXS).filter(|tx| mask & (1 << tx) != 0).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Health {
    Pending,
    Live,
    Degraded,
    Down,
}

/// Oracle: one held transaction
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Held {
    fee: Option<u64>,
    quorum: bool,
    ours: bool,
}

struct Oracle {
    health: Vec<Health>,
    listed: Vec<BTreeSet<usize>>,
    polled_since_block: Vec<bool>,
    /// Peer → its last answer and when
    listings: BTreeMap<usize, (Instant, BTreeSet<usize>)>,
    round: Option<Instant>,
    held: BTreeMap<usize, Held>,
}

impl Oracle {
    fn fresh(&self) -> Vec<(usize, &BTreeSet<usize>)> {
        let Some(round) = self.round else { return Vec::new() };
        let fresh = self.listings.iter().filter(|(_, (at, _))| round - *at < LISTING_FRESH);
        fresh.map(|(peer, (_, txs))| (*peer, txs)).collect()
    }

    fn listers(&self, tx: usize) -> Vec<usize> {
        self.fresh().into_iter().filter(|(_, txs)| txs.contains(&tx)).map(|(p, _)| p).collect()
    }

    fn trusted(&self, tx: usize) -> bool {
        (0..self.health.len())
            .any(|v| self.health[v] != Health::Down && self.listed[v].contains(&tx))
    }

    fn listed(&self, tx: usize) -> bool {
        self.trusted(tx) || self.held.get(&tx).is_some_and(|held| held.quorum)
    }

    /// The merge's drop rule after any change
    fn settle(&mut self, moved: bool) {
        let gone: Vec<usize> = self.held.keys().copied().collect();
        for tx in gone {
            let ours = self.held[&tx].ours;
            if !self.listed(tx) && (moved || !ours) {
                self.held.remove(&tx);
            }
        }
    }

    fn sight(&mut self, tx: usize) {
        let held = self.held.entry(tx).or_insert(Held { fee: None, quorum: false, ours: false });
        held.fee = held.fee.or(Some(FEES[tx]));
    }
}

/// One case: the system + its oracle, moved in lockstep
struct Run {
    core: ChainViewCore,
    chain: MockChain,
    tip: BlockRef,
    peers: Vec<SocketAddr>,
    byzantine: Vec<bool>,
    txs: Vec<(TxId, Bytes)>,
    utxos: Ledger,
    start: Instant,
    clock: Duration,
    oracle: Oracle,
}

impl Run {
    fn reading(&self) -> Reading {
        let info = self.chain.blockchain_info(self.tip);
        Reading {
            block_at_best: Some(self.tip),
            info,
            peers: None,
            release: None,
            streaming: false,
        }
    }

    fn id(&self, tx: usize) -> PeerTxId {
        PeerTxId { txid: self.txs[tx].0, auth_digest: None }
    }

    fn apply(&mut self, step: &Move) {
        let validators = self.oracle.health.len();
        match *step {
            Move::Poll { validator, listed } => {
                let v = usize::from(validator) % validators;
                let listed = honest(listed);
                let id = TrustedValidatorId::new(v).expect("small");
                let report = TrustedValidatorReport::Read {
                    reading: self.reading(),
                    health: zaino_traffic::Health::Live,
                    listed: true,
                };
                self.core.apply(id, report);
                let before = &self.oracle.listed[v];
                let sighted = |tx: &usize| Sighted {
                    txid: self.txs[*tx].0,
                    raw: Some(self.txs[*tx].1.clone()),
                    fee: Zatoshis::from_u64(FEES[*tx]).expect("in supply"),
                };
                let added = listed.difference(before).map(sighted).collect();
                let removed = before.difference(&listed).map(|tx| self.txs[*tx].0).collect();
                self.core.apply(id, TrustedValidatorReport::Listed(Listing { added, removed }));
                self.oracle.health[v] = Health::Live;
                self.oracle.polled_since_block[v] = true;
                self.oracle.listed[v] = listed.clone();
                listed.iter().for_each(|tx| self.oracle.sight(*tx));
                self.oracle.settle(false);
            }
            Move::Fail { validator } => {
                let v = usize::from(validator) % validators;
                self.core.apply(
                    TrustedValidatorId::new(v).expect("small"),
                    TrustedValidatorReport::Failed(zaino_traffic::Health::Degraded),
                );
                self.oracle.health[v] = Health::Degraded;
                self.oracle.settle(false);
            }
            Move::Kill { validator } => {
                let v = usize::from(validator) % validators;
                self.core.apply(
                    TrustedValidatorId::new(v).expect("small"),
                    TrustedValidatorReport::Failed(zaino_traffic::Health::Down),
                );
                self.oracle.health[v] = Health::Down;
                self.oracle.listed[v].clear();
                self.oracle.settle(false);
            }
            Move::Round { ref answers, wait } => self.round(answers, wait),
            Move::Block => {
                self.tip = self.chain.mine_empty(1);
                self.core.apply_headers(Some(Arc::new(self.chain.verified(self.tip))));
                self.oracle.polled_since_block.iter_mut().for_each(|polled| *polled = false);
                self.oracle.settle(true);
            }
            Move::Send { tx } => {
                let tx = usize::from(tx);
                let (txid, raw) = self.txs[tx].clone();
                self.core.mark_ours(txid, raw);
                let held = self.oracle.held.entry(tx);
                held.or_insert(Held { fee: None, quorum: false, ours: false }).ours = true;
            }
        }
    }

    fn round(&mut self, answers: &[Option<u8>], wait: u8) {
        let now = self.start + self.clock;
        let listing = |peer: usize, mask: u8| match self.byzantine[peer] {
            true => any_of(mask),
            false => honest(mask),
        };
        let answered: Vec<(usize, Option<BTreeSet<usize>>)> = answers
            .iter()
            .enumerate()
            .map(|(peer, mask)| (peer, mask.map(|m| listing(peer, m))))
            .collect();
        let ids = |txs: &BTreeSet<usize>| txs.iter().map(|tx| self.id(*tx)).collect();
        let real = answered.iter().map(|(peer, txs)| (self.peers[*peer], txs.as_ref().map(ids)));
        let candidates = self.core.apply_listings(real.collect(), now);
        let view = self.core.current();
        let next = NextBlock::above(self.tip.height, &self.chain.schedule().upgrades);
        let fetched = candidates.into_iter().map(|(id, raw)| {
            let tx = self.txs.iter().position(|(txid, _)| *txid == id.txid).expect("ours");
            (id, raw.unwrap_or_else(|| self.txs[tx].1.clone()))
        });
        let judged = judge(&view, &self.utxos, next, fetched.collect());
        let admitted = judged.into_iter().filter_map(|(id, raw, fee)| Some((id, raw, fee.ok()?)));
        self.core.admit_by_quorum(admitted.collect());

        // oracle: listings, retention + drop, then admission against the held set before it
        for (peer, txs) in answered {
            if let Some(txs) = txs {
                self.oracle.listings.insert(peer, (now, txs));
            }
        }
        self.oracle.round = Some(now);
        let polled = self.oracle.fresh().len();
        let netgroups = |peers: &[usize]| {
            peers.iter().map(|p| Netgroup::of(self.peers[*p])).collect::<BTreeSet<_>>().len()
        };
        for tx in 0..TXS {
            let retained = self.oracle.listers(tx).len() * 3 > polled;
            if let Some(held) = self.oracle.held.get_mut(&tx) {
                held.quorum &= retained;
            }
        }
        self.oracle.settle(false);
        let before: BTreeSet<usize> = self.oracle.held.keys().copied().collect();
        for tx in 0..TXS {
            let listers = self.oracle.listers(tx);
            let admits = listers.len() * 3 > polled * 2 && netgroups(&listers) > ECLIPSE_NETGROUPS;
            let passes = match tx {
                CHILD => before.contains(&A),
                BAD | ORPHAN => false,
                _ => true,
            };
            let already = self.oracle.held.get(&tx).is_some_and(|held| held.quorum);
            if admits && passes && !already {
                self.oracle.sight(tx);
                self.oracle.held.get_mut(&tx).expect("just held").quorum = true;
            }
        }
        self.clock += Duration::from_secs(u64::from(wait));
    }

    /// `rivals_before`: B and RIVAL both quorum-held before `step`
    fn check(&self, step: &Move, rivals_before: bool) -> bool {
        let view = self.core.current();
        let real: BTreeMap<usize, Held> = view
            .mempool
            .iter()
            .map(|(txid, sighting)| {
                let tx = self.txs.iter().position(|(t, _)| t == txid).expect("one of ours");
                let fee = sighting.entry(*txid).fee.map(Zatoshis::into_u64);
                (tx, Held { fee, quorum: sighting.quorum(), ours: sighting.ours() })
            })
            .collect();
        assert_eq!(real, self.oracle.held, "{step:?}: the held mempool");
        assert!(!real.contains_key(&BAD) && !real.contains_key(&ORPHAN), "{step:?}: never held");

        let holder = (0..self.oracle.health.len())
            .any(|v| self.oracle.health[v] == Health::Live && self.oracle.polled_since_block[v]);
        let fresh = self.oracle.fresh();
        let netgroups: BTreeSet<Netgroup> =
            fresh.iter().map(|(p, _)| Netgroup::of(self.peers[*p])).collect();
        let quorate = netgroups.len() > ECLIPSE_NETGROUPS;
        assert_eq!(view.mempool().is_some(), holder || quorate, "{step:?}: the gate");

        // - admission > 2/3 + retention > 1/3 → overlap
        // - held on afterwards by an honest split
        let rivals = [B, RIVAL].iter().all(|tx| real.get(tx).is_some_and(|h| h.quorum));
        if rivals && !rivals_before {
            let lists_both = fresh.iter().any(|(_, txs)| txs.contains(&B) && txs.contains(&RIVAL));
            assert!(lists_both, "{step:?}: rivals both quorum-held with no peer listing both");
        }
        let all_honest = !self.byzantine.contains(&true);
        assert!(!(rivals && all_honest), "{step:?}: rivals both quorum-held, every peer honest");
        rivals
    }
}

fn run(validators: usize, groups: Vec<u8>, byzantine: Vec<bool>, steps: Vec<Move>) {
    let alice = p2pkh([0xaa; 20]);
    let mut chain = MockChain::regtest();
    let funded = [10_000, 10_000, 5_000].map(|zats| {
        let block = chain.mine(|b| b.coinbase(|c| c.pay(&alice, zats)));
        chain.output(block, 0, 0)
    });
    let funding = |n: usize| funded[n - 1].clone();
    let unknown = OutPoint::new([9; 32], 0);
    let (a, a_raw) = raw_transparent(&[funding(1)], &[(&alice, 9_000)]);
    let txs = [
        (a, a_raw),
        raw_transparent(&[funding(2)], &[(&alice, 9_500)]),
        raw_transparent(&[funding(2)], &[(&alice, 9_000)]),
        raw_transparent(&[OutPoint::new(a.into(), 0)], &[(&alice, 8_000)]),
        raw_transparent(&[funding(3)], &[(&alice, 6_000)]),
        raw_transparent(&[unknown], &[(&alice, 1)]),
    ];
    let peers: Vec<SocketAddr> = groups
        .iter()
        .enumerate()
        .map(|(n, group)| SocketAddr::from(([10, group % 8, 0, n as u8 + 1], 8233)))
        .collect();

    let tip = chain.tip();
    let utxos = chain.ledger(tip);
    let trusted_validators: Vector<TrustedValidatorMeta> =
        (0..validators).map(|v| TrustedValidatorMeta::new(format!("v{v}:8232"))).collect();
    let core = ChainViewCore::new(trusted_validators, FinalityDepth::CONSENSUS);
    core.apply_headers(Some(Arc::new(chain.verified(tip))));
    let oracle = Oracle {
        health: vec![Health::Pending; validators],
        listed: vec![BTreeSet::new(); validators],
        polled_since_block: vec![false; validators],
        listings: BTreeMap::new(),
        round: None,
        held: BTreeMap::new(),
    };
    let txs = txs.into_iter().map(|(txid, raw)| (txid, Bytes::from(raw))).collect();
    let start = Instant::now();
    let mut run = Run {
        core,
        chain,
        tip,
        peers,
        byzantine,
        txs,
        utxos,
        start,
        clock: Duration::ZERO,
        oracle,
    };
    let mut rivals = false;
    for step in &steps {
        run.apply(step);
        rivals = run.check(step, rivals);
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// 1–2 trusted validators, 6–10 peers over ≤ 8 netgroups (some byzantine): see the module doc
    #[test]
    fn the_mempool_holds_exactly_what_a_live_validator_a_peer_quorum_or_we_vouch_for(
        (validators, groups, byzantine, steps) in (1usize..=2, 6usize..=10).prop_flat_map(|(v, p)| (
            Just(v),
            prop::collection::vec(any::<u8>(), p),
            prop::collection::vec(prop::bool::weighted(0.2), p),
            moves(p),
        )),
    ) {
        run(validators, groups, byzantine, steps);
    }
}
