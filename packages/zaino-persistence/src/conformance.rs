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
    ops::Bound,
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
pub const NODES: Namespace = Namespace::new("pool/nodes");
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
    /// `open`; commits are written only by `flush`)
    fn open(&self) -> Self::Backend;

    /// Commits survive a reopen without a flush (nothing buffered)
    fn keeps_unflushed(&self) -> bool {
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
        let within = |key: &[u8]| {
            let above = match lower {
                Bound::Included(lo) => key >= lo,
                Bound::Excluded(lo) => key > lo,
                Bound::Unbounded => true,
            };
            let below = match upper {
                Bound::Included(hi) => key <= hi,
                Bound::Excluded(hi) => key < hi,
                Bound::Unbounded => true,
            };
            above && below
        };
        let rows = self.scanned.iter().filter(|(key, _)| within(key));
        rows.map(|(key, value)| (key.clone(), value.clone()))
            .collect()
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

    /// - tip; every sequence record one at a time, whole, and as a forward + reverse chunked range
    /// - full + per-owner scans; sampled values + a miss; every removed key absent
    /// - values over every id issued + as many never issued
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
            for direction in [ScanDirection::Forward, ScanDirection::Reverse] {
                let got = chunked(
                    reader,
                    namespace,
                    Bound::Unbounded,
                    Bound::Unbounded,
                    direction,
                    40,
                );
                assert_eq!(
                    got,
                    ordered(&whole, direction),
                    "{label}: {namespace} {direction:?}"
                );
                filtered(
                    reader,
                    namespace,
                    (Bound::Unbounded, Bound::Unbounded),
                    direction,
                    40,
                    &ordered(&whole, direction),
                    &format!("{label}: {namespace} {direction:?} filtered"),
                );
            }
            let middle = len / 3..len - len / 3;
            let (lower, upper) = (
                Bound::Included(sequence_key(middle.start)),
                Bound::Excluded(sequence_key(middle.end)),
            );
            let expected = &whole[middle.start as usize..middle.end as usize];
            let got = chunked(reader, namespace, lower, upper, ScanDirection::Forward, 40);
            assert_eq!(got, expected, "{label}: {namespace} middle range");
        }

        let every = self.scan(Bound::Unbounded, Bound::Unbounded);
        assert_eq!(
            reader.scan(SCANNED).expect("scan"),
            every,
            "{label}: full scan"
        );
        for owner in [0u8, 3, 9] {
            let (from, to) = (scanned_key(owner, 0), scanned_key(owner, u32::MAX));
            let expected = self.scan(Bound::Included(&from[..]), Bound::Excluded(&to[..]));
            let lower = Bound::Included(from.clone());
            let upper = Bound::Excluded(to.clone());
            let got = chunked(reader, SCANNED, lower, upper, ScanDirection::Forward, 64);
            assert_eq!(got, expected, "{label}: owner {owner}");
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
    }

    /// `scan_range` of `scanned` within the bounds, both directions, at `budget` and 1 byte
    fn assert_scans(
        &self,
        reader: &impl BackendReader,
        (lower, upper): (Bound<Vec<u8>>, Bound<Vec<u8>>),
        budget: u16,
        label: &str,
    ) {
        let expected = self.scan(
            lower.as_ref().map(Vec::as_slice),
            upper.as_ref().map(Vec::as_slice),
        );
        for direction in [ScanDirection::Forward, ScanDirection::Reverse] {
            for budget in [usize::from(budget).max(1), 1] {
                let got = chunked(
                    reader,
                    SCANNED,
                    lower.clone(),
                    upper.clone(),
                    direction,
                    budget,
                );
                let expected = ordered(&expected, direction);
                let label = format!("{label}: {lower:?}..{upper:?} {direction:?} {budget}");
                assert_eq!(got, expected, "{label}");
                filtered(
                    reader,
                    SCANNED,
                    (lower.clone(), upper.clone()),
                    direction,
                    budget,
                    &expected,
                    &format!("{label} filtered"),
                );
            }
        }
    }
}

/// A sequence's records as the backend keys them
fn sequence_entries(records: &[Vec<u8>]) -> Vec<(RawKey, RawValue)> {
    let entries = records.iter().enumerate();
    entries
        .map(|(at, record)| (sequence_key(at as u64), record.clone()))
        .collect()
}

