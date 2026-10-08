//! Publisher model (`global-snapshot.md` §6): random inputs through [`Core::publish`], `check()`
//! after every publish (inside it), every snapshot against a naive model
//!
//! - Chain view: header moves (extend, reorg above final, finalize), holders, mempool (relays,
//!   unlisted sightings turning servable, drops)
//! - NFS: served tip on the best (lagging), on an old best (a reorg in flight), or nothing yet
//! - Each move published or coalesced with the next (the run loop's `watch`es keep the latest)
//! - Tails opened at random, drained after every publish: each tx once per epoch, every one the
//!   epoch carried, end iff the served tip moved and only once the new snapshot is stored

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;
use std::sync::Arc;

use bytes::Bytes;
use futures::FutureExt;
use proptest::prelude::*;
use zaino_chainview::{ChainViewSnapshot, EndpointSet};
use zaino_header_chain::{HeaderChain, VerifiedChain};
use zaino_index_tree_state::PoolActivations;
use zaino_nfs::{ChainParams, Indexed};
use zaino_persistence::{DiskView, IndexKind};
use zaino_primitives::testing::Chain;
use zaino_primitives::types::{BlockHash, BlockRef, Height, ReorgDepth, TransactionId};
use zcash_protocol::consensus::NetworkType;

use crate::publisher::Core;
use crate::{MempoolTail, Snapshot, Tips, Unready};

const DEPTH: ReorgDepth = ReorgDepth::new(NonZeroU32::new(3).expect("nonzero"));
const ADDRESSES: [&str; 2] = ["10.0.0.1:8232", "10.0.0.2:8232"];

/// - `Reorg`: top `depth` (above final) replaced by `len` heavier blocks
/// - `Sight` = held, unlisted; `List` = a held one turned servable; `Relay` = ours (servable)
/// - `ServeBest` = NFS `lag` below the best; `ServeOld` = an earlier best tip (reorg in flight)
/// - paired `bool` = published now (else coalesced with the next)
#[derive(Debug, Clone)]
enum Input {
    Extend(u32),
    Reorg { depth: u32, len: u32 },
    Finalize,
    Hold(u8),
    Relay(u8),
    Sight(u8),
    List(u8),
    Drop(u8),
    ServeBest { lag: u32 },
    ServeOld { pick: u8 },
    Tail,
}

fn moves() -> impl Strategy<Value = Vec<(Input, bool)>> {
    let input = prop_oneof![
        3 => (1u32..=4).prop_map(Input::Extend),
        2 => (1u32..=5, 1u32..=4).prop_map(|(depth, len)| Input::Reorg { depth, len }),
        1 => Just(Input::Finalize),
        2 => (0u8..4).prop_map(Input::Hold),
        2 => any::<u8>().prop_map(Input::Relay),
        2 => any::<u8>().prop_map(Input::Sight),
        2 => any::<u8>().prop_map(Input::List),
        1 => any::<u8>().prop_map(Input::Drop),
        4 => (0u32..=5).prop_map(|lag| Input::ServeBest { lag }),
        1 => any::<u8>().prop_map(|pick| Input::ServeOld { pick }),
        2 => Just(Input::Tail),
    ];
    prop::collection::vec((input, prop::bool::weighted(0.7)), 1..64)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    /// Every publish: seq + 1, tips = the naive tips of the latest inputs (G2–G4, G6), unready
    /// reasons; every tail = its epoch exactly, ended iff the served tip moved past it (G5)
    #[test]
    fn every_publish_composes_the_latest_inputs_and_tails_follow_the_served_tip(moves in moves()) {
        run(moves);
    }
}

/// One epoch as the model sees it: `txs` = servable at its opening ∪ every later arrival
struct Epoch {
    key: Option<BlockRef>,
    txs: BTreeSet<TransactionId>,
    closed: bool,
}

struct Tailed {
    tail: MempoolTail,
    epoch: usize,
    received: Vec<TransactionId>,
    ended: bool,
}

/// `held` = bitmask over `ADDRESSES`; `mempool` = txid → servable
struct Model {
    builder: Chain,
    headers: HeaderChain,
    old_tips: Vec<BlockRef>,
    held: u8,
    mempool: BTreeMap<TransactionId, bool>,
    served: Option<BlockRef>,
    synced: bool,
    epochs: Vec<Epoch>,
}

