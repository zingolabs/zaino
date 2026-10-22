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
use proptest::test_runner::TestCaseError;
use tokio::time::Instant;
use zaino_header_chain::HeaderChainSnapshot;
use zaino_primitives::types::{BlockRef, FinalityDepth, Netgroup, PeerTxId, ECLIPSE_NETGROUPS};
use zaino_source::testing::{p2pkh, raw_transparent, Ledger, MockChain};
use zaino_traffic::TrustedValidatorId;
use zcash_protocol::value::Zatoshis;
use zcash_protocol::TxId;
use zcash_transparent::bundle::OutPoint;

use crate::admission::NextBlock;
use crate::chain_view_core::ChainViewCore;
use crate::listings::LISTING_FRESH;
use crate::observation_merge::{Reading, Sighted, TrustedValidatorReport};
use crate::peer_mempool::judge;
use crate::trusted_validators::TrustedValidatorMeta;

/// - `A`, `B`, `Rival` (spends `B`'s prevout) fund from coinbases at 1..=3
/// - `Child` spends `A`'s output, `Bad` pays out more than it spends, `Orphan` spends an outpoint
///   nobody holds
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Tx {
    A,
    B,
    Rival,
    Child,
    Bad,
    Orphan,
}

const TXS: [Tx; 6] = [Tx::A, Tx::B, Tx::Rival, Tx::Child, Tx::Bad, Tx::Orphan];
const VALID: [Tx; 4] = [Tx::A, Tx::B, Tx::Rival, Tx::Child];

