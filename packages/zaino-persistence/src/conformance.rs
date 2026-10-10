//! What every [`PersistenceEngine`] must answer, through the port alone (feature `testing`)
//!
//! - engine crate implements [`Subject`] once, runs [`history`] under proptest + [`contract`] as
//!   a plain test; `PROPTEST_CASES=1000` = its heavy run
//! - store driven as a writer drives it (apply, commit), under [`Overlay`]s kept as the NFS keeps
//!   them (with, rebase), against `Vec` / `BTreeMap` models
//! - storage-specific moves (power loss, background work, internal invariants) = [`Subject`]
//!   hooks with no-op defaults
//! - [`Model`] doubles as the expected state for an engine's own crash and fault tests

use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroUsize,
    panic::{catch_unwind, AssertUnwindSafe},
    path::Path,
};

use bytes::Bytes;
use proptest::{prelude::*, strategy::Union};
use zaino_primitives::types::{BlockHash, BlockRef, Height};
use zcash_protocol::consensus::NetworkType;

use crate::{
    manifest::IndexKind,
    overlay::{Overlay, OverlayView},
    port::{
        BlockChanges, MapRead, MapTable, PersistenceEngine, Schema, SequenceRead, SequenceTable,
        Store, Tables, View, Width,
    },
};

pub const BLOCKS: SequenceTable = SequenceTable::new(0, "blocks", Width::Variable);
pub const HEIGHTS: SequenceTable = SequenceTable::new(1, "heights", Width::fixed(8));
pub const NODES: SequenceTable = SequenceTable::new(2, "pool/nodes", Width::fixed(4));
pub const SCANNED: MapTable =
    MapTable::new(0, "scanned", Width::fixed(12), Width::fixed(8), 8).deletes();
pub const PROBED: MapTable = MapTable::new(1, "probed", Width::fixed(16), Width::fixed(4), 0);

/// Every shape an index declares: variable sequence, fixed one, one in a sub-directory, scoped map
/// with removals (`account ‖ seq`, read per account), insert-only point-lookup map (hash-like ids)
pub const TABLES: Tables = Tables::new(&[BLOCKS, HEIGHTS, NODES], &[SCANNED, PROBED]);

pub const SCHEMA: Schema = Schema::new(IndexKind::CompactBlock, 1, NetworkType::Regtest, TABLES);

/// Engine under test, over storage the suite can reopen (and, if it can, crash)
pub trait Subject {
    type Engine: PersistenceEngine<Store: Store<View: SequenceRead + MapRead>>;

    /// Engine over the storage as it stands now (reopen = fresh `open` through it)
    fn engine(&self) -> Self::Engine;

    fn path(&self) -> &Path;

    /// Storage replaced by what a crash right now would leave; `false` = no such image (step
    /// reopens instead)
    fn power_loss(&mut self) -> bool {
        false
    }

    /// Background work finished (merges); no-op where there is none
    fn settle(&self, _store: &StoreOf<Self>) {}

    /// Engine-internal invariants after every step (`just_opened` = a fresh open)
    fn check(&self, _store: &StoreOf<Self>, _just_opened: bool, _label: &str) {}
}

pub type StoreOf<S> = <<S as Subject>::Engine as PersistenceEngine>::Store;

/// Commits only when told (`write_buffer` never reached)
fn open<S: Subject>(subject: &S) -> StoreOf<S> {
    subject.engine().open(subject.path(), &SCHEMA, NonZeroUsize::MAX).expect("open")
}

/// Record `n` of `blocks`: 0 to 22 bytes (empty records included)
pub fn block(n: u32) -> Vec<u8> {
    vec![n as u8; n as usize % 23]
}

/// Uniform 8 bytes per account (engines may shard on leading key bytes)
fn account(n: u8) -> [u8; 8] {
    u64::from(n).wrapping_mul(0x9e37_79b9_7f4a_7c15).to_be_bytes()
}

pub fn scanned_key(owner: u8, seq: u32) -> Vec<u8> {
    [&account(owner)[..], &seq.to_be_bytes()].concat()
}