/// `entries` in `direction` order (`entries` ascending)
fn ordered(entries: &[(RawKey, RawValue)], direction: ScanDirection) -> Vec<(RawKey, RawValue)> {
    let mut entries = entries.to_vec();
    if direction == ScanDirection::Reverse {
        entries.reverse();
    }
    entries
}

/// Raw bytes of an entry
fn size((key, value): &(RawKey, RawValue)) -> usize {
    key.len() + value.len()
}

/// A `scan_range`, joined, checking every chunk is non-empty, within `budget` (or one entry) and
/// full: a chunk ends only where the next entry would take it past `budget`
fn chunked(
    reader: &impl BackendReader,
    namespace: Namespace,
    lower: Bound<RawKey>,
    upper: Bound<RawKey>,
    direction: ScanDirection,
    budget: usize,
) -> Vec<(RawKey, RawValue)> {
    let range = ScanRange {
        lower,
        upper,
        direction,
        chunk_bytes: NonZeroUsize::new(budget).expect("budgets are non-zero"),
    };
    let chunks: Vec<Vec<(RawKey, RawValue)>> = reader
        .scan_range(namespace, range, raw_entry)
        .collect::<Result<_, _>>()
        .expect("scan_range");
    for (at, chunk) in chunks.iter().enumerate() {
        let bytes: usize = chunk.iter().map(size).sum();
        assert!(!chunk.is_empty(), "{namespace}: an empty chunk");
        assert!(
            chunk.len() == 1 || bytes <= budget,
            "{namespace}: a {bytes}-byte chunk over its {budget}-byte budget"
        );
        if let Some(next) = chunks.get(at + 1).and_then(|next| next.first()) {
            assert!(
                bytes + size(next) > budget,
                "{namespace}: a {bytes}-byte chunk ended before an entry that fit its \
                 {budget}-byte budget"
            );
        }
    }
    chunks.concat()
}

