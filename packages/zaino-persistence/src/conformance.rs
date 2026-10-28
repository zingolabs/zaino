//! What every [`Backend`] must answer, through the port alone (feature `testing`)
//!
//! - backend crate implements [`Subject`] once, runs [`history`] under proptest + [`contract`] as
//!   a plain test; `PROPTEST_CASES=1000` = its heavy run
//! - backend driven as a writer drives it (commit, flush), against `Vec` / `BTreeMap` models
//! - storage-specific moves (reopen, power loss, background work, internal invariants) =
//!   [`Subject`] hooks with no-op defaults
//! - [`Model`] doubles as the expected state for a backend's own crash and fault tests
//!
//! Test support: assertions here panic by design.

use std::{
    collections::{BTreeMap, BTreeSet},
    num::{NonZeroU32, NonZeroUsize},
    ops::{Bound, RangeBounds},
};

use proptest::{prelude::*, strategy::Union};
use zaino_primitives::types::{BlockHash, BlockRef, Height};

use crate::{
    raw_entry, sequence_key, Backend, BackendReader, BackendWriter, CommitError, MaintenanceReader,
    Namespace, NamespaceSpec, Part, RawKey, RawValue, ReadError, ScanDirection, ScanRange, Width,
    WriteOp,
};

/// Variable-width sequence (empty records included)
pub const BLOCKS: Namespace = Namespace::new("blocks");
/// Fixed 8-byte sequence
pub const HEIGHTS: Namespace = Namespace::new("heights");
/// Fixed 4-byte sequence, in a sub-directory for engines with them
const NODES: Namespace = Namespace::new("pool/nodes");
/// `account ‖ seq` → 8 bytes, removable, read per account
pub const SCANNED: Namespace = Namespace::new("scanned");
/// Hash-like 16-byte ids → 4 bytes, insert-only point lookups
pub const PROBED: Namespace = Namespace::new("probed");

/// `n` > 0 as a width
const fn width(n: u32) -> NonZeroU32 {
    NonZeroU32::MIN.saturating_add(n - 1)
}

/// Every shape a backend must hold
pub const SPECS: [NamespaceSpec; 5] = [
    NamespaceSpec::sequence(BLOCKS, Width::Variable),
    NamespaceSpec::sequence(HEIGHTS, Width::Fixed(width(8))),
    NamespaceSpec::sequence(NODES, Width::Fixed(width(4))),
    NamespaceSpec::map(SCANNED, width(12), width(8), true),
    NamespaceSpec::map(PROBED, width(16), width(4), false),
];

/// Backend under test, over storage the suite can reopen (and, if it can, crash)
pub trait Subject {
    /// The backend type.
    type Backend: Backend<Reader: MaintenanceReader>;

    /// Backend over the storage as it stands now, declared with [`SPECS`] (reopen = fresh
    /// `open`)
    fn open(&self) -> Self::Backend;

    /// Commits survive a reopen without a flush (nothing buffered)
    fn keeps_unflushed(&self) -> bool {
        false
    }

    /// The backend may write commits before a flush (e.g. once its buffer fills): a reopen or
    /// crash then holds a prefix of the commits, from the last flushed one to the last one
    fn writes_on_its_own(&self) -> bool {
        false
    }

    /// Storage replaced by what a crash right now would leave; `false` = no such image (step
    /// reopens instead)
    fn power_loss(&mut self) -> bool {
        false
    }

    /// Background work finished (merges); optional: a no-op where there is none, or where a
    /// backend can't wait for it through the port (then a pinned reader is checked only against
    /// work that lands at a flush, and the backend tests the rest itself)
    fn settle(&self, _backend: &Self::Backend) {}

    /// Backend-internal invariants after every step (`just_opened` = a fresh open)
    fn check(&self, _backend: &Self::Backend, _just_opened: bool, _label: &str) {}
}

/// Record `n` of `blocks`: 0 to 22 bytes (empty records included)
pub fn block(n: u32) -> Vec<u8> {
    vec![n as u8; n as usize % 23]
}

/// Uniform 8 bytes per account (engines may shard on leading key bytes)
fn account(n: u8) -> [u8; 8] {
    u64::from(n)
        .wrapping_mul(0x9e37_79b9_7f4a_7c15)
        .to_be_bytes()
}

/// `account(owner) ‖ seq`
pub fn scanned_key(owner: u8, seq: u32) -> Vec<u8> {
    [&account(owner)[..], &seq.to_be_bytes()].concat()
}

/// Uniform first 8 bytes, `n` in the last 4 (distinct per `n`)
pub fn probed_key(n: u32) -> Vec<u8> {
    let mut id = vec![0u8; 16];
    id[..8].copy_from_slice(
        &u64::from(n)
            .wrapping_mul(0x9e37_79b9_7f4a_7c15)
            .to_be_bytes(),
    );
    id[12..].copy_from_slice(&n.to_be_bytes());
    id
}

/// Tip of advance `n` (from 1): height `n - 1`, hash `[n; 32]`
pub fn block_ref(n: usize) -> BlockRef {
    salted_ref(n, 0)
}

/// [`block_ref`] on branch `salt` (0 = the first branch)
fn salted_ref(n: usize, salt: u8) -> BlockRef {
    let height = Height::try_from(n as u32 - 1).expect("small height");
    let mut hash = [n as u8; 32];
    hash[0] ^= salt;
    BlockRef {
        hash: BlockHash::from(hash),
        height,
    }
}

/// `bytes` on branch `salt` (a dropped record ≠ its replacement)
fn salted(mut bytes: Vec<u8>, salt: u8) -> Vec<u8> {
    if let Some(first) = bytes.first_mut() {
        *first ^= salt;
    }
    bytes
}

