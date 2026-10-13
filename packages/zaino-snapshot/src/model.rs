//! Publisher model (`global-snapshot.md`): random inputs through [`Core::publish`], `check()`
//! after every publish (inside it), every snapshot against a naive model
//!
//! - Chain view: header moves (extend, reorg above final, finalize), holders, mempool (relays,
//!   drops), a block mining a held tx while the view still lists it (listings lag a block)
//! - NFS: served tip on the best (lagging), on an old best (a reorg in flight), or nothing yet;
//!   root = genesis (every served block above it nonfinalized)
//! - Each move published or coalesced with the next (the run loop's `watch`es keep the latest)
//! - Tails opened at random, drained after every publish: each tx once per epoch, every one the
//!   epoch carried, end iff the served tip moved and only once the new snapshot is stored

use std::collections::BTreeSet;
use std::num::NonZeroU32;
use std::sync::Arc;

use bytes::Bytes;
use futures::FutureExt;
use proptest::prelude::*;
use zaino_chainview::{ChainViewSnapshot, EndpointSet};
use zaino_header_chain::testing::{insert, HeaderViews};
use zaino_header_chain::{HeaderChain, VerifiedChain};
use zaino_nfs::{ChainParams, Indexed};
use zaino_persistence::{DiskView, IndexKind};
use zaino_primitives::testing::MockChain;
use zaino_primitives::types::{BlockRef, ReorgDepth, TransactionId};

use crate::publisher::Core;
use crate::{Down, MempoolTail, Standing, Tips};

const DEPTH: ReorgDepth = ReorgDepth::new(NonZeroU32::new(3).expect("nonzero"));
const ADDRESSES: [&str; 2] = ["10.0.0.1:8232", "10.0.0.2:8232"];

/// - `Reorg`: top `depth` (above final) replaced by `len` blocks, the first outweighing them
/// - `Relay` = ours (held, served); `Mine` = one block on best carrying that txid (kept held)
/// - `ServeBest` = NFS `lag` below the best; `ServeOld` = an earlier best tip (reorg in flight)
/// - paired `bool` = published now (else coalesced with the next)
#[derive(Debug, Clone)]
enum Input {
    Extend(u32),
    Reorg { depth: u32, len: u32 },
    Finalize,
    Hold(u8),
    Relay(u8),
    Mine(u8),
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
        4 => (0u8..8).prop_map(Input::Relay),
        3 => (0u8..8).prop_map(Input::Mine),
        1 => (0u8..8).prop_map(Input::Drop),
        4 => (0u32..=5).prop_map(|lag| Input::ServeBest { lag }),
        1 => any::<u8>().prop_map(|pick| Input::ServeOld { pick }),
        2 => Just(Input::Tail),
    ];
    prop::collection::vec((input, prop::bool::weighted(0.7)), 1..64)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    /// Every publish: seq + 1, tips = the naive tips of the latest inputs (G2–G4, G6),
    /// standing; every tail = its epoch exactly, ended iff the served tip moved past it (G5)
    #[test]
    fn every_publish_composes_the_latest_inputs_and_tails_follow_the_served_tip(moves in moves()) {
        run(moves);
    }
}

/// One epoch as the model sees it: `txs` = held at its opening ∪ every later arrival
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