/// Uniform first 8 bytes, `n` in the last 4 (distinct per `n`)
pub fn probed_key(n: u32) -> Vec<u8> {
    let mut id = vec![0u8; 16];
    id[..8].copy_from_slice(&u64::from(n).wrapping_mul(0x9e37_79b9_7f4a_7c15).to_be_bytes());
    id[12..].copy_from_slice(&n.to_be_bytes());
    id
}

/// Tip of `BlockChanges` `n` (from 1): height `n - 1`, hash `[n; 32]`
pub fn block_ref(n: usize) -> BlockRef {
    salted_ref(n, 0)
}

/// [`block_ref`] on branch `salt` (0 = the first branch)
fn salted_ref(n: usize, salt: u8) -> BlockRef {
    let height = Height::try_from(n as u32 - 1).expect("small height");
    let mut hash = [n as u8; 32];
    hash[0] ^= salt;
    BlockRef { hash: BlockHash::from(hash), height }
}

/// `bytes` on branch `salt` (a dropped record ≠ its replacement)
fn salted(mut bytes: Vec<u8>, salt: u8) -> Vec<u8> {
    if let Some(first) = bytes.first_mut() {
        *first ^= salt;
    }
    bytes
}

/// Panic payload text (`panic!` with arguments → `String`, a literal → `&str`)
pub fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    match payload.downcast::<String>() {
        Ok(message) => *message,
        Err(payload) => {
            payload.downcast::<&str>().map(|message| message.to_string()).unwrap_or_default()
        }
    }
}

/// Every table's contents through the last [`advance`](Self::advance)
///
/// - `salt` = branch of the next advance's contents (0 outside [`history`]); `bytes` = inserted
///   item bytes (a lower bound on what a buffer holds: removals uncounted)
#[derive(Debug, Clone, Default)]
pub struct Model {
    advances: usize,
    tip: Option<BlockRef>,
    salt: u8,
    bytes: usize,
    blocks: Vec<Vec<u8>>,
    heights: Vec<Vec<u8>>,
    nodes: Vec<Vec<u8>>,
    scanned: BTreeMap<Vec<u8>, Vec<u8>>,
    removed: BTreeSet<Vec<u8>>,
    probed: BTreeMap<Vec<u8>, Vec<u8>>,
    seq: u32,
    ids: u32,
}

impl Model {
    /// Next block, applied here, returned as `BlockChanges`: `records` blocks + twice as many nodes,
    /// one height record, `removals` picks of `scanned` rows held before it (`pick % rows`, a pick
    /// repeated = removed once), one `scanned` row per listed owner (fresh seqs), `ids` fresh
    /// `probed` ids
    pub fn advance(
        &mut self,
        records: u8,
        owners: &[u8],
        ids: u16,
        removals: &[u16],
    ) -> BlockChanges {
        self.advances += 1;
        let tip = salted_ref(self.advances, self.salt);
        self.tip = Some(tip);
        let mut changes = BlockChanges::new(tip, SCHEMA);
        let held: Vec<Vec<u8>> = self.scanned.keys().cloned().collect();
        if !held.is_empty() {
            let picked: BTreeSet<&Vec<u8>> =
                removals.iter().map(|pick| &held[usize::from(*pick) % held.len()]).collect();
            for key in picked {
                changes.map(SCANNED).remove(key);
                self.scanned.remove(key);
                self.removed.insert(key.clone());
            }
        }
        for _ in 0..records {
            let record = block(self.blocks.len() as u32);
            self.append(&mut changes, BLOCKS, record);
            for _ in 0..2 {
                let node = (self.nodes.len() as u32).to_le_bytes().to_vec();
                self.append(&mut changes, NODES, node);
            }
        }
        let height = (self.advances as u64).to_be_bytes().to_vec();
        self.append(&mut changes, HEIGHTS, height);
        for &owner in owners {
            self.seq += 1;
            let key = scanned_key(owner, self.seq);
            let value = salted(u64::from(self.seq).to_be_bytes().to_vec(), self.salt);
            changes.map(SCANNED).insert(&key, &value);
            self.bytes += key.len() + value.len();
            self.scanned.insert(key, value);
        }
        for n in self.ids..self.ids + u32::from(ids) {
            let value = salted(n.to_be_bytes().to_vec(), self.salt);
            changes.map(PROBED).insert(&probed_key(n), &value);
            self.bytes += 16 + value.len();
            self.probed.insert(probed_key(n), value);
        }
        self.ids += u32::from(ids);
        changes
    }