fn put(namespace: Namespace, key: Vec<u8>, value: Vec<u8>) -> WriteOp {
    WriteOp::Put {
        namespace,
        key,
        value,
    }
}

fn delete(namespace: Namespace, key: Vec<u8>) -> WriteOp {
    WriteOp::Delete { namespace, key }
}

/// Every namespace's contents through the last [`advance`](Self::advance)
///
/// - `salt` = branch of the next advance's contents (0 outside [`history`])
#[derive(Debug, Clone, Default)]
pub struct Model {
    advances: usize,
    tip: Option<BlockRef>,
    salt: u8,
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
    /// Next block, applied here, returned as its tip + ops: `records` blocks + twice as many
    /// nodes, one height record, `removals` picks of `scanned` rows held before it (`pick % rows`,
    /// a pick repeated = removed once), one `scanned` row per listed owner (fresh seqs), `ids`
    /// fresh `probed` ids
    pub fn advance(
        &mut self,
        records: u8,
        owners: &[u8],
        ids: u16,
        removals: &[u16],
    ) -> (BlockRef, Vec<WriteOp>) {
        self.advances += 1;
        let tip = salted_ref(self.advances, self.salt);
        self.tip = Some(tip);
        let mut ops = Vec::new();
        let held: Vec<Vec<u8>> = self.scanned.keys().cloned().collect();
        if !held.is_empty() {
            let picked: BTreeSet<&Vec<u8>> = removals
                .iter()
                .map(|pick| &held[usize::from(*pick) % held.len()])
                .collect();
            for key in picked {
                ops.push(delete(SCANNED, key.clone()));
                self.scanned.remove(key);
                self.removed.insert(key.clone());
            }
        }
        for _ in 0..records {
            let record = block(self.blocks.len() as u32);
            self.append(&mut ops, BLOCKS, record);
            for _ in 0..2 {
                let node = (self.nodes.len() as u32).to_le_bytes().to_vec();
                self.append(&mut ops, NODES, node);
            }
        }
        let height = (self.advances as u64).to_be_bytes().to_vec();
        self.append(&mut ops, HEIGHTS, height);
        for &owner in owners {
            self.seq += 1;
            let key = scanned_key(owner, self.seq);
            let value = salted(u64::from(self.seq).to_be_bytes().to_vec(), self.salt);
            ops.push(put(SCANNED, key.clone(), value.clone()));
            self.scanned.insert(key, value);
        }
        for n in self.ids..self.ids + u32::from(ids) {
            let value = salted(n.to_be_bytes().to_vec(), self.salt);
            ops.push(put(PROBED, probed_key(n), value.clone()));
            self.probed.insert(probed_key(n), value);
        }
        self.ids += u32::from(ids);
        (tip, ops)
    }

    /// `record` on this branch, at the end of `namespace` here and in `ops`
    fn append(&mut self, ops: &mut Vec<WriteOp>, namespace: Namespace, record: Vec<u8>) {
        let record = salted(record, self.salt);
        let held = self.sequence_mut(namespace);
        ops.push(put(
            namespace,
            sequence_key(held.len() as u64),
            record.clone(),
        ));
        held.push(record);
    }

    fn sequence_mut(&mut self, namespace: Namespace) -> &mut Vec<Vec<u8>> {
        if namespace == BLOCKS {
            &mut self.blocks
        } else if namespace == HEIGHTS {
            &mut self.heights
        } else {
            &mut self.nodes
        }
    }

    /// `scanned` rows within the bounds, in key order
    fn scan(&self, lower: Bound<&[u8]>, upper: Bound<&[u8]>) -> Vec<(RawKey, RawValue)> {
        let rows: Vec<_> = self.scanned.clone().into_iter().collect();
        within(&rows, (lower, upper))
    }

    /// The next tip, committed with no ops
    fn advance_empty(&mut self) -> BlockRef {
        self.advances += 1;
        let tip = salted_ref(self.advances, self.salt);
        self.tip = Some(tip);
        tip
    }

    /// `owner` + `raw` seq reduced into 0 to `seq + 1` (lands on, between and past issued rows);
    /// `kind` % 3 = unbounded, included, excluded
    fn bound(&self, (owner, raw, kind): (u8, u32, u8)) -> Bound<Vec<u8>> {
        let key = scanned_key(owner, raw % (self.seq + 2));
        match kind % 3 {
            0 => Bound::Unbounded,
            1 => Bound::Included(key),
            _ => Bound::Excluded(key),
        }
    }

    /// `probed` rows, in key order
    fn probed_entries(&self) -> Vec<(RawKey, RawValue)> {
        self.probed.clone().into_iter().collect()
    }