/// A `scan_range` through a filter keeping every other entry, checking the filter sees every
/// entry in `entries` (all of the range, in walk order) exactly once, and each chunk counts only
/// kept entries against `budget` and ends only where the next entry seen would pass it
fn filtered(
    reader: &impl BackendReader,
    namespace: Namespace,
    (lower, upper): (Bound<RawKey>, Bound<RawKey>),
    direction: ScanDirection,
    budget: usize,
    entries: &[(RawKey, RawValue)],
    label: &str,
) {
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let filter = {
        let seen = std::sync::Arc::clone(&seen);
        move |key: &[u8], value: &[u8]| {
            let mut seen = seen.lock().expect("filter log");
            let keep = seen.len() % 2 == 0;
            seen.push(key.to_vec());
            keep.then(|| (key.to_vec(), value.to_vec()))
        }
    };
    let range = ScanRange {
        lower,
        upper,
        direction,
        chunk_bytes: NonZeroUsize::new(budget).expect("budgets are non-zero"),
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
    let kept: Vec<(RawKey, RawValue)> = entries.iter().step_by(2).cloned().collect();
    assert_eq!(chunks.concat(), kept, "{label}: exactly the kept entries");
    for (at, chunk) in chunks.iter().enumerate() {
        let bytes: usize = chunk.iter().map(size).sum();
        assert!(!chunk.is_empty(), "{label}: an empty chunk");
        assert!(
            chunk.len() == 1 || bytes <= budget,
            "{label}: {bytes} kept bytes over the {budget}-byte budget"
        );
        let stopped_before = seen_by
            .get(at)
            .filter(|_| at + 1 < chunks.len())
            .and_then(|&seen| entries.get(seen));
        if let Some(next) = stopped_before {
            assert!(
                bytes + size(next) > budget,
                "{label}: a chunk of {bytes} kept bytes ended before an entry that fit {budget}"
            );
        }
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
}

/// - `Grow`: node above the newest (`records` blocks + nodes, one height, `removals` of held
///   scanned rows, `owners` scanned rows, `ids` probed ids)
/// - `Apply`: oldest unapplied nodes committed, `count` reduced at run time
/// - `Flush`: every committed node durable
/// - `Reorg`: unapplied nodes cut to `keep` (reduced), later nodes on a new branch
/// - `Settle`: background work finishes; `Reopen`: exit mid-work; `PowerLoss`: crash now (both:
///   unflushed commits lost unless the subject keeps them; nodes kept and re-applied later)
/// - `Pin`: a reader + the model it must keep answering, re-checked every later step
/// - `Scan`: raw bounds reduced at run time, both directions, `budget` and 1-byte chunks
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
///   keeps unflushed commits); [`Subject::check`] holds
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
                match !crashed && subject.keeps_unflushed() {
                    true => oracle.flushed(),
                    false => oracle.applied = 0,
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
    for (namespace, key) in [(BLOCKS, vec![0; 4]), (PROBED, vec![0; 15])] {
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
    let footprint = backend.footprint().expect("footprint");
    assert!(
        footprint.iter().all(|namespace| namespace.bytes > 0),
        "flushed: every namespace holds data {footprint:?}"
    );
    backend.flush().expect("an empty flush");
    drop(backend);

    let backend = subject.open();
    let reader = || backend.reader().expect("reader");
    model.assert_view(&reader(), "reopened");
    assert!(
        backend.verify().expect("verify").is_clean(),
        "reopened: verify clean"
    );

    let flushed = model.clone();
    let (tip, ops) = model.advance(1, &[3], 1, &[0]);
    commit(&backend, tip, ops.clone()).expect("commit");
    drop(backend);
    let backend = subject.open();
    let reader = || backend.reader().expect("reader");
    match subject.keeps_unflushed() {
        true => model.assert_view(&reader(), "reopened: unflushed kept"),
        false => {
            flushed.assert_view(&reader(), "reopened: unflushed lost");
            commit(&backend, tip, ops).expect("recommit");
        }
    }
    model.assert_view(&reader(), "recommitted");

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
            Box::new(|e| matches!(e, CommitError::NotNextPosition { .. })),
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
            "a delete in a sequence",
            next(),
            vec![delete(BLOCKS, sequence_key(0))],
            Box::new(|e| matches!(e, CommitError::DeleteNotAllowed { .. })),
        ),
        (
            "a delete in a map without deletes",
            next(),
            vec![delete(PROBED, probed_key(0))],
            Box::new(|e| matches!(e, CommitError::DeleteNotAllowed { .. })),
        ),
        (
            "a key inserted twice in one batch",
            next(),
            vec![
                put(PROBED, probed_key(9_999), vec![0; 4]),
                put(PROBED, probed_key(9_999), vec![1; 4]),
            ],
            Box::new(|e| matches!(e, CommitError::DuplicateInBatch { .. })),
        ),
        (
            "a key inserted and removed in one batch",
            next(),
            vec![
                put(SCANNED, fresh.clone(), vec![0; 8]),
                delete(SCANNED, fresh.clone()),
            ],
            Box::new(|e| matches!(e, CommitError::DuplicateInBatch { .. })),
        ),
        (
            "a key already present",
            next(),
            vec![ok_append(), put(PROBED, probed_key(0), vec![0; 4])],
            Box::new(|e| matches!(e, CommitError::KeyExists { .. })),
        ),
        (
            "a delete of an absent key",
            next(),
            vec![ok_append(), delete(SCANNED, fresh.clone())],
            Box::new(|e| matches!(e, CommitError::KeyAbsent { .. })),
        ),
        (
            "a re-insert of a removed key",
            next(),
            vec![ok_append(), put(SCANNED, removed.clone(), vec![0; 8])],
            Box::new(|e| matches!(e, CommitError::KeyRemoved { .. })),
        ),
        (
            "a second delete of a removed key",
            next(),
            vec![ok_append(), delete(SCANNED, removed.clone())],
            Box::new(|e| matches!(e, CommitError::KeyRemoved { .. })),
        ),
        (
            "a valid op before a refused one",
            next(),
            vec![
                delete(SCANNED, held.clone()),
                put(SCANNED, held.clone(), vec![0; 8]),
            ],
            Box::new(|e| matches!(e, CommitError::DuplicateInBatch { .. })),
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
}