    /// `record` on this branch, at the end of `table` here and in `changes`
    fn append(&mut self, changes: &mut BlockChanges, table: SequenceTable, record: Vec<u8>) {
        let record = salted(record, self.salt);
        changes.sequence(table).append(&record);
        self.bytes += record.len();
        let held = if table == BLOCKS {
            &mut self.blocks
        } else if table == HEIGHTS {
            &mut self.heights
        } else {
            &mut self.nodes
        };
        held.push(record);
    }

    /// `start` inclusive to `end` exclusive, as `(key, value)`
    fn scan(&self, start: &[u8], end: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
        let rows =
            self.scanned.iter().filter(|(key, _)| start <= key.as_slice() && key.as_slice() < end);
        rows.map(|(key, value)| (key.clone(), value.clone())).collect()
    }

    /// `owner` + `raw` seq reduced into 0 to `seq + 1` (lands on, between and past issued rows)
    fn bound(&self, (owner, raw): (u8, u32)) -> Vec<u8> {
        scanned_key(owner, raw % (self.seq + 2))
    }

    /// - every sequence record (one at a time + as ranges); full + per-owner scans
    /// - sampled values + a miss; values over every id issued + as many never issued (each asked
    ///   twice: answers in caller order)
    pub fn assert_view(&self, view: &(impl SequenceRead + MapRead), label: &str) {
        assert_eq!(view.tip(), self.tip, "{label}: tip");
        for (table, model) in
            [(BLOCKS, &self.blocks), (HEIGHTS, &self.heights), (NODES, &self.nodes)]
        {
            let (name, read) = (table.name, view.sequence(table));
            let len = model.len() as u64;
            assert_eq!(read.count(), len, "{label}: {name} count");
            let one_by_one: Vec<_> = (0..len).map(|at| read.record(at)).collect();
            let expected: Vec<_> =
                model.iter().map(|record| Some(Bytes::from(record.clone()))).collect();
            assert_eq!(one_by_one, expected, "{label}: {name} records");
            assert_eq!(read.record(len), None, "{label}: {name} past the end");
            assert_eq!(read.records(0..len), *model, "{label}: {name} whole range");
            let middle = len / 3..len - len / 3;
            let expected = &model[middle.start as usize..middle.end as usize];
            assert_eq!(read.records(middle), expected, "{label}: {name} middle range");
        }

        let scanned = view.map(SCANNED);
        let every = self.scan(&[0; 12], &[0xff; 12]);
        let all = scanned.range(&[0; 12], &[0xff; 12], usize::MAX).expect("unbounded");
        assert_eq!(owned(all), every, "{label}: full scan");
        for owner in [0u8, 3, 9] {
            let (from, to) = (scanned_key(owner, 0), scanned_key(owner, u32::MAX));
            let answer = scanned.range(&from, &to, usize::MAX).expect("unbounded");
            assert_eq!(owned(answer), self.scan(&from, &to), "{label}: owner {owner}");
        }
        for (key, value) in self.scanned.iter().step_by(7) {
            assert_eq!(scanned.value(key).as_deref(), Some(&value[..]), "{label}: {key:?}");
        }
        assert_eq!(scanned.value(&scanned_key(1, self.seq + 1)), None, "{label}: miss");
        // every removed key + a sample of held ones, as one batch and one by one
        let held = self.scanned.iter().step_by(3).map(|(key, value)| (key, Some(value)));
        let asked: Vec<(&Vec<u8>, Option<&Vec<u8>>)> =
            self.removed.iter().map(|key| (key, None)).chain(held).collect();
        let keys: Vec<&[u8]> = asked.iter().map(|(key, _)| key.as_slice()).collect();
        let answers: Vec<Option<Bytes>> =
            asked.iter().map(|(_, value)| value.map(|value| Bytes::from(value.clone()))).collect();
        assert_eq!(scanned.values(&keys), answers, "{label}: removed + held values");
        let singles: Vec<_> = keys.iter().map(|key| scanned.value(key)).collect();
        assert_eq!(singles, answers, "{label}: removed + held value");

        let probed = view.map(PROBED);
        let asked: Vec<Vec<u8>> =
            (0..2 * self.ids + 2).flat_map(|n| [probed_key(n), probed_key(n)]).collect();
        let answers: Vec<Option<Bytes>> =
            asked.iter().map(|id| self.probed.get(id).cloned().map(Bytes::from)).collect();
        let keys: Vec<&[u8]> = asked.iter().map(Vec::as_slice).collect();
        assert_eq!(probed.values(&keys), answers, "{label}: values");
        let singles: Vec<_> = keys.iter().map(|id| probed.value(id)).collect();
        assert_eq!(singles, answers, "{label}: value");
    }