impl Tx {
    /// Fee a valid one leaves (a trusted listing names the same)
    fn fee(self) -> u64 {
        match self {
            Tx::A | Tx::Rival | Tx::Child => 1_000,
            Tx::B => 500,
            Tx::Bad | Tx::Orphan => panic!("{self:?}: invalid, never priced"),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Peer {
    netgroup: u8,
    byzantine: bool,
}

/// - `Poll`: the validator answers, listing these (valid ones only: it validated them)
/// - `Round`: per peer `None` = unanswered, else the txs its mempool holds; then `wait`
#[derive(Debug, Clone)]
enum Move {
    Poll { validator: TrustedValidatorId, listed: BTreeSet<Tx> },
    Fail { validator: TrustedValidatorId },
    Kill { validator: TrustedValidatorId },
    Round { answers: Vec<Option<BTreeSet<Tx>>>, wait: Duration },
    Block,
    Send { tx: Tx },
}

/// What an honest node's mempool can hold: valid only, one of two rivals, a child with its parent
fn honest(mut held: BTreeSet<Tx>) -> BTreeSet<Tx> {
    held.retain(|tx| VALID.contains(tx));
    if held.contains(&Tx::B) {
        held.remove(&Tx::Rival);
    }
    if !held.contains(&Tx::A) {
        held.remove(&Tx::Child);
    }
    held
}

fn any_txs() -> impl Strategy<Value = BTreeSet<Tx>> {
    prop::collection::btree_set(prop::sample::select(TXS.to_vec()), 0..=TXS.len())
}

/// Honest peers' listings filtered here: the shrunk case shows each as it was answered
fn moves(validators: usize, peers: Vec<Peer>) -> impl Strategy<Value = Vec<Move>> {
    let validator =
        move || (0..validators).prop_map(|index| TrustedValidatorId::new(index).expect("≤ 2"));
    let answers: Vec<_> = peers
        .iter()
        .map(|peer| {
            let byzantine = peer.byzantine;
            let listing = any_txs().prop_map(move |txs| if byzantine { txs } else { honest(txs) });
            proptest::option::weighted(0.85, listing)
        })
        .collect();
    let wait = (0u64..=20).prop_map(Duration::from_secs);
    let step = prop_oneof![
        3 => (validator(), any_txs().prop_map(honest))
            .prop_map(|(validator, listed)| Move::Poll { validator, listed }),
        1 => validator().prop_map(|validator| Move::Fail { validator }),
        1 => validator().prop_map(|validator| Move::Kill { validator }),
        5 => (answers, wait).prop_map(|(answers, wait)| Move::Round { answers, wait }),
        1 => Just(Move::Block),
        1 => prop::sample::select(VALID.to_vec()).prop_map(|tx| Move::Send { tx }),
    ];
    prop::collection::vec(step, 1..48)
}

#[derive(Debug, Clone)]
struct Case {
    validators: usize,
    peers: Vec<Peer>,
    moves: Vec<Move>,
}

fn case() -> impl Strategy<Value = Case> {
    let peer = (0u8..8, prop::bool::weighted(0.2))
        .prop_map(|(netgroup, byzantine)| Peer { netgroup, byzantine });
    (1usize..=2, prop::collection::vec(peer, 6..=10)).prop_flat_map(|(validators, peers)| {
        moves(validators, peers.clone()).prop_map(move |moves| Case {
            validators,
            peers: peers.clone(),
            moves,
        })
    })
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

/// `listings` = per peer its last answer and when
struct Oracle {
    health: Vec<Health>,
    listed: Vec<BTreeSet<Tx>>,
    polled_since_block: Vec<bool>,
    listings: BTreeMap<usize, (Instant, BTreeSet<Tx>)>,
    round: Option<Instant>,
    held: BTreeMap<Tx, Held>,
}

impl Oracle {
    fn fresh(&self) -> Vec<(usize, &BTreeSet<Tx>)> {
        let Some(round) = self.round else { return Vec::new() };
        let fresh = self.listings.iter().filter(|(_, (at, _))| round - *at < LISTING_FRESH);
        fresh.map(|(peer, (_, txs))| (*peer, txs)).collect()
    }

    fn listers(&self, tx: Tx) -> Vec<usize> {
        self.fresh().into_iter().filter(|(_, txs)| txs.contains(&tx)).map(|(p, _)| p).collect()
    }

    fn trusted(&self, tx: Tx) -> bool {
        (0..self.health.len())
            .any(|v| self.health[v] != Health::Down && self.listed[v].contains(&tx))
    }

    fn listed(&self, tx: Tx) -> bool {
        self.trusted(tx) || self.held.get(&tx).is_some_and(|held| held.quorum)
    }

    /// The merge's drop rule after any change
    fn settle(&mut self, moved: bool) {
        let gone: Vec<Tx> = self.held.keys().copied().collect();
        for tx in gone {
            let ours = self.held[&tx].ours;
            if !self.listed(tx) && (moved || !ours) {
                self.held.remove(&tx);
            }
        }
    }

    fn sight(&mut self, tx: Tx) {
        let held = self.held.entry(tx).or_insert(Held { fee: None, quorum: false, ours: false });
        held.fee = held.fee.or(Some(tx.fee()));
    }
}

/// One case: the system + its oracle, moved in lockstep (`txs` in `TXS` order)
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
            telemetry: zaino_source::Telemetry::default(),
            streaming: false,
        }
    }

    fn txid(&self, tx: Tx) -> TxId {
        self.txs[tx as usize].0
    }

    fn tx(&self, txid: &TxId) -> Tx {
        TXS[self.txs.iter().position(|(t, _)| t == txid).expect("one of the case's")]
    }

    fn id(&self, tx: Tx) -> PeerTxId {
        PeerTxId { txid: self.txid(tx), auth_digest: None }
    }

    fn apply(&mut self, step: &Move) {
        match step {
            Move::Poll { validator, listed } => {
                let v = validator.get();
                let report = TrustedValidatorReport::Read {
                    reading: self.reading(),
                    health: zaino_traffic::Health::Live,
                    listed: true,
                };
                self.core.apply(*validator, report);
                let sighted = |tx: &Tx| Sighted {
                    txid: self.txid(*tx),
                    raw: Some(self.txs[*tx as usize].1.clone()),
                    fee: Zatoshis::from_u64(tx.fee()).expect("in supply"),
                };
                let listing = listed.iter().map(sighted).collect();
                self.core.apply(*validator, TrustedValidatorReport::Listed(listing));
                self.oracle.health[v] = Health::Live;
                self.oracle.polled_since_block[v] = true;
                self.oracle.listed[v] = listed.clone();
                listed.iter().for_each(|tx| self.oracle.sight(*tx));
                self.oracle.settle(false);
            }
            Move::Fail { validator } => {
                let failed = TrustedValidatorReport::Failed(zaino_traffic::Health::Degraded);
                self.core.apply(*validator, failed);
                self.oracle.health[validator.get()] = Health::Degraded;
                self.oracle.settle(false);
            }
            Move::Kill { validator } => {
                let killed = TrustedValidatorReport::Failed(zaino_traffic::Health::Down);
                self.core.apply(*validator, killed);
                self.oracle.health[validator.get()] = Health::Down;
                self.oracle.listed[validator.get()].clear();
                self.oracle.settle(false);
            }
            Move::Round { answers, wait } => self.round(answers, *wait),
            Move::Block => {
                self.tip = self.chain.mine_empty(1);
                let snapshot = HeaderChainSnapshot::of(&self.chain, self.tip);
                self.core.apply_headers(Some(Arc::new(snapshot)));
                self.oracle.polled_since_block.iter_mut().for_each(|polled| *polled = false);
                self.oracle.settle(true);
            }
            Move::Send { tx } => {
                let (txid, raw) = self.txs[*tx as usize].clone();
                self.core.mark_ours(txid, raw);
                let held = self.oracle.held.entry(*tx);
                held.or_insert(Held { fee: None, quorum: false, ours: false }).ours = true;
            }
        }
    }

    fn round(&mut self, answers: &[Option<BTreeSet<Tx>>], wait: Duration) {
        let now = self.start + self.clock;
        let ids = |txs: &BTreeSet<Tx>| txs.iter().map(|tx| self.id(*tx)).collect();
        let real =
            answers.iter().enumerate().map(|(peer, txs)| (self.peers[peer], txs.as_ref().map(ids)));
        let candidates = self.core.apply_listings(real.collect(), now);
        let view = self.core.current();
        let next = NextBlock::above(self.tip.height, &self.chain.schedule().upgrades);
        let fetched = candidates.into_iter().map(|(id, raw)| {
            let tx = self.tx(&id.txid);
            (id, raw.unwrap_or_else(|| self.txs[tx as usize].1.clone()))
        });
        let judged = judge(&view, &self.utxos, next, fetched.collect());
        let admitted = judged.into_iter().filter_map(|(id, raw, fee)| Some((id, raw, fee.ok()?)));
        self.core.admit_by_quorum(admitted.collect());

        // oracle: listings, retention + drop, then admission against the held set before it
        for (peer, txs) in answers.iter().enumerate() {
            if let Some(txs) = txs {
                self.oracle.listings.insert(peer, (now, txs.clone()));
            }
        }
        self.oracle.round = Some(now);
        let polled = self.oracle.fresh().len();
        let netgroups = |peers: &[usize]| {
            peers.iter().map(|p| Netgroup::of(self.peers[*p])).collect::<BTreeSet<_>>().len()
        };
        for tx in TXS {
            let retained = self.oracle.listers(tx).len() * 3 > polled;
            if let Some(held) = self.oracle.held.get_mut(&tx) {
                held.quorum &= retained;
            }
        }
        self.oracle.settle(false);
        let before: BTreeSet<Tx> = self.oracle.held.keys().copied().collect();
        for tx in TXS {
            let listers = self.oracle.listers(tx);
            let admits = listers.len() * 3 > polled * 2 && netgroups(&listers) > ECLIPSE_NETGROUPS;
            let passes = match tx {
                Tx::Child => before.contains(&Tx::A),
                Tx::Bad | Tx::Orphan => false,
                Tx::A | Tx::B | Tx::Rival => true,
            };
            let already = self.oracle.held.get(&tx).is_some_and(|held| held.quorum);
            if admits && passes && !already {
                self.oracle.sight(tx);
                self.oracle.held.get_mut(&tx).expect("just held").quorum = true;
            }
        }
        self.clock += wait;
    }

    /// `rivals_before`: B and Rival both quorum-held before `step`; `Ok` = both now
    fn check(&self, step: &Move, rivals_before: bool) -> Result<bool, TestCaseError> {
        let view = self.core.current();
        let real: BTreeMap<Tx, Held> = view
            .mempool
            .iter()
            .map(|(txid, sighting)| {
                let fee = sighting.entry(*txid).fee.map(Zatoshis::into_u64);
                (self.tx(txid), Held { fee, quorum: sighting.quorum(), ours: sighting.ours() })
            })
            .collect();
        prop_assert_eq!(&real, &self.oracle.held, "after {:?}: held mempool, real vs oracle", step);

        let holder = (0..self.oracle.health.len())
            .any(|v| self.oracle.health[v] == Health::Live && self.oracle.polled_since_block[v]);
        let fresh = self.oracle.fresh();
        let netgroups: BTreeSet<Netgroup> =
            fresh.iter().map(|(p, _)| Netgroup::of(self.peers[*p])).collect();
        let quorate = netgroups.len() > ECLIPSE_NETGROUPS;
        prop_assert_eq!(
            view.mempool().is_some(),
            holder || quorate,
            "after {:?}: mempool served vs (a live validator polled since the block {} || {} fresh \
             netgroups > {})",
            step,
            holder,
            netgroups.len(),
            ECLIPSE_NETGROUPS
        );

        // - admission > 2/3 + retention > 1/3 → overlap
        // - held on afterwards by an honest split
        let rivals = [Tx::B, Tx::Rival].iter().all(|tx| real.get(tx).is_some_and(|h| h.quorum));
        if rivals && !rivals_before {
            let both =
                fresh.iter().any(|(_, txs)| txs.contains(&Tx::B) && txs.contains(&Tx::Rival));
            prop_assert!(
                both,
                "after {:?}: rivals newly both quorum-held, no fresh peer lists both",
                step
            );
        }
        let all_honest = !self.byzantine.contains(&true);
        prop_assert!(
            !(rivals && all_honest),
            "after {:?}: rivals both quorum-held, every peer honest",
            step
        );
        Ok(rivals)
    }
}

fn run(case: &Case) -> Result<(), TestCaseError> {
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
    let peers: Vec<SocketAddr> = case
        .peers
        .iter()
        .enumerate()
        .map(|(n, peer)| SocketAddr::from(([10, peer.netgroup, 0, n as u8 + 1], 8233)))
        .collect();

    let tip = chain.tip();
    let utxos = chain.ledger(tip);
    let validators = case.validators;
    let trusted_validators: Vector<TrustedValidatorMeta> =
        (0..validators).map(|v| TrustedValidatorMeta::new(format!("v{v}:8232"))).collect();
    let core = ChainViewCore::new(trusted_validators, FinalityDepth::CONSENSUS);
    core.apply_headers(Some(Arc::new(HeaderChainSnapshot::of(&chain, tip))));
    let oracle = Oracle {
        health: vec![Health::Pending; validators],
        listed: vec![BTreeSet::new(); validators],
        polled_since_block: vec![false; validators],
        listings: BTreeMap::new(),
        round: None,
        held: BTreeMap::new(),
    };
    let txs = txs.into_iter().map(|(txid, raw)| (txid, Bytes::from(raw))).collect();
    let byzantine = case.peers.iter().map(|peer| peer.byzantine).collect();
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
    for step in &case.moves {
        run.apply(step);
        rivals = run.check(step, rivals)?;
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// 1–2 trusted validators, 6–10 peers over ≤ 8 netgroups (some byzantine): see the module doc
    #[test]
    fn the_mempool_holds_exactly_what_a_live_validator_a_peer_quorum_or_we_vouch_for(
        case in case(),
    ) {
        run(&case)?;
    }
}