    /// - tip; every sequence record one at a time, whole, as chunked ranges (whole, a middle run)
    /// - `scanned`: full and per-owner scans; sampled values + a miss; every removed key absent
    /// - `probed`: values over every id issued + as many never issued; full scans
    pub fn assert_view(&self, reader: &impl MaintenanceReader, label: &str) {
        assert_eq!(reader.tip().expect("tip"), self.tip, "{label}: tip");
        for (namespace, model) in [
            (BLOCKS, &self.blocks),
            (HEIGHTS, &self.heights),
            (NODES, &self.nodes),
        ] {
            let len = model.len() as u64;
            let one_by_one: Vec<_> = (0..len)
                .map(|at| reader.get(namespace, &sequence_key(at)).expect("get"))
                .collect();
            let expected: Vec<_> = model.iter().cloned().map(Some).collect();
            assert_eq!(one_by_one, expected, "{label}: {namespace} records");
            let past = reader.get(namespace, &sequence_key(len)).expect("get");
            assert_eq!(past, None, "{label}: {namespace} past the end");
            let whole = sequence_entries(model);
            let scanned = reader.scan(namespace).expect("scan");
            assert_eq!(scanned, whole, "{label}: {namespace} scan");
            let label = format!("{label}: {namespace}");
            let unbounded = (Bound::Unbounded, Bound::Unbounded);
            walks(reader, namespace, unbounded, &whole, &[40, 20], &label);
            let middle = len / 3..len - len / 3;
            let bounds = (
                Bound::Included(sequence_key(middle.start)),
                Bound::Excluded(sequence_key(middle.end)),
            );
            walks(reader, namespace, bounds, &whole, &[40], &label);
        }

        let every = self.scan(Bound::Unbounded, Bound::Unbounded);
        assert_eq!(
            reader.scan(SCANNED).expect("scan"),
            every,
            "{label}: full scan"
        );
        for owner in [0u8, 3, 9] {
            let (from, to) = (scanned_key(owner, 0), scanned_key(owner, u32::MAX));
            let bounds = (Bound::Included(from), Bound::Excluded(to));
            walks(
                reader,
                SCANNED,
                bounds,
                &every,
                &[64],
                &format!("{label}: owner {owner}"),
            );
        }
        for (key, value) in self.scanned.iter().step_by(7) {
            let got = reader.get(SCANNED, key).expect("get");
            assert_eq!(got.as_ref(), Some(value), "{label}: {key:?}");
        }
        let miss = reader
            .get(SCANNED, &scanned_key(1, self.seq + 1))
            .expect("get");
        assert_eq!(miss, None, "{label}: miss");
        for key in &self.removed {
            let got = reader.get(SCANNED, key).expect("get");
            assert_eq!(got, None, "{label}: removed {key:?}");
        }

        for n in 0..2 * self.ids + 2 {
            let id = probed_key(n);
            let got = reader.get(PROBED, &id).expect("get");
            assert_eq!(got.as_ref(), self.probed.get(&id), "{label}: probed {n}");
        }
        let probed = self.probed_entries();
        assert_eq!(
            reader.scan(PROBED).expect("scan"),
            probed,
            "{label}: {PROBED} scan"
        );
        let label = format!("{label}: {PROBED}");
        let unbounded = (Bound::Unbounded, Bound::Unbounded);
        walks(reader, PROBED, unbounded, &probed, &[64], &label);
    }

    /// Every namespace scanned with bounds shorter and longer than its keys ([`odd_bounds`])
    fn assert_odd_bounds(&self, reader: &impl BackendReader, label: &str) {
        for (namespace, model) in [
            (BLOCKS, &self.blocks),
            (HEIGHTS, &self.heights),
            (NODES, &self.nodes),
        ] {
            let whole = sequence_entries(model);
            odd_bounds(reader, namespace, &whole, &format!("{label}: {namespace}"));
        }
        let every = self.scan(Bound::Unbounded, Bound::Unbounded);
        odd_bounds(reader, SCANNED, &every, &format!("{label}: {SCANNED}"));
        odd_bounds(
            reader,
            PROBED,
            &self.probed_entries(),
            &format!("{label}: {PROBED}"),
        );
    }

    /// `scan_range` of `scanned` within the bounds, both directions, at `budget` and 1 byte
    fn assert_scans(
        &self,
        reader: &impl BackendReader,
        bounds: (Bound<Vec<u8>>, Bound<Vec<u8>>),
        budget: u16,
        label: &str,
    ) {
        let every = self.scan(Bound::Unbounded, Bound::Unbounded);
        let budgets = [usize::from(budget).max(1), 1];
        walks(reader, SCANNED, bounds, &every, &budgets, label);
    }
}

/// `entries` (ascending) within `bounds`, in key order
fn within(
    entries: &[(RawKey, RawValue)],
    bounds: (Bound<&[u8]>, Bound<&[u8]>),
) -> Vec<(RawKey, RawValue)> {
    let held = entries
        .iter()
        .filter(|(key, _)| bounds.contains(key.as_slice()));
    held.cloned().collect()
}

/// A sequence's records as the backend keys them
fn sequence_entries(records: &[Vec<u8>]) -> Vec<(RawKey, RawValue)> {
    let entries = records.iter().enumerate();
    entries
        .map(|(at, record)| (sequence_key(at as u64), record.clone()))
        .collect()
}

/// Raw bytes of an entry
fn size((key, value): &(RawKey, RawValue)) -> usize {
    key.len() + value.len()
}

/// What a walk's filter keeps
#[derive(Debug, Clone, Copy)]
enum Keep {
    All,
    /// Every other entry it sees, from the first
    EveryOther,
}

/// Every [`walk`] of `namespace` within `bounds` (`all` = the namespace, ascending): both
/// directions, both [`Keep`]s, each budget
fn walks(
    reader: &impl BackendReader,
    namespace: Namespace,
    bounds: (Bound<RawKey>, Bound<RawKey>),
    all: &[(RawKey, RawValue)],
    budgets: &[usize],
    label: &str,
) {
    let (lower, upper) = &bounds;
    let mut entries = within(
        all,
        (
            lower.as_ref().map(Vec::as_slice),
            upper.as_ref().map(Vec::as_slice),
        ),
    );
    for direction in [ScanDirection::Forward, ScanDirection::Reverse] {
        if direction == ScanDirection::Reverse {
            entries.reverse();
        }
        for &budget in budgets {
            for keep in [Keep::All, Keep::EveryOther] {
                let range = ScanRange {
                    lower: lower.clone(),
                    upper: upper.clone(),
                    direction,
                    chunk_bytes: NonZeroUsize::new(budget).expect("budgets are non-zero"),
                };
                let label = format!("{label}: {range:?} {keep:?}");
                walk(reader, namespace, range, &entries, keep, &label);
            }
        }
    }
}