    /// `range` over `from..to` at every limit around the answer's size: over the limit = `None`,
    /// never a truncated answer
    fn assert_scans(&self, view: &impl MapRead, (from, to): (&[u8], &[u8]), raw: u16, label: &str) {
        let expected = self.scan(from, to);
        let len = expected.len();
        for limit in [len.saturating_sub(1), len, len + 1, usize::from(raw) % (len + 2)] {
            let answer = view.map(SCANNED).range(from, to, limit).map(owned);
            let bounded = (len <= limit).then(|| expected.clone());
            assert_eq!(answer, bounded, "{label}: {from:?}..{to:?} at most {limit}");
        }
    }
}

fn owned(rows: Vec<(Bytes, Bytes)>) -> Vec<(Vec<u8>, Vec<u8>)> {
    rows.into_iter().map(|(key, value)| (key.to_vec(), value.to_vec())).collect()
}

/// Block above durable, as the NFS holds it: its changes, contents through it, its layer
struct Node {
    changes: BlockChanges,
    model: Model,
    layer: Overlay,
}

/// Oracle for [`history`]: `nodes` = blocks above `committed`, oldest first, the first `applied`
/// buffered in the store; `salt` = branch of the next node
#[derive(Default)]
struct Oracle {
    committed: Model,
    nodes: Vec<Node>,
    applied: usize,
    salt: u8,
}

impl Oracle {
    /// Contents through the last applied node (what `staged()` reads)
    fn buffered(&self) -> &Model {
        self.applied.checked_sub(1).map_or(&self.committed, |at| &self.nodes[at].model)
    }

    /// Contents through the newest node (a snapshot's)
    fn newest(&self) -> &Model {
        self.nodes.last().map_or(&self.committed, |node| &node.model)
    }

    /// Newest node's layer over `durable` (`durable` = committed)
    fn newest_view<V: View>(&self, durable: V) -> OverlayView<V> {
        let layer = self.nodes.last().map_or_else(|| Overlay::empty(&SCHEMA), |n| n.layer.clone());
        OverlayView::new(durable, layer)
    }

    /// Node on the current branch above the newest: parent's layer `.with` its changes
    fn grow(&mut self, (records, owners, ids, removals): (u8, &[u8], u16, &[u16])) {
        let (mut model, layer) = match self.nodes.last() {
            Some(node) => (node.model.clone(), node.layer.clone()),
            None => (self.committed.clone(), Overlay::empty(&SCHEMA)),
        };
        model.salt = self.salt;
        let changes = model.advance(records, owners, ids, removals);
        let layer = layer.with(&changes);
        self.nodes.push(Node { changes, model, layer });
    }

