//! `DiskEngine` on `SimFs`: random histories against `Vec` / `BTreeMap` models, then files-only
//! concerns: every crash state, every failed I/O call, open's trimming + refusals, golden manifest
//! bytes, offline verify, each guard's panic

use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroU32,
    path::Path,
    sync::Arc,
};

use proptest::{prelude::*, strategy::Union};
use zaino_primitives::types::BlockHash;
use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::consensus::NetworkType;

use super::*;
use crate::{
    fs::{RealFs, SimFs},
    lsm::file_name,
    manifest::IndexKind,
    overlay::Overlay,
    port::{MapTable, SequenceTable, Tables, Width},
    sequence::Records,
};

const ROOT: &str = "/idx";

const BLOCKS: SequenceTable = SequenceTable::new(0, "blocks", Width::Variable);
const HEIGHTS: SequenceTable = SequenceTable::new(1, "heights", Width::fixed(8));
const NODES: SequenceTable = SequenceTable::new(2, "pool/nodes", Width::fixed(4));
const SCANNED: MapTable =
    MapTable::new(0, "scanned", Width::fixed(12), Width::fixed(8), 8).deletes();
const PROBED: MapTable = MapTable::new(1, "probed", Width::fixed(16), Width::fixed(4), 0);

/// Every shape an index declares: variable sequence, fixed one, one in a sub-directory, scoped map
/// with removals (`account ‖ seq`, read per account), insert-only point-lookup map (hash-like ids)
const TABLES: Tables = Tables::new(&[BLOCKS, HEIGHTS, NODES], &[SCANNED, PROBED]);

const SCHEMA: Schema = Schema::new(IndexKind::CompactBlock, 1, NetworkType::Regtest, TABLES);

/// Commits only when told (`write_buffer` never reached)
fn open(engine: &DiskEngine) -> DiskStore {
    engine.open(Path::new(ROOT), &SCHEMA, NonZeroUsize::MAX).expect("open")
}

impl DiskEngine {
    /// Merges after `fanout` same-tier segments (small = merges in a handful of commits)
    fn with_fanout(fs: Arc<dyn Fs>, fanout: usize) -> Self {
        let fanout = Fanout::new(fanout).expect("fanout >= 2");
        Self::new(fs, LsmConfig { fanout, ..LsmConfig::default() })
    }
}

impl DiskStore {
    /// Blocks until every running merge finishes (lands on the next commit)
    fn settle(&self) {
        for log in &self.maps {
            log.settle();
        }
    }
}

/// `fs`, flushing the whole device on any `sync_data` when `flush_device` (ext4 / XFS-like: commits
/// take the `Barrier` path)
fn sim_fs(fs: Arc<SimFs>, flush_device: bool) -> Arc<SimFs> {
    if flush_device {
        fs.flush_device();
    }
    fs
}

/// Record `n` of `blocks`: 0 to 22 bytes (empty records included)
fn block(n: u32) -> Vec<u8> {
    vec![n as u8; n as usize % 23]
}

/// Uniform 8 bytes per account (LSM may shard on leading key bytes)
fn account(n: u8) -> [u8; 8] {
    u64::from(n).wrapping_mul(0x9e37_79b9_7f4a_7c15).to_be_bytes()
}

fn scanned_key(owner: u8, seq: u32) -> Vec<u8> {
    [&account(owner)[..], &seq.to_be_bytes()].concat()
}

/// Uniform first 8 bytes, `n` in the last 4 (distinct per `n`)
fn probed_key(n: u32) -> Vec<u8> {
    let mut id = vec![0u8; 16];
    id[..8].copy_from_slice(&u64::from(n).wrapping_mul(0x9e37_79b9_7f4a_7c15).to_be_bytes());
    id[12..].copy_from_slice(&n.to_be_bytes());
    id
}

/// Tip of `BlockChanges` `n` (from 1): height `n - 1`, hash `[n; 32]`
fn block_ref(n: usize) -> BlockRef {
    salted_ref(n, 0)
}