/// `held` = bitmask over `ADDRESSES`; `mempool` = our relays still held
struct Model {
    builder: MockChain,
    headers: HeaderChain,
    old_tips: Vec<BlockRef>,
    held: u8,
    mempool: BTreeSet<TransactionId>,
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
        let ours: Vec<(TransactionId, Bytes)> = self.mempool.iter().map(raw).collect();
        let chain = Some(self.chain());
        Arc::new(ChainViewSnapshot::fixed(chain, self.held_by(), &ADDRESSES, &ours))
    }

    fn indexed(&self) -> Option<Arc<Indexed<DiskView>>> {
        let path = self.builder.blocks(self.served?);
        let params = ChainParams::of(&self.builder);
        let none: [(IndexKind, DiskView); 0] = [];
        let root = path[0].at();
        Some(Arc::new(Indexed::fixed_above(self.chain(), root, &path[1..], params, none)))
    }

    /// Txids of the served path above genesis (by the builder, not `served_nonfinalized`)
    fn mined_under_served(&self) -> BTreeSet<TransactionId> {
        let path = self.served.map(|served| self.builder.blocks(served)).unwrap_or_default();
        path.iter().skip(1).flat_map(|block| block.transactions()).map(|tx| tx.txid).collect()
    }

    /// Path membership by the builder, not `hash_at`
    fn on_best(&self, at: BlockRef) -> bool {
        let best = self.headers.best().expect("genesis verified").block;
        let path = self.builder.blocks(best);
        path.get(u32::from(at.height) as usize).is_some_and(|block| block.header().hash == at.hash)
    }

    fn apply(&mut self, input: &Input) {
        let best = self.headers.best().expect("genesis verified").block;
        let txid = |pick: u8| TransactionId::from([pick; 32]);
        match *input {
            Input::Extend(count) => {
                let tip = self.builder.branch(best).mine_empty(count).tip();
                let above = u32::from(best.height) as usize + 1;
                insert(&mut self.headers, &self.builder.blocks(tip)[above..]).expect("valid");
            }
            Input::Reorg { depth, len } => {
                let floor = self.headers.final_tip().map_or(0, |tip| u32::from(tip.height));
                let at = u32::from(best.height).saturating_sub(depth).max(floor);
                let parent = self.builder.blocks(best)[at as usize].at();
                let tip = self.builder.branch(parent).outweigh().mine_empty(len).tip();
                let above = at as usize + 1;
                insert(&mut self.headers, &self.builder.blocks(tip)[above..]).expect("valid");
                if self.headers.best().expect("verified").block.hash != best.hash {
                    self.old_tips.push(best);
                }
            }
            Input::Finalize => {
                if let Some(boundary) = self.headers.finalizable() {
                    self.headers.finalize(boundary);
                }
            }
            Input::Hold(mask) => self.held = mask,
            Input::Relay(pick) => drop(self.mempool.insert(txid(pick))),
            Input::Mine(pick) => {
                let path = self.builder.blocks(best);
                let on_path =
                    path.iter().flat_map(|b| b.transactions()).any(|tx| tx.txid == txid(pick));
                let branch = self.builder.branch(best);
                let tip = match on_path {
                    true => branch.mine_empty(1).tip(),
                    false => branch.mine(|block| block.tx(|tx| tx.txid([pick; 32]))).tip(),
                };
                let above = u32::from(best.height) as usize + 1;
                insert(&mut self.headers, &self.builder.blocks(tip)[above..]).expect("valid");
            }
            Input::Drop(pick) => drop(self.mempool.remove(&txid(pick))),
            Input::ServeBest { lag } => {
                let height = u32::from(best.height).saturating_sub(lag);
                self.served = Some(self.builder.blocks(best)[height as usize].at());
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
        let held = &self.mempool - &self.mined_under_served();
        if served_moved {
            if let Some(epoch) = self.epochs.last_mut() {
                epoch.closed = true;
            }
            self.epochs.push(Epoch { key: self.served, txs: held, closed: false });
        } else if let Some(epoch) = self.epochs.last_mut() {
            epoch.txs.extend(held);
        }
        self.synced = synced;
        Tips {
            best: Some(best),
            final_tip: Some(chain.final_tip()),
            served: self.served,
            held_by: self.held_by(),
            synced,
        }
    }
}

fn run(moves: Vec<(Input, bool)>) {
    let builder = MockChain::regtest().varied_work();
    let headers = builder.header_chain(DEPTH);
    let mut model = Model {
        builder,
        headers,
        old_tips: Vec::new(),
        held: 0,
        mempool: BTreeSet::new(),
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
            assert_eq!(snap.standing(EPOCH), standing_of(&expected), "{context}: standing");
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
                    let read = crate::compose::mempool_read(&snap);
                    assert!(!read, "{context}: G6 a read mempool refused a stream: {refused}");
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

/// From tips alone (a chain always verified; holders `Live`, the rest `Pending`)
/// The clock the model reads standing at: before every block, so every best is fresh
const EPOCH: i64 = 0;

fn standing_of(tips: &Tips) -> Standing {
    match (tips.held_by.is_empty(), tips.synced) {
        (true, true) => Standing::Degraded(Down::TipNotHeld),
        (true, false) => Standing::Down(Down::TipNotHeld),
        (false, false) => Standing::Syncing,
        (false, true) => Standing::Serving,
    }
}