    /// Every node rebased onto `durable` (commit or reopen: what it now holds dropped)
    fn rebase(&mut self, durable: &impl View) {
        for node in &mut self.nodes {
            node.layer = node.layer.rebase(durable);
        }
    }

    /// Store view = committed; staged = through the last applied; node layers over the view =
    /// their contents (oldest + newest read in full); buffered bytes >= applied item bytes, 0 iff
    /// nothing applied
    fn assert<S: Store<View: SequenceRead + MapRead>>(&self, store: &S, label: &str) {
        self.committed.assert_view(&store.committed(), &format!("{label}: view"));
        self.buffered().assert_view(&store.staged(), &format!("{label}: staged"));
        let (buffered, items) = (store.buffered_bytes(), self.buffered().bytes);
        let items = items - self.committed.bytes;
        assert!(buffered >= items, "{label}: buffered bytes {buffered} < items {items}");
        assert_eq!(buffered == 0, self.applied == 0, "{label}: buffered bytes {buffered}");
        for node in &self.nodes {
            node.layer.check(label);
        }
        for node in [self.nodes.first(), self.nodes.last()].into_iter().flatten() {
            let view = OverlayView::new(store.committed(), node.layer.clone());
            node.model.assert_view(&view, &format!("{label}: node {:?}", node.model.tip));
        }
    }
}

/// Views taken at step `at` + the models they must keep answering (staged = a borrow: never pinned)
struct Pinned<V> {
    at: usize,
    committed: Model,
    newest: Model,
    view: V,
    node: OverlayView<V>,
}

/// - `Grow`: node above the newest (`records` blocks + nodes, one height, `removals` of held
///   scanned rows (committed, buffered or in a node), `owners` scanned rows, `ids` probed ids)
/// - `Apply`: oldest unapplied nodes into the store, `count` reduced at run time
/// - `Commit`: every buffered node durable, then every node rebased onto the new view
/// - `Reorg`: unapplied nodes cut to `keep` (reduced), later nodes on a new branch
/// - `Settle`: background work finishes; `Reopen`: exit mid-work; `PowerLoss`: crash now (both:
///   buffer lost, nodes kept and re-applied later)
/// - `Pin`: views + models kept, re-checked every later step
/// - `Scan`: raw bounds reduced at run time, every limit around the answer's size
#[derive(Debug, Clone)]
pub enum Step {
    Grow { records: u8, owners: Vec<u8>, ids: u16, removals: Vec<u16> },
    Apply { count: u8 },
    Commit,
    Reorg { keep: u8 },
    Settle,
    Reopen,
    PowerLoss,
    Pin,
    Scan { from: (u8, u32), to: (u8, u32), limit: u16 },
}

/// Swarm testing (TigerBeetle `tree_fuzz`): whole step kinds off per case (a uniform mix dilutes
/// rare interleavings: all reopens, no settles, only small blocks, no reorgs, …)
pub fn steps() -> impl Strategy<Value = Vec<Step>> {
    let on = prop::array::uniform8(prop::bool::ANY);
    on.prop_flat_map(|[settle, reopen, power_loss, pin, scan, large, reorg, removing]| {
        // small blocks; with `large`, sometimes past a 4 KiB block (205 scanned / 204 probed)
        let small = prop::collection::vec(0u8..4, 0..8);
        let (owners, ids) = match large {
            true => (
                prop_oneof![4 => small, 1 => prop::collection::vec(0u8..4, 250..600)].boxed(),
                prop_oneof![4 => 0u16..8, 1 => 200u16..600].boxed(),
            ),
            false => (small.boxed(), (0u16..8).boxed()),
        };
        // `removing`: up to as many removals as inserts, sometimes most of what is held (whole
        // segments cancel in merges)
        let removals = match removing {
            true => prop_oneof![
                4 => prop::collection::vec(any::<u16>(), 0..8),
                1 => prop::collection::vec(any::<u16>(), 100..600),
            ]
            .boxed(),
            false => Just(Vec::new()).boxed(),
        };
        let grow = (0u8..6, owners, ids, removals)
            .prop_map(|(records, owners, ids, removals)| Step::Grow {
                records,
                owners,
                ids,
                removals,
            })
            .boxed();
        let bound = (0u8..6, any::<u32>());
        let query = (bound.clone(), bound, any::<u16>()).prop_map(|(from, to, limit)| Step::Scan {
            from,
            to,
            limit,
        });
        let mut kinds = vec![
            (4, grow),
            (3, any::<u8>().prop_map(|count| Step::Apply { count }).boxed()),
            (2, Just(Step::Commit).boxed()),
        ];
        for (enabled, weight, kind) in [
            (reorg, 1, any::<u8>().prop_map(|keep| Step::Reorg { keep }).boxed()),
            (settle, 1, Just(Step::Settle).boxed()),
            (reopen, 1, Just(Step::Reopen).boxed()),
            (power_loss, 1, Just(Step::PowerLoss).boxed()),
            (pin, 1, Just(Step::Pin).boxed()),
            (scan, 2, query.boxed()),
        ] {
            if enabled {
                kinds.push((weight, kind));
            }
        }
        prop::collection::vec(Union::new_weighted(kinds), 1..40)
    })
}