impl Model {
    fn chain(&self) -> Arc<VerifiedChain> {
        Arc::new(self.headers.verified().expect("genesis verified"))
    }

    fn held_by(&self) -> EndpointSet {
        EndpointSet::at((0..ADDRESSES.len()).filter(|at| self.held & (1 << at) != 0))
    }

    fn view(&self) -> Arc<ChainViewSnapshot> {
        let raw = |txid: &TransactionId| (*txid, Bytes::copy_from_slice(&<[u8; 32]>::from(*txid)));
        let of = |servable: bool| -> Vec<(TransactionId, Bytes)> {
            let picked = self.mempool.iter().filter(|(_, s)| **s == servable);
            picked.map(|(txid, _)| raw(txid)).collect()
        };
        let (ours, unlisted) = (of(true), of(false));
        let chain = Some(self.chain());
        Arc::new(ChainViewSnapshot::fixed(chain, self.held_by(), &ADDRESSES, &ours, &unlisted))
    }

    fn indexed(&self) -> Option<Arc<Indexed<DiskView>>> {
        let params = ChainParams { network: NetworkType::Regtest, activations: ACTIVATIONS };
        let none: [(IndexKind, DiskView); 0] = [];
        Some(Arc::new(Indexed::fixed(self.chain(), self.served?, params, none)))
    }

    fn servable(&self) -> BTreeSet<TransactionId> {
        self.mempool.iter().filter(|(_, servable)| **servable).map(|(txid, _)| *txid).collect()
    }

    /// Path membership by the builder, not `hash_at`
    fn on_best(&self, at: BlockRef) -> bool {
        let best = self.headers.best().expect("genesis verified").block;
        let path = self.builder.path(best.hash);
        path.get(u32::from(at.height) as usize).is_some_and(|block| block.header().hash == at.hash)
    }

    fn apply(&mut self, input: &Input) {
        let best = self.headers.best().expect("genesis verified").block;
        let txid = |pick: u8| TransactionId::from([pick; 32]);
        match *input {
            Input::Extend(count) => {
                let tip = self.builder.extend(best.hash, count);
                let above = u32::from(best.height) as usize + 1;
                self.headers.insert_blocks(&self.builder.path(tip.hash)[above..]).expect("valid");
            }
            Input::Reorg { depth, len } => {
                let floor = self.headers.final_tip().map_or(0, |tip| u32::from(tip.height));
                let at = u32::from(best.height).saturating_sub(depth).max(floor);
                let path: Vec<BlockHash> =
                    self.builder.path(best.hash).iter().map(|b| b.header().hash).collect();
                let (parent, replaced) = (path[at as usize], &path[at as usize + 1..]);
                let Some(heavy) = self.builder.mine_heavier(parent, replaced) else { return };
                let tip = self.builder.extend(heavy.hash, len - 1);
                let above = at as usize + 1;
                self.headers.insert_blocks(&self.builder.path(tip.hash)[above..]).expect("valid");
                if self.headers.best().expect("verified").block.hash != best.hash {
                    self.old_tips.push(best);
                }
            }
            Input::Finalize => {
                if let Some(boundary) = self.headers.finalizable() {
                    self.headers.finalize(boundary).expect("in-memory store");
                }
            }
            Input::Hold(mask) => self.held = mask,
            Input::Relay(pick) => drop(self.mempool.insert(txid(pick), true)),
            Input::Sight(pick) => drop(self.mempool.entry(txid(pick)).or_insert(false)),
            Input::List(pick) => {
                if let Some(servable) = self.mempool.get_mut(&txid(pick)) {
                    *servable = true;
                }
            }
            Input::Drop(pick) => drop(self.mempool.remove(&txid(pick))),
            Input::ServeBest { lag } => {
                let height = u32::from(best.height).saturating_sub(lag);
                let block = &self.builder.path(best.hash)[height as usize];
                let header = block.header();
                self.served = Some(BlockRef { hash: header.hash, height: header.height });
            }
            Input::ServeOld { pick } => {
                let old = self.old_tips.get(usize::from(pick) % self.old_tips.len().max(1));
                self.served = old.copied().or(self.served);
            }
            Input::Tail => {}
        }
    }