/// [`block_ref`] on branch `salt` (0 = the first branch)
fn salted_ref(n: usize, salt: u8) -> BlockRef {
    let height = BlockHeight::from(n as u32 - 1);
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

/// Every table's contents through the last [`advance`](Self::advance)
///
/// - `salt` = branch of the next advance's contents (0 outside [`history`]); `bytes` = inserted
///   item bytes (a lower bound on what a buffer holds: removals uncounted)
#[derive(Debug, Clone, Default)]
struct Model {
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
    fn advance(&mut self, records: u8, owners: &[u8], ids: u16, removals: &[u16]) -> BlockChanges {
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
    fn assert_view(&self, view: &(impl SequenceRead + MapRead), label: &str) {
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
    fn newest_view(&self, durable: DiskView) -> OverlayView<DiskView> {
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
    fn rebase(&mut self, durable: &DiskView) {
        for node in &mut self.nodes {
            node.layer = node.layer.rebase(durable);
        }
    }

    /// Store view = committed; staged = through the last applied; node layers over the view =
    /// their contents (oldest + newest read in full); buffered bytes >= applied item bytes, 0 iff
    /// nothing applied
    fn assert(&self, store: &DiskStore, label: &str) {
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
struct Pinned {
    at: usize,
    committed: Model,
    newest: Model,
    view: DiskView,
    node: OverlayView<DiskView>,
}

/// - `Grow`: node above the newest (`records` blocks + nodes, one height, `removals` of held
///   scanned rows (committed, buffered or in a node), `owners` scanned rows, `ids` probed ids)
/// - `Apply`: oldest unapplied nodes into the store, `count` reduced at run time
/// - `Commit`: every buffered node durable, then every node rebased onto the new view
/// - `Reorg`: unapplied nodes cut to `keep` (reduced), later nodes on a new branch
/// - `Settle`: merges finish; `Reopen`: exit mid-merge; `PowerLoss`: crash now (both: buffer
///   lost, nodes kept and re-applied later)
/// - `Pin`: views + models kept, re-checked every later step
/// - `Scan`: raw bounds reduced at run time, every limit around the answer's size
#[derive(Debug, Clone)]
enum Step {
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
fn steps() -> impl Strategy<Value = Vec<Step>> {
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

/// History against `Oracle`, after every step:
///
/// - `committed()` = committed prefix; `staged()` = committed + buffered; buffered bytes >= applied
/// - each node's layer over the view = its contents; pinned views = their models
/// - crash or reopen = exactly the committed nodes; [`assert_tiers`] + `Overlay::check` hold
fn history(fanout: usize, flush_device: bool, steps: &[Step]) {
    let mut fs = sim_fs(SimFs::new(), flush_device);
    let mut store = open(&DiskEngine::with_fanout(fs.clone(), fanout));
    let mut oracle = Oracle::default();
    let mut pinned: Option<Pinned> = None;

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
            Step::Settle => store.settle(),
            // crash image taken while merges may still be writing: acked = durable
            Step::Reopen | Step::PowerLoss => {
                if matches!(step, Step::PowerLoss) {
                    fs = fs.power_loss();
                }
                drop(store);
                store = open(&DiskEngine::with_fanout(fs.clone(), fanout));
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
        assert_tiers(&store, &fs, fanout, reopened, &label);
        store.buffer.check(&label);
        oracle.assert(&store, &label);
        if let Some(pin) = &pinned {
            let label = format!("{label}: pinned at step {}", pin.at);
            pin.committed.assert_view(&pin.view, &format!("{label}: view"));
            pin.newest.assert_view(&pin.node, &format!("{label}: newest node"));
        }
    }
}

/// - each map's tiers within `fanout` inputs + `STALL_WINDOWS` (2) idle windows
/// - `just_opened`: every listed segment + sums on disk, beyond them only running merges' outputs
fn assert_tiers(store: &DiskStore, fs: &SimFs, fanout: usize, just_opened: bool, label: &str) {
    for (table, log) in SCHEMA.maps().iter().zip(&store.maps) {
        let name = table.name;
        let mut per_tier = BTreeMap::<u32, usize>::new();
        for segment in log.segments() {
            *per_tier.entry(segment.records.ilog(fanout as u64)).or_default() += 1;
        }
        let bounded = per_tier.values().all(|&segments| segments < 3 * fanout);
        assert!(bounded, "{label}: {name} segments per tier {per_tier:?}");
        if just_opened {
            let on_disk = fs.list(&Path::new(ROOT).join(name)).expect("list");
            for segment in log.segments() {
                let file = file_name(segment.id);
                let present =
                    [file.clone(), format!("{file}.crc")].iter().all(|f| on_disk.contains(f));
                assert!(present, "{label}: {name} lost listed {file}: {on_disk:?}");
            }
            let extra = on_disk.len() - 2 * log.segments().len();
            assert!(extra <= 2 * log.merging(), "{label}: {name} unlisted {on_disk:?}");
        }
    }
}

proptest! {
    // 64 cases ≈ seconds; heavy run after any change: docs/testing.md "Heavy run"
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    /// [`history`] at fanouts merging every few commits and rarely, per-file fsync and one-flush
    /// (`Barrier`) commits
    #[test]
    fn random_histories_answer_like_a_btreemap_through_merges_reopens_and_power_loss(
        fanout in prop_oneof![Just(2usize), Just(3), Just(8)],
        flush_device in any::<bool>(),
        steps in steps(),
    ) {
        history(fanout, flush_device, &steps);
    }
}

#[test]
fn a_store_at_the_smallest_write_buffer_commits_inside_apply() {
    let mut store = DiskEngine::with_fanout(SimFs::new(), 2)
        .open(Path::new(ROOT), &SCHEMA, NonZeroUsize::MIN)
        .expect("open");
    let mut changes = store.changes(block_ref(1));
    changes.sequence(BLOCKS).append(&block(0));

    store.apply(changes);

    assert_eq!(store.committed().tip(), Some(block_ref(1)));
    assert_eq!(store.buffered_bytes(), 0);
}

/// Fanout 2, merges launched + landed mid-history, some cancelling value + tombstone pairs; per
/// crash state: recovered = an acked or the attempted commit, every table = the model at that
/// commit (a removed row never back), only listed files, commits continue; per-file fsync and
/// one-flush (`Barrier`) commits
#[test]
fn every_crash_state_reopens_to_exactly_an_acknowledged_or_the_attempted_commit() {
    for flush_device in [false, true] {
        crash_states_reopen_to_a_commit(flush_device);
    }
}

fn crash_states_reopen_to_a_commit(flush_device: bool) {
    let (fs, recording) = SimFs::recording();
    let fs = sim_fs(fs, flush_device);
    let batches: [(u8, &[u8], u16, &[u16]); 4] =
        [(2, &[1, 2, 1], 2, &[]), (1, &[1, 3], 1, &[0]), (0, &[], 1, &[0, 1]), (3, &[4], 0, &[])];
    let mut models = vec![Model::default()];
    {
        let mut store = open(&DiskEngine::with_fanout(fs.clone(), 2));
        for (acked, (records, owners, ids, removals)) in (1u64..).zip(batches) {
            store.settle();
            let mut model = models.last().expect("seeded").clone();
            store.apply(model.advance(records, owners, ids, removals));
            store.commit().expect("commit");
            models.push(model);
            fs.set_tag(acked);
        }
        let merged = store.maps.iter().map(|log| log.segments().len()).collect::<Vec<_>>();
        // scanned: 3 + (2 + a tombstone) merged → 4 (one pair cancelled), 2 tombstones, 1 row
        assert_eq!(merged, [3, 2], "merges landed: scanned 4+2+1 rows, probed 2+2");
    }

    let states = recording.crash_states();
    assert!(states.len() > 50, "enumerated {} crash states", states.len());
    for state in states {
        let label = &format!("flush_device {flush_device}: {}", state.label);
        let mut store = DiskEngine::with_fanout(state.fs.clone(), 2)
            .open(Path::new(ROOT), &SCHEMA, NonZeroUsize::MAX)
            .unwrap_or_else(|error| panic!("{label}: {error}"));
        let recovered = store.committed().tip().map_or(0, |tip| u32::from(tip.height) as usize + 1);
        let acked = usize::try_from(state.tag).expect("small");
        assert!(recovered == acked || recovered == acked + 1, "{label}: recovered {recovered}");
        models[recovered].assert_view(&store.committed(), label);
        assert_tiers(&store, &state.fs, 2, true, label);

        let mut model = models[recovered].clone();
        store.apply(model.advance(1, &[9], 1, &[]));
        store.commit().expect("commit after recovery");
        model.assert_view(
            &store.committed(),
            &format!("{label}: commits continue at the recovered end"),
        );
    }
}

/// Op 0, 1, 2, … failed until the workload succeeds (Pebble `errorfs`); each = the injected `Err`
/// (never a panic, never swallowed), restart = acked or attempted commit; per-file fsync and
/// one-flush (`Barrier`) commits (a failed `write_back` included)
#[test]
fn every_failed_io_call_surfaces_poisons_the_store_and_recovers_to_a_committed_state() {
    for flush_device in [false, true] {
        failed_io_poisons_and_recovers(flush_device);
    }
}

fn failed_io_poisons_and_recovers(flush_device: bool) {
    let batches: [(u8, &[u8], u16, &[u16]); 3] =
        [(2, &[1, 2], 1, &[]), (1, &[1], 1, &[0]), (1, &[3], 2, &[1])];
    let mut models = vec![Model::default()];
    for (records, owners, ids, removals) in batches {
        let mut model = models.last().expect("seeded").clone();
        model.advance(records, owners, ids, removals);
        models.push(model);
    }

    let mut failures = 0;
    for fail_at in 0.. {
        assert!(fail_at < 10_000, "workload never succeeded");
        let fs = sim_fs(SimFs::new(), flush_device);
        fs.fail_from(fail_at);
        let engine = DiskEngine::with_fanout(fs.clone(), 2);
        let mut acked = 0;
        let error = match engine.open(Path::new(ROOT), &SCHEMA, NonZeroUsize::MAX) {
            Err(error) => error.to_string(),
            Ok(mut store) => {
                let mut failed = None;
                for at in 0..batches.len() {
                    store.settle();
                    let (records, owners, ids, removals) = batches[at];
                    store.apply(models[at].clone().advance(records, owners, ids, removals));
                    match store.commit() {
                        Ok(()) => acked = at + 1,
                        Err(error) => {
                            failed = Some(error);
                            break;
                        }
                    }
                }
                let failed = match failed {
                    Some(failed) => failed,
                    None => {
                        store.settle();
                        if fs.mutations() <= fail_at {
                            break;
                        }
                        // op `fail_at` hit a merge the last commit launched: surfaces next commit
                        store.apply(models[acked].clone().advance(0, &[], 0, &[]));
                        let next = store.commit();
                        next.expect_err("a failed background merge surfaces at the next commit")
                    }
                };
                failed.to_string()
            }
        };
        failures += 1;
        assert!(
            error.contains("injected EIO"),
            "flush_device {flush_device} op {fail_at}: {error}"
        );

        let fs = fs.restarted();
        let mut store = DiskEngine::with_fanout(fs.clone(), 2)
            .open(Path::new(ROOT), &SCHEMA, NonZeroUsize::MAX)
            .unwrap_or_else(|error| {
                panic!("flush_device {flush_device} op {fail_at}: reopen: {error}")
            });
        let recovered = store.committed().tip().map_or(0, |tip| u32::from(tip.height) as usize + 1);
        assert!(
            recovered == acked || recovered == acked + 1,
            "flush_device {flush_device} op {fail_at}: recovered {recovered}"
        );
        models[recovered]
            .assert_view(&store.committed(), &format!("flush_device {flush_device} op {fail_at}"));
        store.apply(models[recovered].clone().advance(1, &[9], 1, &[]));
        store.commit().expect("commit after restart");
    }
    assert!(failures > 40, "only {failures} failure points exercised");
}

/// Read 0, 1, 2, … failed while reopening a committed store until the open succeeds
///
/// - each failure = the injected `Err`, never a panic; successful open finds every commit
#[test]
fn every_failed_read_at_open_surfaces_and_a_clean_open_finds_every_commit() {
    let fs = SimFs::new();
    let mut model = Model::default();
    {
        let mut store = open(&DiskEngine::with_fanout(fs.clone(), 2));
        for n in 0..3u8 {
            store.apply(model.advance(2, &[n], 1, &[]));
            store.commit().expect("commit");
        }
    }

    let mut failures = 0;
    for fail_at in 0.. {
        assert!(fail_at < 1_000, "open never succeeded");
        let fs = fs.restarted();
        fs.fail_reads_from(fail_at);
        match DiskEngine::with_fanout(fs.clone(), 2).open(
            Path::new(ROOT),
            &SCHEMA,
            NonZeroUsize::MAX,
        ) {
            Ok(store) => {
                model.assert_view(&store.committed(), &format!("read {fail_at}"));
                break;
            }
            Err(error) => {
                assert!(error.to_string().contains("injected read EIO"), "read {fail_at}: {error}");
                failures += 1;
            }
        }
    }
    assert!(failures > 0, "open reads through positional reads (the manifest at least)");
}

// guards: each fires on the bug it guards, naming it (RocksDB: never seen firing = not known to work)

#[test]
#[should_panic(expected = "heights: a 7-byte item, width 8")]
fn a_sequence_item_off_its_width_panics() {
    BlockChanges::new(block_ref(1), SCHEMA).sequence(HEIGHTS).append(&[0; 7]);
}

#[test]
#[should_panic(expected = "scanned: a 11-byte item, width 12")]
fn a_map_key_off_its_width_panics() {
    BlockChanges::new(block_ref(1), SCHEMA).map(SCANNED).insert(&[0; 11], &[0; 8]);
}

#[test]
#[should_panic(expected = "compact_block: sequence stray not in its schema")]
fn a_write_to_a_sequence_outside_the_schema_panics() {
    let stray = SequenceTable::new(3, "stray", Width::Variable);
    BlockChanges::new(block_ref(1), SCHEMA).sequence(stray).append(&[]);
}

#[test]
#[should_panic(expected = "compact_block: map probed not in its schema")]
fn a_write_to_a_map_outside_the_schema_panics() {
    let stray = MapTable::new(1, "probed", Width::fixed(16), Width::fixed(8), 0);
    BlockChanges::new(block_ref(1), SCHEMA).map(stray).insert(&[0; 16], &[0; 8]);
}

#[test]
#[should_panic(expected = "compact_block: map probed not in its schema")]
fn a_read_of_a_map_outside_the_schema_panics() {
    let stray = MapTable::new(1, "probed", Width::fixed(16), Width::fixed(8), 0);
    open(&DiskEngine::with_fanout(SimFs::new(), 2)).committed().map(stray).value(&[0; 16]);
}

#[test]
#[should_panic(expected = "sequence id != its position")]
fn tables_with_a_sequence_id_off_its_position_panic() {
    static SKIPPED: [SequenceTable; 1] = [SequenceTable::new(1, "skipped", Width::Variable)];
    Tables::new(&SKIPPED, &[]);
}

#[test]
#[should_panic(expected = "changes built for another schema")]
fn changes_for_another_schema_panic() {
    let other = Schema::new(IndexKind::BlockHash, 1, NetworkType::Regtest, TABLES);
    open(&DiskEngine::with_fanout(SimFs::new(), 2)).apply(BlockChanges::new(block_ref(1), other));
}

#[test]
#[should_panic(expected = "apply at height 0, not above the last applied Some(BlockHeight(0))")]
fn apply_at_a_height_not_above_the_last_panics() {
    let mut store = open(&DiskEngine::with_fanout(SimFs::new(), 2));
    store.apply(store.changes(block_ref(1)));
    store.commit().expect("commit");
    store.apply(store.changes(block_ref(1)));
}

#[test]
#[should_panic(expected = "scanned: a map key inserted twice")]
fn a_key_inserted_twice_by_one_block_panics() {
    let mut store = open(&DiskEngine::with_fanout(SimFs::new(), 2));
    let mut changes = store.changes(block_ref(1));
    changes.map(SCANNED).insert(&scanned_key(1, 0), &[0; 8]);
    changes.map(SCANNED).insert(&scanned_key(1, 0), &[0; 8]);
    store.apply(changes);
}

#[test]
#[should_panic(expected = "scanned: a map key removed twice")]
fn a_key_removed_twice_by_one_block_panics() {
    let mut store = open(&DiskEngine::with_fanout(SimFs::new(), 2));
    let mut changes = store.changes(block_ref(1));
    changes.map(SCANNED).remove(&scanned_key(1, 0));
    changes.map(SCANNED).remove(&scanned_key(1, 0));
    store.apply(changes);
}

#[test]
#[should_panic(expected = "scanned: a map key inserted and removed by one block")]
fn a_key_inserted_and_removed_by_one_block_panics() {
    let mut store = open(&DiskEngine::with_fanout(SimFs::new(), 2));
    let mut changes = store.changes(block_ref(1));
    changes.map(SCANNED).insert(&scanned_key(1, 0), &[0; 8]);
    changes.map(SCANNED).remove(&scanned_key(1, 0));
    store.apply(changes);
}

#[test]
#[should_panic(expected = "probed: a map key held twice")]
fn a_key_the_buffer_holds_inserted_again_panics() {
    let mut store = open(&DiskEngine::with_fanout(SimFs::new(), 2));
    let mut first = store.changes(block_ref(1));
    first.map(PROBED).insert(&probed_key(0), &[0; 4]);
    store.apply(first);
    let mut again = store.changes(block_ref(2));
    again.map(PROBED).insert(&probed_key(0), &[1; 4]);
    store.apply(again);
}

#[test]
#[should_panic(expected = "scanned: a map key removed twice")]
fn a_key_the_buffer_removed_removed_again_panics() {
    let mut store = open(&DiskEngine::with_fanout(SimFs::new(), 2));
    let mut first = store.changes(block_ref(1));
    first.map(SCANNED).remove(&scanned_key(1, 0));
    store.apply(first);
    let mut again = store.changes(block_ref(2));
    again.map(SCANNED).remove(&scanned_key(1, 0));
    store.apply(again);
}

#[test]
#[should_panic(expected = "probed: remove on a table without `deletes()`")]
fn a_removal_from_a_table_without_deletes_panics() {
    BlockChanges::new(block_ref(1), SCHEMA).map(PROBED).remove(&probed_key(0));
}

// same key inserted by two commits: neither layer above durable can see the first

#[test]
#[should_panic(expected = "a key held 2 times across committed segments")]
fn an_insert_only_key_in_two_committed_segments_panics_on_read() {
    let mut store = open(&DiskEngine::with_fanout(SimFs::new(), 2));
    for n in [1, 2] {
        let mut changes = store.changes(block_ref(n));
        changes.map(PROBED).insert(&probed_key(0), &[0; 4]);
        store.apply(changes);
        store.commit().expect("commit");
    }
    store.committed().map(PROBED).range(&[0; 16], &[0xff; 16], usize::MAX);
}

#[test]
#[should_panic(expected = "a key held 2 times across committed segments")]
fn a_deletes_key_held_in_two_committed_segments_panics_on_read() {
    let mut store = open(&DiskEngine::with_fanout(SimFs::new(), 2));
    for n in [1, 2] {
        let mut changes = store.changes(block_ref(n));
        changes.map(SCANNED).insert(&scanned_key(1, 0), &[0; 8]);
        store.apply(changes);
        store.commit().expect("commit");
    }
    store.committed().map(SCANNED).range(&[0; 12], &[0xff; 12], usize::MAX);
}

/// Two 1-row segments at fanout 2 = a merge; its duplicate = the merge's `Contract` error, surfaced
/// at the next commit (either table kind)
#[test]
fn a_merge_of_a_key_held_twice_fails_the_next_commit() {
    for table in [PROBED, SCANNED] {
        let mut store = open(&DiskEngine::with_fanout(SimFs::new(), 2));
        let key = if table == PROBED { probed_key(0) } else { scanned_key(1, 0) };
        let value = vec![0; if table == PROBED { 4 } else { 8 }];
        for n in [1, 2] {
            let mut changes = store.changes(block_ref(n));
            changes.map(table).insert(&key, &value);
            store.apply(changes);
            store.commit().expect("commit");
        }
        store.settle();
        store.apply(store.changes(block_ref(3)));

        let refused = store.commit().expect_err("a merge of two values of one key").to_string();

        assert!(refused.contains("key held 2 times: value, value"), "{}: {refused}", table.name);
    }
}

#[test]
#[should_panic(expected = "commit after a failed one")]
fn a_commit_after_a_failed_one_panics() {
    let fs = SimFs::new();
    let mut store = open(&DiskEngine::with_fanout(fs.clone(), 2));
    fs.fail_from(fs.mutations());
    let mut changes = store.changes(block_ref(1));
    changes.sequence(BLOCKS).append(&block(1));
    store.apply(changes);
    store.commit().expect_err("injected EIO");
    let _ = store.commit();
}

/// - open drops bytes past the manifest
/// - refused: lost file, torn tail page, data in a directory that never committed
#[test]
fn open_trims_to_the_manifest_and_refuses_lost_torn_or_unmanifested_data() {
    let populated = || {
        let fs = SimFs::new();
        let mut store = open(&DiskEngine::new(fs.clone(), LsmConfig::default()));
        store.apply(Model::default().advance(4, &[1, 2], 3, &[]));
        store.commit().expect("commit");
        fs
    };
    let path = |name: &str| Path::new(ROOT).join(name);
    let blocks_len = |fs: &SimFs| fs.contents(&path("blocks.dat")).expect("blocks").len();

    let fs = populated();
    let committed = (0..4).map(|n| block(n).len()).sum::<usize>();
    fs.corrupt(&path("blocks.dat"), |bytes| bytes.extend_from_slice(&[0xa5; 64]));
    let store = open(&DiskEngine::new(fs.clone(), LsmConfig::default()));
    assert_eq!(blocks_len(&fs), committed, "uncommitted tail truncated");
    let records = store.committed().sequence(BLOCKS).records(0..4);
    assert_eq!(records, (0..4).map(block).collect::<Vec<_>>());
    drop(store);

    let refused = |edit: &dyn Fn(&SimFs)| {
        let fs = populated();
        edit(&fs);
        DiskEngine::new(fs, LsmConfig::default())
            .open(Path::new(ROOT), &SCHEMA, NonZeroUsize::MAX)
            .expect_err("refused")
            .to_string()
    };
    let short = refused(&|fs| fs.corrupt(&path("pool/nodes.dat"), |bytes| bytes.truncate(10)));
    assert_eq!(short, "/idx/pool/nodes.dat is 10 bytes, the committed state needs 32");
    let torn = refused(&|fs| fs.corrupt(&path("blocks.idx"), |bytes| bytes[3] ^= 1));
    assert_eq!(torn, "/idx/blocks.idx: tail page fails its checksum");

    let bare = SimFs::new();
    bare.create_dir_all(Path::new(ROOT)).expect("dir");
    bare.open(&path("heights.dat")).expect("file").file.write_all_at(&[1], 0).expect("write");
    let opened = DiskEngine::new(bare, LsmConfig::default()).open(
        Path::new(ROOT),
        &SCHEMA,
        NonZeroUsize::MAX,
    );
    assert!(matches!(opened, Err(StoreError::Manifest(ManifestError::Unmanifested { .. }))));
}

/// Manifest body pinned byte for byte: committed tip, each sequence's seals in schema order (two
/// for a variable one), each map's segment list
#[test]
fn a_manifest_body_is_its_golden_bytes() {
    const FIXED: SequenceTable = SequenceTable::new(0, "fixed", Width::fixed(4));
    const VARIABLE: SequenceTable = SequenceTable::new(1, "variable", Width::Variable);
    const MAP: MapTable = MapTable::new(0, "map", Width::fixed(8), Width::fixed(1), 0);
    const TABLES: Tables = Tables::new(&[FIXED, VARIABLE], &[MAP]);
    let schema = Schema::new(IndexKind::CompactBlock, 1, NetworkType::Regtest, TABLES);
    let sealed =
        |n: u8| Sealed { len: u64::from(n), tail: u32::from(n) << 8, sums: u32::from(n) << 16 };
    let body = Body {
        committed: Committed { tip: Some(block_ref(3)) },
        sequences: vec![
            Seals { data: sealed(4), records: Records::Fixed(NonZeroU32::new(4).expect("4")) },
            Seals { data: sealed(5), records: Records::Variable(sealed(8)) },
        ],
        maps: vec![vec![SegmentMeta { id: 7, records: 2, sealed: sealed(9) }]],
    };
    let seal = |n: u8| {
        [
            u64::from(n).to_le_bytes().as_slice(),
            &(u32::from(n) << 8).to_le_bytes(),
            &(u32::from(n) << 16).to_le_bytes(),
        ]
        .concat()
    };
    let golden = [
        &3u64.to_le_bytes()[..],
        &[3; 32],
        &seal(4),
        &seal(5),
        &seal(8),
        &1u32.to_le_bytes(),
        &7u32.to_le_bytes(),
        &2u64.to_le_bytes(),
        &seal(9),
    ]
    .concat();
    assert_eq!(body.encode(), golden);
    assert_eq!(Body::decode(&golden, &schema).expect("decodes"), body);

    let partial = Body::decode(&[&golden[..40], &seal(6)].concat(), &schema);
    assert!(matches!(partial, Err(ManifestError::Body(_))), "6 bytes is not whole 4-byte records");
}

/// `verify` over a real directory: clean, flipped byte names its page, missing file lost, file a
/// merge retired between manifest read and scrub scrubbed again
#[test]
fn verify_names_bad_pages_and_lost_files_and_rescrubs_after_a_merge() {
    let root = tempfile::tempdir().expect("tempdir");
    let path = root.path().join("idx");
    let fs = RealFs::shared();
    let engine = DiskEngine::with_fanout(fs.clone(), 2);
    let mut store = engine.open(&path, &SCHEMA, NonZeroUsize::MAX).expect("open");
    let mut model = Model::default();
    store.apply(model.advance(3, &[1], 1, &[]));
    store.commit().expect("commit");
    store.apply(model.advance(1, &[2], 1, &[]));
    store.commit().expect("commit");
    let read = || dir::read_committed(fs.as_ref(), &path, identity(&SCHEMA)).expect("read");
    let before = read();
    store.settle();
    store.apply(model.advance(0, &[], 0, &[]));
    store.commit().expect("the merge lands, its inputs unlinked");
    let after = read();

    let clean = engine.verify(&path, &SCHEMA).expect("verify");
    assert!(clean.is_clean(), "{clean:?}");
    assert_eq!(clean.heights, 3);
    let names: Vec<&str> = clean.units.iter().map(|unit| unit.name.as_str()).collect();
    assert_eq!(names[..4], ["blocks.dat", "blocks.idx", "heights.dat", "pool/nodes.dat"]);
    assert_eq!(names.len(), 4 + 2, "one merged segment per map");

    let mut reads = [before.clone(), after.clone()].into_iter().chain(std::iter::repeat(after));
    let rescrubbed =
        verify_committed(fs.as_ref(), &path, &SCHEMA, || Ok(reads.next().expect("endless")))
            .expect("verify");
    assert_eq!(rescrubbed, clean, "retired inputs: scrubbed again against the newer manifest");
    let stale =
        verify_committed(fs.as_ref(), &path, &SCHEMA, || Ok(before.clone())).expect("verify");
    assert!(stale.units.iter().any(|unit| unit.lost), "a listed file missing is lost: {stale:?}");

    let heights = path.join("heights.dat");
    let mut bytes = std::fs::read(&heights).expect("read");
    bytes[3] ^= 1;
    std::fs::write(&heights, &bytes).expect("write");
    let corrupt = engine.verify(&path, &SCHEMA).expect("verify");
    let unit = corrupt.units.iter().find(|unit| unit.name == "heights.dat").expect("listed");
    assert_eq!((corrupt.is_clean(), unit.bad_pages.clone()), (false, vec![0]));

    let fresh = engine.verify(&root.path().join("absent"), &SCHEMA).expect("verify");
    assert_eq!(fresh, Verification { heights: 0, units: Vec::new() });
}