/// History against the subject + `Oracle`, after every step:
///
/// - `committed()` = committed prefix; `staged()` = committed + buffered; buffered bytes >= applied
/// - each node's layer over the view = its contents; pinned views = their models
/// - crash or reopen = exactly the committed nodes; [`Subject::check`] + `Overlay::check` hold
pub fn history<S: Subject>(mut subject: S, steps: &[Step]) {
    let mut store = open(&subject);
    let mut oracle = Oracle::default();
    let mut pinned: Option<Pinned<<StoreOf<S> as Store>::View>> = None;

    for (at, step) in steps.iter().enumerate() {
        let label = format!("step {at} {step:?}");
        let mut reopened = false;
        match step {
            Step::Grow { records, owners, ids, removals } => {
                oracle.grow((*records, owners, *ids, removals))
            }
            Step::Apply { count } => {
                let unapplied = oracle.nodes.len() - oracle.applied;
                let count = usize::from(*count) % (unapplied + 1);
                for node in &oracle.nodes[oracle.applied..oracle.applied + count] {
                    store.apply(node.changes.clone());
                }
                oracle.applied += count;
            }
            Step::Commit => {
                store.commit().expect("commit");
                oracle.committed = oracle.buffered().clone();
                oracle.nodes.drain(..oracle.applied);
                oracle.applied = 0;
                oracle.rebase(&store.committed());
            }
            Step::Reorg { keep } => {
                let unapplied = oracle.nodes.len() - oracle.applied;
                oracle.nodes.truncate(oracle.applied + usize::from(*keep) % (unapplied + 1));
                oracle.salt = oracle.salt.wrapping_add(1);
            }
            Step::Settle => subject.settle(&store),
            // crash image taken while background work may still be writing: acked = durable
            Step::Reopen | Step::PowerLoss => {
                if matches!(step, Step::PowerLoss) {
                    subject.power_loss();
                }
                drop(store);
                store = open(&subject);
                oracle.applied = 0;
                oracle.rebase(&store.committed());
                reopened = true;
            }
            Step::Pin => {
                pinned = Some(Pinned {
                    at,
                    committed: oracle.committed.clone(),
                    newest: oracle.newest().clone(),
                    view: store.committed(),
                    node: oracle.newest_view(store.committed()),
                })
            }
            Step::Scan { from, to, limit } => {
                let newest = oracle.newest();
                let (from, to) = (newest.bound(*from), newest.bound(*to));
                let bounds = (from.as_slice(), to.as_slice());
                let staged = format!("{label}: staged");
                oracle.buffered().assert_scans(&store.staged(), bounds, *limit, &staged);
                let node = oracle.newest_view(store.committed());
                newest.assert_scans(&node, bounds, *limit, &format!("{label}: newest node"));
            }
        }
        subject.check(&store, reopened, &label);
        oracle.assert(&store, &label);
        if let Some(pin) = &pinned {
            let label = format!("{label}: pinned at step {}", pin.at);
            pin.committed.assert_view(&pin.view, &format!("{label}: view"));
            pin.newest.assert_view(&pin.node, &format!("{label}: newest node"));
        }
    }
}

