//! Publisher model (`usage.md`): random inputs through [`Core::publish`], `check()`
//! after every publish (inside it), every snapshot against a naive model
//!
//! - Chain view: header moves (extend, reorg above finalized, finalize), holders (some claiming
//!   past best), mempool (relays, drops), a block mining a held tx while the view still lists it
//!   (listings lag a block)
//! - NFS: served tip on the best (lagging), on an old best (a reorg in flight), or nothing yet
//! - NFS root = genesis (every served block above it)
//! - Each move published or coalesced with the next (the run loop's `watch`es keep the latest)
//! - Tails opened at random, drained after every publish: each tx once per epoch, every one the
//!   epoch carried, end iff the served tip moved and only once the new snapshot is stored

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;
use std::sync::Arc;

use bytes::Bytes;
use futures::FutureExt;
use proptest::prelude::*;
use zaino_chainview::{ChainViewSnapshot, TrustedValidatorSet};
use zaino_header_chain::testing::insert;
use zaino_header_chain::{HeaderChain, HeaderChainSnapshot};
use zaino_nfs::{ChainParams, NfsPublication};
use zaino_persistence::DiskView;
use zaino_primitives::types::{BlockRef, FinalityDepth};
use zaino_source::testing::{h, raw_transaction, MockChain, QuietPanics};
use zaino_sync::IndexKind;
use zcash_protocol::TxId;

use crate::publisher::Core;
use crate::{MempoolTail, ServingStatus, Tips};

const DEPTH: FinalityDepth = FinalityDepth::new(NonZeroU32::new(3).expect("nonzero"));
const ADDRESSES: [&str; 2] = ["10.0.0.1:8232", "10.0.0.2:8232"];
/// Relayed transactions: `tx0`..`tx7`
const RELAYS: u8 = 8;

/// - `Reorg`: top `depth` (above finalized) replaced by `len` blocks, the first outweighing them
/// - `Hold` = bitmask over `ADDRESSES` of the validators holding best
/// - `Ahead` = holders in `mask` claim a block past best (header sync short of them)
/// - `Ahead.read` = their hash at best polled (still holders), else not yet (no longer holders)
/// - `Relay` = ours (held, served)
/// - `Mine` = one block on best carrying that txid (kept held)
/// - `ServeBest` = NFS `lag` below the best
/// - `ServeOld` = the best tip replaced `back` reorgs ago (0 = the latest; none = no move)
#[derive(Debug, Clone)]
enum Input {
    Extend(u32),
    Reorg { depth: u32, len: u32 },
    Finalize,
    Hold(u8),
    Ahead { mask: u8, read: bool },
    Relay(u8),
    Mine(u8),
    Drop(u8),
    ServeBest { lag: u32 },
    ServeOld { back: usize },
    Tail,
}

/// `publish` = false: coalesced with the next publish
#[derive(Clone)]
struct Move {
    input: Input,
    publish: bool,
}

/// One line per move, even under `{:#?}` (shrunk case fits a screen)
impl std::fmt::Debug for Move {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let coalesced = if self.publish { "" } else { ", coalesced" };
        write!(f, "{:?}{coalesced}", self.input)
    }
}

fn moves() -> impl Strategy<Value = Vec<Move>> {
    let input = prop_oneof![
        3 => (1u32..=4).prop_map(Input::Extend),
        2 => (1u32..=5, 1u32..=4).prop_map(|(depth, len)| Input::Reorg { depth, len }),
        1 => Just(Input::Finalize),
        2 => (0u8..4).prop_map(Input::Hold),
        2 => (0u8..4, any::<bool>()).prop_map(|(mask, read)| Input::Ahead { mask, read }),
        4 => (0..RELAYS).prop_map(Input::Relay),
        3 => (0..RELAYS).prop_map(Input::Mine),
        1 => (0..RELAYS).prop_map(Input::Drop),
        4 => (0u32..=5).prop_map(|lag| Input::ServeBest { lag }),
        1 => (0usize..8).prop_map(|back| Input::ServeOld { back }),
        2 => Just(Input::Tail),
    ];
    let one =
        (input, prop::bool::weighted(0.7)).prop_map(|(input, publish)| Move { input, publish });
    prop::collection::vec(one, 1..64)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    /// - every publish: seq + 1, tips = the naive tips of the latest inputs (G2–G4), serving
    ///   status, the mempool gate (G6)
    /// - every tail = its epoch exactly, ended iff the served tip moved past it (G5)
    #[test]
    fn every_publish_composes_the_latest_inputs_and_tails_follow_the_served_tip(moves in moves()) {
        let _quiet = QuietPanics::hold();
        run(moves);
    }
}

/// One epoch as the model sees it: `txs` = held at its opening ∪ every later arrival
struct Epoch {
    key: Option<BlockRef>,
    txs: BTreeSet<TxId>,
    closed: bool,
}

struct Tailed {
    tail: MempoolTail,
    epoch: usize,
    received: Vec<TxId>,
    ended: bool,
}