/// Bounds shorter and longer than the key width, around the middle entry of `all` (ascending):
/// a prefix of it, it with a byte appended, a one-byte bound, every bound kind
fn odd_bounds(
    reader: &impl BackendReader,
    namespace: Namespace,
    all: &[(RawKey, RawValue)],
    label: &str,
) {
    let Some((key, _)) = all.get(all.len() / 2) else {
        return;
    };
    let prefix = key[..key.len() - 1].to_vec();
    let longer = [key.clone(), vec![0]].concat();
    let past = [key.clone(), vec![0xff]].concat();
    for bounds in [
        (
            Bound::Included(prefix.clone()),
            Bound::Excluded(longer.clone()),
        ),
        (
            Bound::Excluded(prefix.clone()),
            Bound::Included(longer.clone()),
        ),
        (Bound::Excluded(past), Bound::Unbounded),
        (Bound::Unbounded, Bound::Included(vec![key[0]])),
        (Bound::Included(vec![key[0]]), Bound::Excluded(prefix)),
        (Bound::Excluded(longer), Bound::Excluded(vec![0xff; 2])),
    ] {
        walks(
            reader,
            namespace,
            bounds,
            all,
            &[40],
            &format!("{label}: odd bounds"),
        );
    }
}

/// One `scan_range` of `entries` (all of `range`, in walk order) through a [`Keep`] filter,
/// checking the port's chunk rule:
///
/// - the filter sees every entry exactly once, in walk order
/// - each chunk visits at least one entry, and at most `chunk_bytes` raw bytes unless it visits
///   one
/// - a chunk ends only where the next entry would take the bytes it visited past `chunk_bytes`
/// - each chunk holds exactly the entries it visited that the filter kept (empty if none)
fn walk(
    reader: &impl BackendReader,
    namespace: Namespace,
    range: ScanRange,
    entries: &[(RawKey, RawValue)],
    keep: Keep,
    label: &str,
) {
    let budget = range.chunk_bytes.get();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let filter = {
        let seen = std::sync::Arc::clone(&seen);
        move |key: &[u8], value: &[u8]| {
            let mut seen = seen.lock().expect("filter log");
            let kept = match keep {
                Keep::All => true,
                Keep::EveryOther => seen.len() % 2 == 0,
            };
            seen.push(key.to_vec());
            kept.then(|| (key.to_vec(), value.to_vec()))
        }
    };
    let mut chunks = Vec::new();
    let mut seen_by = Vec::new();
    for chunk in reader.scan_range(namespace, range, filter) {
        chunks.push(chunk.expect("scan_range"));
        seen_by.push(seen.lock().expect("filter log").len());
    }
    let seen = seen.lock().expect("filter log").clone();
    let keys: Vec<&RawKey> = entries.iter().map(|(key, _)| key).collect();
    assert_eq!(
        seen.iter().collect::<Vec<_>>(),
        keys,
        "{label}: the filter sees every entry once, in walk order"
    );
    let mut start = 0;
    for (at, (chunk, &end)) in chunks.iter().zip(&seen_by).enumerate() {
        let visited = &entries[start..end];
        assert!(!visited.is_empty(), "{label}: chunk {at} visited nothing");
        let bytes: usize = visited.iter().map(size).sum();
        assert!(
            visited.len() == 1 || bytes <= budget,
            "{label}: chunk {at} visited {bytes} bytes, over its {budget}-byte budget"
        );
        if at + 1 < chunks.len() {
            assert!(
                bytes + size(&entries[end]) > budget,
                "{label}: chunk {at} ended after {bytes} bytes, before an entry that fit {budget}"
            );
        }
        let kept: Vec<_> = (start..end)
            .filter(|index| matches!(keep, Keep::All) || index % 2 == 0)
            .map(|index| entries[index].clone())
            .collect();
        assert_eq!(chunk, &kept, "{label}: chunk {at} = its kept entries");
        start = end;
    }
}

/// Commit `ops` at `tip` through a fresh writer handle
fn commit<B: Backend>(backend: &B, tip: BlockRef, ops: Vec<WriteOp>) -> Result<(), CommitError> {
    backend.writer().expect("writer").commit(tip, ops)
}

/// A block above durable: its tip, ops and contents through it
struct Node {
    tip: BlockRef,
    ops: Vec<WriteOp>,
    model: Model,
}

/// Oracle for [`history`]: `nodes` = blocks above `durable`, oldest first, the first `applied`
/// committed to the backend (visible, not yet flushed); `salt` = branch of the next node
#[derive(Default)]
struct Oracle {
    durable: Model,
    nodes: Vec<Node>,
    applied: usize,
    salt: u8,
}

impl Oracle {
    /// Contents through the last applied node (what readers see)
    fn visible(&self) -> &Model {
        self.applied
            .checked_sub(1)
            .map_or(&self.durable, |at| &self.nodes[at].model)
    }

    /// Contents through the newest node
    fn newest(&self) -> &Model {
        self.nodes.last().map_or(&self.durable, |node| &node.model)
    }

    /// Node on the current branch above the newest
    fn grow(&mut self, (records, owners, ids, removals): (u8, &[u8], u16, &[u16])) {
        let mut model = self.newest().clone();
        model.salt = self.salt;
        let (tip, ops) = model.advance(records, owners, ids, removals);
        self.nodes.push(Node { tip, ops, model });
    }