/// Panic expected from `act` (any wording: each names its own invariant)
fn refused(what: &str, act: impl FnOnce()) {
    let acted = catch_unwind(AssertUnwindSafe(act));
    assert!(acted.is_err(), "{what}: done instead of panicking");
}

/// View = only a tip (what `Overlay::rebase` + `OverlayView::new` read)
#[derive(Clone)]
struct Tip(Option<BlockRef>);

impl View for Tip {
    fn tip(&self) -> Option<BlockRef> {
        self.0
    }

    fn schema(&self) -> &Schema {
        &SCHEMA
    }
}

/// Port's fixed promises, on a fresh subject:
///
/// - empty open; apply buffered (staged, not in the view) until commit; empty commit = no write
/// - identity refused across kind / format / network; reopen resumes at the tip; verify clean
/// - each store + layer misuse panics with nothing buffered, work continuing after
/// - `write_buffer` reached = committed by `apply` itself
pub fn contract<S: Subject>(subject: S) {
    let engine = subject.engine();
    let mut model = Model::default();
    let mut store = open(&subject);
    model.assert_view(&store.committed(), "fresh");
    model.assert_view(&store.staged(), "fresh staged");
    assert_eq!(store.schema(), &SCHEMA, "the store keeps its schema");
    assert_eq!(store.committed().schema(), &SCHEMA, "its views read by it");
    assert_eq!(store.path(), subject.path(), "the store keeps its path");

    let empty = model.clone();
    store.apply(model.advance(2, &[1, 2], 2, &[]));
    // removes a row the buffer holds: cancelled there, never written
    store.apply(model.advance(0, &[], 1, &[0]));
    empty.assert_view(&store.committed(), "applied, not committed: not in the view");
    model.assert_view(&store.staged(), "applied: staged");
    assert!(store.buffered_bytes() > model.bytes, "buffered bytes > applied items (+ overhead)");
    store.commit().expect("commit");
    model.assert_view(&store.committed(), "committed");
    model.assert_view(&store.staged(), "committed: staged = view");
    assert_eq!(store.buffered_bytes(), 0, "committed: nothing buffered");
    store.commit().expect("nothing buffered");
    model.assert_view(&store.committed(), "an empty commit writes nothing");
    drop(store);

    for (what, schema) in [
        ("network", Schema::new(IndexKind::CompactBlock, 1, NetworkType::Main, TABLES)),
        ("kind", Schema::new(IndexKind::TreeState, 1, NetworkType::Regtest, TABLES)),
        ("format", Schema::new(IndexKind::CompactBlock, 2, NetworkType::Regtest, TABLES)),
    ] {
        let refused = engine.open(subject.path(), &schema, NonZeroUsize::MAX).map(|_| ());
        assert!(refused.is_err(), "another {what} opened as this store");
    }

    let mut store = open(&subject);
    model.assert_view(&store.committed(), "reopened");
    let verified = engine.verify(subject.path(), &SCHEMA).expect("verify");
    assert!(verified.is_clean() && verified.heights == 2, "{verified:?}");

    // store preconditions: each a panic before anything is buffered
    let other = Overlay::empty(&Schema::new(
        IndexKind::CompactBlock,
        1,
        NetworkType::Regtest,
        Tables::new(&[], &[]),
    ));
    let foreign = SequenceTable::new(0, "blocks", Width::fixed(1));
    refused("changes for another schema", || store.apply(other.changes(block_ref(3))));
    refused("a tip at the committed one", || store.apply(store.changes(block_ref(2))));
    let mut twice = store.changes(block_ref(3));
    twice.map(PROBED).insert(&probed_key(99), &[0; 4]);
    twice.map(PROBED).insert(&probed_key(99), &[1; 4]);
    refused("a key twice in one changes", || store.apply(twice));
    let held_key = model.scanned.keys().next().cloned().expect("a scanned row committed");
    let mut removed_twice = store.changes(block_ref(3));
    removed_twice.map(SCANNED).remove(&held_key);
    removed_twice.map(SCANNED).remove(&held_key);
    refused("a key removed twice in one changes", || store.apply(removed_twice));
    let fresh = scanned_key(7, model.seq + 1);
    let mut inserted_removed = store.changes(block_ref(3));
    inserted_removed.map(SCANNED).insert(&fresh, &[0; 8]);
    inserted_removed.map(SCANNED).remove(&fresh);
    refused("a key inserted and removed by one changes", || store.apply(inserted_removed));
    refused("a removal from a table without `deletes()`", || {
        store.changes(block_ref(3)).map(PROBED).remove(&probed_key(0));
    });
    refused("a table of another schema", || {
        store.changes(block_ref(3)).sequence(foreign).append(&[0]);
    });
    refused("a read of another schema's table", || {
        store.committed().sequence(foreign).record(0);
    });
    let mut buffered = model.clone();
    // removes a committed row: a tombstone over it from the next segment
    store.apply(buffered.advance(1, &[3], 1, &[0]));
    refused("a tip at the buffered one", || store.apply(store.changes(block_ref(3))));
    let mut held = store.changes(block_ref(4));
    held.map(PROBED).insert(&probed_key(buffered.ids - 1), &[0; 4]);
    refused("a key the buffer holds", || store.apply(held));
    let mut removed_again = store.changes(block_ref(4));
    removed_again.map(SCANNED).remove(&held_key);
    refused("a key the buffer removed", || store.apply(removed_again));
    model.assert_view(&store.committed(), "after misuse: view");
    buffered.assert_view(&store.staged(), "after misuse: staged");
    store.commit().expect("commit after misuse");
    buffered.assert_view(&store.committed(), "commits continue after misuse");

    // layer preconditions (pure: `with` and `rebase` leave the layer as it was)
    let mut nodes = buffered.clone();
    let layer_removes = nodes.scanned.keys().next().cloned().expect("a scanned row committed");
    let layer = Overlay::empty(&SCHEMA).with(&nodes.advance(1, &[4], 1, &[0]));
    let mut held = layer.changes(block_ref(5));
    held.map(PROBED).insert(&probed_key(nodes.ids - 1), &[0; 4]);
    let mut removed_again = layer.changes(block_ref(5));
    removed_again.map(SCANNED).remove(&layer_removes);
    refused("with a tip not above the layer's", || drop(layer.with(&layer.changes(block_ref(4)))));
    refused("with another schema's tables", || drop(layer.with(&other.changes(block_ref(5)))));
    refused("with a key the layer holds", || drop(layer.with(&held)));
    refused("with a key the layer removed", || drop(layer.with(&removed_again)));
    refused("rebase onto another branch", || drop(layer.rebase(&Tip(Some(salted_ref(4, 1))))));
    refused("rebase past the layer", || drop(layer.rebase(&Tip(Some(block_ref(5))))));
    refused("a layer under durable", || {
        drop(OverlayView::new(Tip(Some(block_ref(4))), layer.clone()))
    });
    refused("a layer over another schema's view", || {
        drop(OverlayView::new(store.committed(), other.clone()))
    });
    let view = OverlayView::new(store.committed(), layer.rebase(&store.committed()));
    nodes.assert_view(&view, "layers continue after misuse");
    let rebased = layer.rebase(&Tip(Some(block_ref(4))));
    assert_eq!(rebased, Overlay::empty(&SCHEMA), "rebase onto its tip = empty");
    drop(store);

    let mut store = engine.open(subject.path(), &SCHEMA, NonZeroUsize::MIN).expect("open");
    store.apply(buffered.advance(1, &[4], 1, &[0]));
    buffered.assert_view(&store.committed(), "write_buffer reached: committed by apply");
    assert_eq!(store.buffered_bytes(), 0, "write_buffer reached: nothing left buffered");
}