/// - `held` / `ahead` = bitmasks over `ADDRESSES` (ahead counts for holders only)
/// - `read` = `Ahead`'s
/// - `mempool` = our relays the view still lists (real bytes: `relayed(pick)`)
struct Model {
    builder: MockChain,
    headers: HeaderChain,
    old_tips: Vec<BlockRef>,
    held: u8,
    ahead: u8,
    read: bool,
    mempool: BTreeMap<TxId, Vec<u8>>,
    served: Option<BlockRef>,
    synced: bool,
    epochs: Vec<Epoch>,
}

fn relayed(pick: u8) -> (TxId, Vec<u8>) {
    raw_transaction(u32::from(pick), 0)
}

/// Txids by relay name (`tx3`), sorted
fn names<'a>(txids: impl IntoIterator<Item = &'a TxId>) -> Vec<String> {
    let name = |txid: &TxId| {
        let pick = (0..RELAYS).find(|pick| relayed(*pick).0 == *txid);
        pick.map_or_else(|| format!("{txid}"), |pick| format!("tx{pick}"))
    };
    let mut named: Vec<String> = txids.into_iter().map(name).collect();
    named.sort();
    named
}

impl Model {
    fn chain(&self) -> Arc<HeaderChainSnapshot> {
        Arc::new(self.headers.snapshot().expect("genesis verified"))
    }

    fn set(mask: u8) -> TrustedValidatorSet {
        TrustedValidatorSet::at((0..ADDRESSES.len()).filter(|at| mask & (1 << at) != 0))
    }

    fn view(&self) -> Arc<ChainViewSnapshot> {
        let raw = |(txid, raw): (&TxId, &Vec<u8>)| (*txid, Bytes::from(raw.clone()));
        let ours: Vec<(TxId, Bytes)> = self.mempool.iter().map(raw).collect();
        let chain = Some(self.chain());
        let view = ChainViewSnapshot::fixed(chain, Self::set(self.held), &ADDRESSES, &ours);
        let ahead = Self::set(self.held & self.ahead).positions();
        Arc::new(ahead.fold(view, |view, at| view.ahead(at, 1, self.read)))
    }

    /// Mempool served: a `Live` holder of best (every `fixed` holder `Live`, ahead + unread = no
    /// longer a holder), no peer listings here
    fn mempool_read(&self) -> bool {
        let unread_ahead = if self.read { 0 } else { self.ahead };
        self.held & !unread_ahead != 0
    }

    fn indexed(&self) -> Option<Arc<NfsPublication<DiskView>>> {
        let path = self.builder.blocks(self.served?);
        let params = ChainParams::of(&self.builder);
        let none: [(IndexKind, DiskView); 0] = [];
        let root = path[0].block_ref();
        Some(Arc::new(NfsPublication::fixed_above(self.chain(), root, &path[1..], params, none)))
    }

    /// Txids of the served path above genesis (by the builder, not `served_above_durable`)
    fn mined_under_served(&self) -> BTreeSet<TxId> {
        let path = self.served.map(|served| self.builder.blocks(served)).unwrap_or_default();
        path.iter().skip(1).flat_map(|block| block.transactions()).map(|tx| tx.txid).collect()
    }

    /// Path membership by the builder, not `hash_at`
    fn on_best(&self, at: BlockRef) -> bool {
        let best = self.headers.best().expect("genesis verified").block;
        let path = self.builder.blocks(best);
        let index = usize::try_from(u32::from(at.height)).expect("height fits usize");
        path.get(index).is_some_and(|block| block.header().hash == at.hash)
    }