    /// Everything applied is now durable
    fn flushed(&mut self) {
        self.durable = self.visible().clone();
        self.nodes.drain(..self.applied);
        self.applied = 0;
    }

    /// Reopened a backend that writes on its own: the applied prefix it holds (found by its tip)
    /// is now durable, the rest unapplied
    fn recovered(&mut self, tip: Option<BlockRef>, label: &str) {
        let held = (0..=self.applied).rev().find(|&count| {
            let model = count
                .checked_sub(1)
                .map_or(&self.durable, |at| &self.nodes[at].model);
            model.tip == tip
        });
        self.applied =
            held.unwrap_or_else(|| panic!("{label}: tip {tip:?} ends no prefix of the commits"));
        self.flushed();
    }
}

/// - `Grow`: node above the newest (`records` blocks + nodes, one height, `removals` of held
///   scanned rows, `owners` scanned rows, `ids` probed ids)
/// - `Apply`: oldest unapplied nodes committed, `count` reduced at run time
/// - `Flush`: every committed node durable
/// - `Reorg`: unapplied nodes cut to `keep` (reduced), later nodes on a new branch
/// - `Settle`: background work finishes; `Reopen`: exit mid-work; `PowerLoss`: crash now (both:
///   unflushed commits lost unless the subject keeps them; nodes kept and re-applied later)
/// - `Pin`: a reader + the model it must keep answering, re-checked every later step
/// - `Scan`: raw bounds reduced at run time, both directions, `budget` and 1-byte chunks; then
///   every namespace with bounds shorter and longer than its keys
#[derive(Debug, Clone)]
pub enum Step {
    /// Grow a node.
    Grow {
        /// Block records.
        records: u8,
        /// Scanned rows' owners.
        owners: Vec<u8>,
        /// Probed ids.
        ids: u16,
        /// Scanned rows removed.
        removals: Vec<u16>,
    },
    /// Commit nodes.
    Apply {
        /// Nodes, reduced.
        count: u8,
    },
    /// Flush.
    Flush,
    /// Cut unapplied nodes.
    Reorg {
        /// Nodes kept, reduced.
        keep: u8,
    },
    /// Background work finishes.
    Settle,
    /// Close and reopen.
    Reopen,
    /// Crash and reopen.
    PowerLoss,
    /// Hold a reader.
    Pin,
    /// Range scan of `scanned`.
    Scan {
        /// Lower bound: owner, raw seq, kind.
        from: (u8, u32, u8),
        /// Upper bound: owner, raw seq, kind.
        to: (u8, u32, u8),
        /// Chunk budget.
        budget: u16,
    },
}