    /// Naive tips + the epoch bookkeeping one publish implies
    fn publish(&mut self) -> Tips {
        let chain = self.chain();
        let best = chain.best();
        let synced = match (self.served, self.synced) {
            (None, _) => false,
            (Some(served), false) => served == best,
            (Some(served), true) => {
                let behind = |at: BlockRef| u32::from(best.height) - u32::from(at.height);
                self.on_best(served) && behind(served) <= DEPTH.get()
            }
        };
        let served_moved = self.epochs.last().is_none_or(|epoch| epoch.key != self.served);
        let servable = self.servable();
        if served_moved {
            if let Some(epoch) = self.epochs.last_mut() {
                epoch.closed = true;
            }
            self.epochs.push(Epoch { key: self.served, txs: servable, closed: false });
        } else if let Some(epoch) = self.epochs.last_mut() {
            epoch.txs.extend(servable);
        }
        self.synced = synced;
        Tips {
            best: Some(best),
            final_tip: chain.final_tip(),
            served: self.served,
            held_by: self.held_by(),
            synced,
        }
    }
}

const ACTIVATIONS: PoolActivations =
    PoolActivations { sapling: Height::GENESIS, orchard: None, ironwood: None };

fn run(moves: Vec<(Input, bool)>) {
    let builder = Chain::new();
    let genesis = builder.genesis().hash;
    let mut headers = HeaderChain::regtest_in_memory(genesis, DEPTH);
    headers.insert_blocks(&builder.path(genesis)).expect("genesis");
    let mut model = Model {
        builder,
        headers,
        old_tips: Vec::new(),
        held: 0,
        mempool: BTreeMap::new(),
        served: None,
        synced: false,
        epochs: vec![Epoch { key: None, txs: BTreeSet::new(), closed: false }],
    };
    let core = Core::new(model.indexed(), model.view(), DEPTH);
    let handle = core.handle();
    let mut seq = 0;
    let mut tails: Vec<Tailed> = Vec::new();

    for (step, (input, publish)) in moves.iter().enumerate() {
        let context = format!("step {step} {input:?}");
        model.apply(input);
        if *publish {
            core.publish(model.indexed(), model.view());
            let expected = model.publish();
            seq += 1;
            let snap = handle.load();
            assert_eq!((snap.seq(), snap.tips()), (seq, expected), "{context}: G2-G4 tips");
            assert_eq!(unready(&snap), unready_of(&expected), "{context}: unready");
        }
        let snap = handle.load();
        if matches!(input, Input::Tail) {
            match snap.mempool_stream() {
                Ok(tail) => {
                    let epoch = model.epochs.len() - 1;
                    let received = tail.opening().iter().map(|entry| entry.txid).collect();
                    tails.push(Tailed { tail, epoch, received, ended: false });
                }
                Err(refused) => {
                    let held = !snap.tips().held_by.is_empty();
                    assert!(!held, "{context}: G6 a held tip refused a stream: {refused}");
                }
            }
        }
        for tailed in tails.iter_mut().filter(|tailed| !tailed.ended) {
            while let Some(next) = tailed.tail.next().now_or_never() {
                match next {
                    Some(logged) => tailed.received.push(logged.entry.txid),
                    None => {
                        tailed.ended = true;
                        break;
                    }
                }
            }
            let epoch = &model.epochs[tailed.epoch];
            let received: BTreeSet<TransactionId> = tailed.received.iter().copied().collect();
            assert_eq!(received.len(), tailed.received.len(), "{context}: G5 a tx sent twice");
            assert_eq!(received, epoch.txs, "{context}: G5 tail = its epoch's transactions");
            assert_eq!(tailed.ended, epoch.closed, "{context}: G5 ends iff the served tip moved");
            if tailed.ended {
                let stored = handle.load().tips().served;
                assert_ne!(stored, epoch.key, "{context}: G5 ended before the new tip stored");
            }
        }
    }
}

fn unready<V>(snap: &Snapshot<V>) -> Vec<Unready> {
    snap.unready().collect()
}

/// `/readyz` reasons from tips alone (a chain always verified here)
fn unready_of(tips: &Tips) -> Vec<Unready> {
    let mut reasons = Vec::new();
    if tips.held_by.is_empty() {
        reasons.push(Unready::TipNotHeld);
    }
    if !tips.synced {
        reasons.push(Unready::Syncing);
    }
    reasons
}