    fn apply(&mut self, input: &Input) {
        let best = self.headers.best().expect("genesis verified").block;
        match *input {
            Input::Extend(count) => {
                let tip = self.builder.branch(best).mine_empty(count).tip();
                let above = usize::try_from(u32::from(best.height)).expect("fits usize") + 1;
                insert(&mut self.headers, &self.builder.blocks(tip)[above..]).expect("valid");
            }
            Input::Reorg { depth, len } => {
                let floor = self.headers.finalized_tip().map_or(0, |tip| u32::from(tip.height));
                let at = u32::from(best.height).saturating_sub(depth).max(floor);
                let parent = self.builder.ancestor(best, h(at));
                let tip = self.builder.branch(parent).outweigh().mine_empty(len).tip();
                let above = usize::try_from(at).expect("height fits usize") + 1;
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
            Input::Ahead { mask, read } => (self.ahead, self.read) = (mask, read),
            Input::Relay(pick) => drop(self.mempool.insert(relayed(pick).0, relayed(pick).1)),
            Input::Mine(pick) => {
                let (txid, raw) = relayed(pick);
                let path = self.builder.blocks(best);
                let on_path = path.iter().flat_map(|b| b.transactions()).any(|tx| tx.txid == txid);
                let branch = self.builder.branch(best);
                let tip = match on_path {
                    true => branch.mine_empty(1).tip(),
                    false => branch.mine(|block| block.raw_tx(raw)).tip(),
                };
                let above = usize::try_from(u32::from(best.height)).expect("fits usize") + 1;
                insert(&mut self.headers, &self.builder.blocks(tip)[above..]).expect("valid");
            }
            Input::Drop(pick) => drop(self.mempool.remove(&relayed(pick).0)),
            Input::ServeBest { lag } => {
                let height = u32::from(best.height).saturating_sub(lag);
                let index = usize::try_from(height).expect("height fits usize");
                self.served = Some(self.builder.blocks(best)[index].block_ref());
            }
            Input::ServeOld { back } => {
                self.served = self.old_tips.iter().rev().nth(back).copied().or(self.served);
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
            (Some(served), false) => served == best && self.held & self.ahead == 0,
            (Some(served), true) => {
                let behind = |at: BlockRef| u32::from(best.height) - u32::from(at.height);
                self.on_best(served) && behind(served) <= DEPTH.get()
            }
        };
        let served_moved = self.epochs.last().is_none_or(|epoch| epoch.key != self.served);
        let listed: BTreeSet<TxId> = self.mempool.keys().copied().collect();
        let held = &listed - &self.mined_under_served();
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
            finalized_tip: Some(chain.finalized_tip()),
            served: self.served,
            synced,
        }
    }
}

fn run(moves: Vec<Move>) {
    let builder = MockChain::regtest().varied_work();
    let headers = HeaderChain::anchored(&builder, DEPTH);
    let mut model = Model {
        builder,
        headers,
        old_tips: Vec::new(),
        held: 0,
        ahead: 0,
        read: false,
        mempool: BTreeMap::new(),
        served: None,
        synced: false,
        epochs: vec![Epoch { key: None, txs: BTreeSet::new(), closed: false }],
    };
    let core = Core::new(model.indexed(), model.view(), DEPTH);
    let handle = core.handle();
    let mut seq = 0;
    let mut tails: Vec<Tailed> = Vec::new();
    // (held, ahead, read, mempool read) as of the stored snapshot
    let mut stored = (0, 0, false, false);

    for (step, Move { input, publish }) in moves.iter().enumerate() {
        let context = format!("step {step} {input:?}");
        model.apply(input);
        if *publish {
            stored = (model.held, model.ahead, model.read, model.mempool_read());
        }
        let (held, ahead, read, mempool_read) = stored;
        let gate = || {
            format!(
                "{context}: mempool served iff a Live holder of best (G6): published holders \
                 {held:#04b}, ahead {ahead:#04b}, read {read}"
            )
        };
        if *publish {
            let prev = handle.load();
            core.publish(model.indexed(), model.view());
            let expected = model.publish();
            seq += 1;
            let grpc_snapshot = handle.load();
            crate::invariants::check(&prev, &grpc_snapshot, DEPTH);
            assert_eq!(
                (grpc_snapshot.seq, grpc_snapshot.tips()),
                (seq, expected),
                "{context}: (seq, tips) published (left) vs the naive model's (G2-G4)"
            );
            assert_eq!(
                grpc_snapshot.serving_status(EPOCH),
                serving_status_of(&expected),
                "{context}: serving status (left) vs the naive model's, synced {}",
                expected.synced
            );
            let served = grpc_snapshot.mempool().map(|_| ());
            assert_eq!(served.is_ok(), mempool_read, "{}: got {served:?}", gate());
        }
        let grpc_snapshot = handle.load();
        if matches!(input, Input::Tail) {
            match grpc_snapshot.mempool_stream() {
                Ok(tail) => {
                    assert!(mempool_read, "{}: a stream opened", gate());
                    let epoch = model.epochs.len() - 1;
                    let received = tail.opening().iter().map(|entry| entry.txid).collect();
                    tails.push(Tailed { tail, epoch, received, ended: false });
                }
                Err(refused) => {
                    assert!(!mempool_read, "{}: stream refused: {refused}", gate())
                }
            }
        }
        for (at, tailed) in tails.iter_mut().enumerate().filter(|(_, tailed)| !tailed.ended) {
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
            let key = epoch.key.map(|key| key.height);
            let tail = format!("{context}: tail {at} on the epoch at {key:?}");
            let received: BTreeSet<TxId> = tailed.received.iter().copied().collect();
            assert!(
                received.len() == tailed.received.len(),
                "{tail}: each tx once per stream (G5): streamed {:?}",
                names(&tailed.received)
            );
            assert!(
                received == epoch.txs,
                "{tail}: streamed = the epoch's transactions (G5): streamed {:?}, epoch {:?}",
                names(&received),
                names(&epoch.txs)
            );
            let message = "ended (left) iff the served tip moved past the epoch (G5)";
            assert_eq!(tailed.ended, epoch.closed, "{tail}: {message}");
            if tailed.ended {
                let stored = handle.load().tips().served;
                let message = "ends only once the new served tip is stored (G5)";
                assert_ne!(stored, epoch.key, "{tail}: {message}");
            }
        }
    }
}

/// Clock the model reads serving status at (before every block: every best fresh)
const EPOCH: i64 = 0;

/// From tips alone (every header run vouched, holders `Live`, the rest `Pending`)
fn serving_status_of(tips: &Tips) -> ServingStatus {
    match tips.synced {
        true => ServingStatus::Serving,
        false => ServingStatus::Syncing,
    }
}