/// Swarm testing (TigerBeetle `tree_fuzz`): whole step kinds off per case (a uniform mix dilutes
/// rare interleavings: all reopens, no settles, only small blocks, no reorgs, …)
pub fn steps() -> impl Strategy<Value = Vec<Step>> {
    let on = prop::array::uniform8(prop::bool::ANY);
    on.prop_flat_map(
        |[settle, reopen, power_loss, pin, scan, large, reorg, removing]| {
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
            let bound = (0u8..6, any::<u32>(), any::<u8>());
            let query =
                (bound.clone(), bound, any::<u16>()).prop_map(|(from, to, budget)| Step::Scan {
                    from,
                    to,
                    budget: budget % 512,
                });
            let mut kinds = vec![
                (4, grow),
                (
                    3,
                    any::<u8>().prop_map(|count| Step::Apply { count }).boxed(),
                ),
                (2, Just(Step::Flush).boxed()),
            ];
            for (enabled, weight, kind) in [
                (
                    reorg,
                    1,
                    any::<u8>().prop_map(|keep| Step::Reorg { keep }).boxed(),
                ),
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
        },
    )
}

/// History against the subject + `Oracle`, after every step:
///
/// - a new reader sees every committed node; a flush makes them durable
/// - a pinned reader keeps its snapshot through commits, flushes, background work and reopens
/// - reopen or crash = exactly the durable nodes (plus the committed ones, for a subject that
///   keeps unflushed commits; a prefix of the committed ones, for one that writes on its own);
///   [`Subject::check`] holds
pub fn history<S: Subject>(mut subject: S, steps: &[Step]) {
    let mut backend = subject.open();
    let mut oracle = Oracle::default();
    let mut pinned: Option<(usize, Model, <S::Backend as Backend>::Reader)> = None;

    for (at, step) in steps.iter().enumerate() {
        let label = format!("step {at} {step:?}");
        let mut reopened = false;
        match step {
            Step::Grow {
                records,
                owners,
                ids,
                removals,
            } => oracle.grow((*records, owners, *ids, removals)),
            Step::Apply { count } => {
                let unapplied = oracle.nodes.len() - oracle.applied;
                let count = usize::from(*count) % (unapplied + 1);
                for node in &oracle.nodes[oracle.applied..oracle.applied + count] {
                    commit(&backend, node.tip, node.ops.clone()).expect("commit");
                }
                oracle.applied += count;
            }
            Step::Flush => {
                backend.flush().expect("flush");
                oracle.flushed();
            }
            Step::Reorg { keep } => {
                let unapplied = oracle.nodes.len() - oracle.applied;
                oracle
                    .nodes
                    .truncate(oracle.applied + usize::from(*keep) % (unapplied + 1));
                oracle.salt = oracle.salt.wrapping_add(1);
            }
            Step::Settle => subject.settle(&backend),
            // crash image taken while background work may still be writing: flushed = durable
            Step::Reopen | Step::PowerLoss => {
                let crashed = matches!(step, Step::PowerLoss) && subject.power_loss();
                drop(backend);
                backend = subject.open();
                if !crashed && subject.keeps_unflushed() {
                    oracle.flushed();
                } else if subject.writes_on_its_own() {
                    let tip = backend.reader().expect("reader").tip().expect("tip");
                    oracle.recovered(tip, &label);
                } else {
                    oracle.applied = 0;
                }
                reopened = true;
            }
            Step::Pin => {
                let reader = backend.reader().expect("reader");
                pinned = Some((at, oracle.visible().clone(), reader));
            }
            Step::Scan { from, to, budget } => {
                let visible = oracle.visible();
                let bounds = (visible.bound(*from), visible.bound(*to));
                let reader = backend.reader().expect("reader");
                visible.assert_scans(&reader, bounds, *budget, &label);
                visible.assert_odd_bounds(&reader, &label);
            }
        }
        subject.check(&backend, reopened, &label);
        let reader = backend.reader().expect("reader");
        oracle.visible().assert_view(&reader, &label);
        if let Some((pinned_at, model, reader)) = &pinned {
            model.assert_view(reader, &format!("{label}: pinned at step {pinned_at}"));
        }
    }
}

/// A misuse case: what it is, its tip and ops, and the refusal it must get
type Refusal = (
    &'static str,
    BlockRef,
    Vec<WriteOp>,
    Box<dyn Fn(&CommitError) -> bool>,
);

/// `result` = the refusal `expected` names, with nothing applied (checked by the caller's view)
fn refused(what: &str, result: Result<(), CommitError>, expected: impl Fn(&CommitError) -> bool) {
    match result {
        Err(error) => assert!(expected(&error), "{what}: refused with {error:?}"),
        Ok(()) => panic!("{what}: committed instead of refused"),
    }
}

/// Port's fixed promises, on a fresh subject:
///
/// - empty open: no tip, every namespace empty, verify clean, every namespace in the footprint
/// - a commit is visible at once; flushed = survives a reopen; unflushed = survives only on a
///   subject that keeps unflushed commits
/// - every rule violation refused with its error, nothing applied, commits continuing after
pub fn contract<S: Subject>(subject: S) {
    let mut model = Model::default();
    let backend = subject.open();
    let reader = || backend.reader().expect("reader");
    model.assert_view(&reader(), "fresh");
    for (namespace, key) in [
        (BLOCKS, vec![0; 4]),
        (BLOCKS, vec![0; 9]),
        (PROBED, vec![0; 15]),
        (PROBED, vec![0; 17]),
    ] {
        assert!(
            matches!(reader().get(namespace, &key), Err(ReadError::WidthMismatch { actual, .. }) if actual == key.len()),
            "{namespace}: a {}-byte point read refused",
            key.len()
        );
    }
    let undeclared = Namespace::new("undeclared");
    let not_found = |error: &ReadError| matches!(error, ReadError::NamespaceNotFound(name) if name == undeclared.as_str());
    let get = reader().get(undeclared, &[0; 8]);
    assert!(
        get.as_ref().is_err_and(not_found),
        "undeclared get: {get:?}"
    );
    let scan = reader().scan(undeclared);
    assert!(
        scan.as_ref().is_err_and(not_found),
        "undeclared scan: {scan:?}"
    );
    for (lower, upper) in [
        (Bound::Unbounded, Bound::Unbounded),
        (Bound::Included(vec![1]), Bound::Excluded(vec![1])),
        (Bound::Included(vec![2]), Bound::Included(vec![1])),
    ] {
        let range = ScanRange {
            lower,
            upper,
            direction: ScanDirection::Forward,
            chunk_bytes: NonZeroUsize::MIN,
        };
        let label = format!("undeclared scan_range {range:?}");
        let mut chunks = reader().scan_range(undeclared, range, raw_entry);
        let first = chunks.next().map(|chunk| chunk.map(drop));
        assert!(
            first
                .as_ref()
                .is_some_and(|chunk| chunk.as_ref().is_err_and(not_found)),
            "{label}: NamespaceNotFound whatever the range, got {first:?}"
        );
        assert!(chunks.next().is_none(), "{label}: finished after the error");
    }
    assert!(
        backend.verify().expect("verify").is_clean(),
        "fresh: verify clean"
    );
    let footprint = backend.footprint().expect("footprint");
    let namespaces: Vec<Namespace> = footprint.iter().map(|ns| ns.namespace).collect();
    let declared: Vec<Namespace> = SPECS.iter().map(|spec| spec.namespace).collect();
    assert_eq!(
        namespaces, declared,
        "footprint: every namespace, declared order"
    );
    assert!(
        footprint.iter().all(|namespace| namespace.bytes == 0),
        "fresh: nothing stored {footprint:?}"
    );

    let fresh = reader();
    let (tip, ops) = model.advance(2, &[1, 2], 2, &[]);
    commit(&backend, tip, ops).expect("commit");
    let first = model.clone();
    let pinned = reader();
    let range = ScanRange {
        lower: Bound::Unbounded,
        upper: Bound::Unbounded,
        direction: ScanDirection::Forward,
        chunk_bytes: NonZeroUsize::MIN,
    };
    let mut chunks = pinned.scan_range(BLOCKS, range, raw_entry);
    let started = chunks.next().expect("blocks held").expect("chunk");
    // removes a row committed in the same buffer
    let (tip, ops) = model.advance(0, &[], 1, &[0]);
    commit(&backend, tip, ops).expect("commit");
    model.assert_view(&reader(), "committed: visible to a new reader before flush");
    Model::default().assert_view(&fresh, "a reader opened before: its snapshot");
    first.assert_view(&pinned, "a reader opened between: its snapshot");
    // the chunks outlive the handle that started them
    drop(pinned);
    let rest: Vec<_> = chunks.collect::<Result<_, _>>().expect("chunks");
    let whole = [vec![started], rest].concat().concat();
    assert_eq!(
        whole,
        sequence_entries(&first.blocks),
        "chunks: the reader's snapshot"
    );
    backend.flush().expect("flush");
    model.assert_view(&reader(), "flushed");
    model.assert_odd_bounds(&reader(), "flushed");
    let footprint = backend.footprint().expect("footprint");
    assert!(
        footprint.iter().all(|namespace| namespace.bytes > 0),
        "flushed: every namespace holds data {footprint:?}"
    );
    backend.flush().expect("an empty flush");

    // a scan started before a commit, a flush, background work and a reopen: its snapshot
    let scanned = model.clone();
    let range = ScanRange {
        lower: Bound::Unbounded,
        upper: Bound::Unbounded,
        direction: ScanDirection::Reverse,
        chunk_bytes: NonZeroUsize::MIN,
    };
    let mut chunks = reader().scan_range(SCANNED, range, raw_entry);
    let started = chunks.next().expect("scanned rows held").expect("chunk");
    let (tip, ops) = model.advance(1, &[4, 5], 1, &[0]);
    commit(&backend, tip, ops).expect("commit");
    backend.flush().expect("flush");
    subject.settle(&backend);
    drop(backend);

    let backend = subject.open();
    let reader = || backend.reader().expect("reader");
    model.assert_view(&reader(), "reopened");
    assert!(
        backend.verify().expect("verify").is_clean(),
        "reopened: verify clean"
    );
    let rest: Vec<_> = chunks.collect::<Result<_, _>>().expect("chunks");
    let mut expected = scanned.scan(Bound::Unbounded, Bound::Unbounded);
    expected.reverse();
    assert_eq!(
        [vec![started], rest].concat().concat(),
        expected,
        "a scan across a commit, flush, settle and reopen: its snapshot"
    );

    let flushed = model.clone();
    let (tip, ops) = model.advance(1, &[3], 1, &[0]);
    commit(&backend, tip, ops.clone()).expect("commit");
    drop(backend);
    let backend = subject.open();
    let reader = || backend.reader().expect("reader");
    let reopened_tip = reader().tip().expect("tip");
    match subject.keeps_unflushed() || (subject.writes_on_its_own() && reopened_tip == Some(tip)) {
        true => model.assert_view(&reader(), "reopened: unflushed kept"),
        false => {
            flushed.assert_view(&reader(), "reopened: unflushed lost");
            commit(&backend, tip, ops).expect("recommit");
        }
    }
    model.assert_view(&reader(), "recommitted");

    // an empty batch commits its tip alone
    let tip = model.advance_empty();
    commit(&backend, tip, Vec::new()).expect("an empty batch");
    model.assert_view(&reader(), "an empty batch: its tip, nothing else");

    // each violation: refused, nothing applied, the next commit fine
    let above = block_ref(model.advances + 1);
    let next = || above;
    let held = model
        .scanned
        .keys()
        .next()
        .cloned()
        .expect("a scanned row held");
    let removed = model
        .removed
        .iter()
        .next()
        .cloned()
        .expect("a scanned row removed");
    let fresh = scanned_key(7, model.seq + 1);
    let (blocks, heights) = (model.blocks.len() as u64, model.heights.len() as u64);
    let undeclared = Namespace::new("undeclared");
    let ok_append = || put(HEIGHTS, sequence_key(heights), vec![0; 8]);
    let ns = |name: &str, namespace: Namespace| name == namespace.as_str();
    let cases: Vec<Refusal> = vec![
        (
            "a tip at the last committed",
            block_ref(model.advances),
            vec![ok_append()],
            Box::new(|e| matches!(e, CommitError::TipNotAbove { .. })),
        ),
        (
            "a tip below the last committed",
            block_ref(model.advances - 1),
            vec![ok_append()],
            Box::new(|e| matches!(e, CommitError::TipNotAbove { .. })),
        ),
        (
            "an undeclared namespace",
            next(),
            vec![ok_append(), put(undeclared, vec![0], vec![0])],
            Box::new(move |e| matches!(e, CommitError::NamespaceNotFound(n) if ns(n, undeclared))),
        ),
        (
            "an append past the next position",
            next(),
            vec![put(BLOCKS, sequence_key(blocks + 1), vec![1])],
            Box::new(
                move |e| matches!(e, CommitError::NotNextPosition { expected, got: Some(got), .. } if *expected == blocks && *got == blocks + 1),
            ),
        ),
        (
            "an append at a held position",
            next(),
            vec![put(BLOCKS, sequence_key(0), vec![1])],
            Box::new(
                move |e| matches!(e, CommitError::NotNextPosition { namespace, .. } if ns(namespace, BLOCKS)),
            ),
        ),
        (
            "one position appended twice in one batch",
            next(),
            vec![ok_append(), ok_append()],
            Box::new(
                move |e| matches!(e, CommitError::NotNextPosition { expected, got: Some(got), .. } if *expected == heights + 1 && *got == heights),
            ),
        ),
        (
            "a sequence key of the wrong length",
            next(),
            vec![put(BLOCKS, vec![0; 4], vec![1])],
            Box::new(|e| matches!(e, CommitError::NotNextPosition { got: None, .. })),
        ),
        (
            "a record of the wrong width",
            next(),
            vec![put(HEIGHTS, sequence_key(heights), vec![0; 7])],
            Box::new(|e| {
                matches!(
                    e,
                    CommitError::WidthMismatch {
                        part: Part::Record,
                        actual: 7,
                        ..
                    }
                )
            }),
        ),
        (
            "a map key of the wrong width",
            next(),
            vec![ok_append(), put(PROBED, vec![0; 15], vec![0; 4])],
            Box::new(|e| {
                matches!(
                    e,
                    CommitError::WidthMismatch {
                        part: Part::Key,
                        actual: 15,
                        ..
                    }
                )
            }),
        ),
        (
            "a map value of the wrong width",
            next(),
            vec![ok_append(), put(PROBED, probed_key(9_999), vec![0; 5])],
            Box::new(|e| {
                matches!(
                    e,
                    CommitError::WidthMismatch {
                        part: Part::Value,
                        actual: 5,
                        ..
                    }
                )
            }),
        ),
        (
            "a delete key of the wrong width",
            next(),
            vec![ok_append(), delete(SCANNED, vec![0; 11])],
            Box::new(move |e| {
                matches!(
                    e,
                    CommitError::WidthMismatch {
                        namespace,
                        part: Part::Key,
                        actual: 11,
                        ..
                    } if ns(namespace, SCANNED)
                )
            }),
        ),
        (
            "a delete in a sequence",
            next(),
            vec![delete(BLOCKS, sequence_key(0))],
            Box::new(
                move |e| matches!(e, CommitError::DeleteNotAllowed { namespace } if ns(namespace, BLOCKS)),
            ),
        ),
        (
            "a delete in a map without deletes",
            next(),
            vec![delete(PROBED, probed_key(0))],
            Box::new(
                move |e| matches!(e, CommitError::DeleteNotAllowed { namespace } if ns(namespace, PROBED)),
            ),
        ),
        (
            "a key inserted twice in one batch",
            next(),
            vec![
                put(PROBED, probed_key(9_999), vec![0; 4]),
                put(PROBED, probed_key(9_999), vec![1; 4]),
            ],
            Box::new(
                move |e| matches!(e, CommitError::DuplicateInBatch { namespace } if ns(namespace, PROBED)),
            ),
        ),
        (
            "a key inserted and removed in one batch",
            next(),
            vec![
                put(SCANNED, fresh.clone(), vec![0; 8]),
                delete(SCANNED, fresh.clone()),
            ],
            Box::new(
                move |e| matches!(e, CommitError::DuplicateInBatch { namespace } if ns(namespace, SCANNED)),
            ),
        ),
        (
            "a held key removed and re-inserted in one batch",
            next(),
            vec![
                delete(SCANNED, held.clone()),
                put(SCANNED, held.clone(), vec![0; 8]),
            ],
            Box::new(
                move |e| matches!(e, CommitError::DuplicateInBatch { namespace } if ns(namespace, SCANNED)),
            ),
        ),
        (
            "a key already present, after a valid append",
            next(),
            vec![ok_append(), put(PROBED, probed_key(0), vec![0; 4])],
            Box::new(
                move |e| matches!(e, CommitError::KeyExists { namespace } if ns(namespace, PROBED)),
            ),
        ),
        (
            "a delete of an absent key",
            next(),
            vec![ok_append(), delete(SCANNED, fresh.clone())],
            Box::new(
                move |e| matches!(e, CommitError::KeyAbsent { namespace } if ns(namespace, SCANNED)),
            ),
        ),
        (
            "a re-insert of a removed key",
            next(),
            vec![ok_append(), put(SCANNED, removed.clone(), vec![0; 8])],
            Box::new(
                move |e| matches!(e, CommitError::KeyRemoved { namespace } if ns(namespace, SCANNED)),
            ),
        ),
        (
            "a second delete of a removed key",
            next(),
            vec![ok_append(), delete(SCANNED, removed.clone())],
            Box::new(
                move |e| matches!(e, CommitError::KeyRemoved { namespace } if ns(namespace, SCANNED)),
            ),
        ),
    ];
    for (what, tip, ops, expected) in cases {
        refused(what, commit(&backend, tip, ops), expected);
        model.assert_view(&reader(), &format!("after {what}: nothing applied"));
    }

    // removed while buffered, then re-inserted: refused before and after the flush
    let before: BTreeSet<Vec<u8>> = model.removed.clone();
    let (tip, ops) = model.advance(1, &[], 0, &[0]);
    commit(&backend, tip, ops).expect("commit after refusals");
    let removed = model.removed.difference(&before).next().cloned();
    let removed = removed.expect("the advance removed a row");
    let reinsert = || vec![put(SCANNED, removed.clone(), vec![0; 8])];
    let is_removed = |e: &CommitError| matches!(e, CommitError::KeyRemoved { .. });
    let above = block_ref(model.advances + 1);
    refused(
        "a re-insert of a buffered removal",
        commit(&backend, above, reinsert()),
        is_removed,
    );
    backend.flush().expect("flush");
    refused(
        "a re-insert of a flushed removal",
        commit(&backend, above, reinsert()),
        is_removed,
    );
    model.assert_view(&reader(), "commits continue after refusals");

    // a tip is checked by height only: a higher one with any hash commits
    let (tip, ops) = model.advance(1, &[], 0, &[]);
    let rehashed = BlockRef {
        hash: BlockHash::from([0xee; 32]),
        ..tip
    };
    commit(&backend, rehashed, ops).expect("a higher tip, another hash");
    model.tip = Some(rehashed);
    model.assert_view(&reader(), "a higher tip with another hash");

    // tips need not be consecutive
    let jump = block_ref(model.advances + 5);
    commit(&backend, jump, Vec::new()).expect("a tip several heights above");
    model.tip = Some(jump);
    model.assert_view(&reader(), "a tip several heights above");
}
