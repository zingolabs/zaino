//! `SelfCumulative<OrderedMonoid>` × Append: a toy commitment tree.
//!
//! The in-crate archetype the append-cumulative bridge's **ordered-monoid scan
//! strategy** exists for — the shape of the real `tree_state` index, but over a
//! single tree of `u64` leaves and a toy hash, so the engine machinery is
//! testable without the zcash crates.
//!
//! Each block contributes an ordered run of leaves. The index keeps, at every
//! height, the **frontier** of an append-only binary Merkle tree: the perfect-
//! subtree roots ("peaks") covering all leaves through that height. The value at
//! height `h` is computed from the value at `h−1` (scope is `SelfCumulative`),
//! yet every height emits its own disjoint `height → frontier` entry
//! (composition is `Append`).
//!
//! # Why it is an ordered monoid, not a serial fold
//!
//! The naive build folds block by block: each block needs the previous block's
//! frontier before it can hash anything. Instead:
//!
//! - a block's **start position** is the tree size before it — a prefix sum of
//!   per-block leaf counts ([`Measure`](OrderedMonoidCarry::Measure)), which is a
//!   plain commutative sum;
//! - given its start, a block's [`Segment`](ToySegment) is built independently
//!   ([`lift`](ToyMerkleIndex::lift)) — this is where every node is hashed, once;
//! - segments combine by an **associative, non-commutative** seam join
//!   ([`combine`](ToyMerkleIndex::combine)), so a batch reduces in parallel while
//!   staying in chain order;
//! - each height's frontier is then a pure lookup
//!   ([`project`](ToyMerkleIndex::project)) into the combined segment — no height
//!   re-hashes a node that straddles a block boundary.
//!
//! The toy hash `blake2b(left ‖ right)` is deliberately **order-sensitive**
//! (`hash(a‖b) ≠ hash(b‖a)`), so a combine that reversed operands, or a bridge
//! that fed blocks to the reduce out of chain order without re-sorting, would
//! produce a wrong frontier deterministically.
//!
//! # Memory
//!
//! A [`ToySegment`] retains **every complete node** in its range so projection
//! is a lookup. A perfect-or-ragged run of `m` leaves has `m` leaf nodes and at
//! most `m − 1` internal nodes, so a segment holds **≤ 2·m nodes** (≤ 2·m·32 B).
//! That is the per-batch bound the real index inherits (≈25 MB for a spam-era
//! 500k-leaf batch).

use std::collections::BTreeMap;
use std::convert::Infallible;

use crate::descriptor::{Append, OrderedMonoid, SelfCumulative};
use crate::primitives::{BlockHeight, IndexId};
use crate::traits::{
    CumulativeAppend, ExtractCumulative, IndexDef, MergeAppend, OrderedMonoidCarry, Schema,
};
use zaino_persistence_codec::keys::HeightKey;
use zaino_persistence_codec::{
    DecodeError as PersistDecodeError, EntryCodec, KeyOrder, PersistentRecord,
};

/// A toy tree node: a 32-byte hash.
type Node = [u8; 32];

/// `blake2b` truncated to 32 bytes — the toy's one hash primitive.
fn blake2b32(data: &[u8]) -> Node {
    let hash = blake2b_simd::Params::new().hash_length(32).hash(data);
    let mut out = [0u8; 32];
    out.copy_from_slice(hash.as_bytes());
    out
}

/// Hash of a single leaf value.
fn leaf_node(leaf: u64) -> Node {
    blake2b32(&leaf.to_le_bytes())
}

/// Hash of an internal node from its children, **`left` before `right`**. Order-
/// sensitive: `hash_pair(a, b) != hash_pair(b, a)`, which is what makes leaf
/// order (chain order) load-bearing.
fn hash_pair(left: &Node, right: &Node) -> Node {
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(left);
    buf[32..].copy_from_slice(right);
    blake2b32(&buf)
}

/// One peak of a frontier: the root of a perfect subtree of `2^level` leaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Peak {
    /// Height of the perfect subtree (it covers `2^level` leaves).
    pub level: u8,
    /// Its root hash.
    pub node: Node,
}

/// The frontier of the tree after some number of leaves: the perfect-subtree
/// roots covering `[0, size)`, ordered **largest first** (the binary
/// decomposition of `size`, most-significant bit first). This is the index's
/// value *and* its carry (`PriorState = Value`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frontier {
    /// Number of leaves this frontier covers.
    pub size: u64,
    /// Peaks, largest level first.
    pub peaks: Vec<Peak>,
}

impl Frontier {
    /// The empty frontier — no leaves, no peaks (the genesis carry).
    fn empty() -> Self {
        Self {
            size: 0,
            peaks: Vec::new(),
        }
    }
}

/// A summary of a contiguous run of leaves `[start, start + len)` holding every
/// complete node wholly inside the run, keyed by `(level, index)` where `index =
/// left_edge / 2^level`. The ordered monoid lives here.
#[derive(Debug, Clone)]
pub struct ToySegment {
    start: u64,
    len: u64,
    nodes: BTreeMap<(u8, u64), Node>,
}

impl ToySegment {
    /// The number of retained nodes — the memory figure the bound in the module
    /// docs (`≤ 2·leaves`) is asserted against.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Fill in every internal node that is now complete (both children present)
    /// and wholly inside `[start, start + len)`. A single ascending pass
    /// suffices: level `L` reads level `L − 1`, already finalised. This is the
    /// only place `combine`/`lift` hash internal nodes.
    fn fill_complete(&mut self) {
        let end = self.start + self.len;
        for level in 1u8..=63 {
            let span = 1u64 << level;
            if span > end {
                break;
            }
            let quotient = end / span;
            if quotient == 0 {
                continue;
            }
            let last_index = quotient - 1;
            let first_index = self.start.div_ceil(span);
            for index in first_index..=last_index {
                let key = (level, index);
                if self.nodes.contains_key(&key) {
                    continue;
                }
                let left = self.nodes.get(&(level - 1, 2 * index)).copied();
                let right = self.nodes.get(&(level - 1, 2 * index + 1)).copied();
                if let (Some(left), Some(right)) = (left, right) {
                    self.nodes.insert(key, hash_pair(&left, &right));
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Sequential reference fold — the ground truth the ordered-monoid path must
// match, and the body of the trait-required `extract`.
// ---------------------------------------------------------------------------

/// Push one leaf onto a frontier held as a `level → peak` map (a binary counter:
/// carry while a peak at the current level already exists, hashing earlier-left).
fn push_leaf(peaks: &mut BTreeMap<u8, Node>, leaf: u64) {
    let mut node = leaf_node(leaf);
    let mut level = 0u8;
    while let Some(sibling) = peaks.remove(&level) {
        node = hash_pair(&sibling, &node);
        level += 1;
    }
    peaks.insert(level, node);
}

/// Append a block's leaves to a frontier, the serial way — the ground truth for
/// the tests and the body of [`ExtractCumulative::extract`].
pub fn append_leaves(prior: &Frontier, leaves: &[u64]) -> Frontier {
    let mut peaks: BTreeMap<u8, Node> = prior.peaks.iter().map(|p| (p.level, p.node)).collect();
    for &leaf in leaves {
        push_leaf(&mut peaks, leaf);
    }
    let added = u64::try_from(leaves.len()).expect("leaf count fits u64");
    let mut peaks: Vec<Peak> = peaks
        .into_iter()
        .map(|(level, node)| Peak { level, node })
        .collect();
    // Largest level first, matching `project`'s most-significant-bit-first walk.
    peaks.sort_by_key(|peak| std::cmp::Reverse(peak.level));
    Frontier {
        size: prior.size + added,
        peaks,
    }
}

/// Deterministic leaves for a block, so the engine's `TestBlockContext` (which
/// carries only a `value`) can drive this index: `value` is the leaf count, and
/// the leaf payloads are a function of the height so distinct blocks hash
/// distinctly. Shared with the engine-level tests' reference fold.
pub fn leaves_for(height: u64, count: u32) -> Vec<u64> {
    (0..count).map(|j| (height << 20) ^ u64::from(j)).collect()
}

// ---------------------------------------------------------------------------
// The index
// ---------------------------------------------------------------------------

/// Block context: the block's height (the entry key) and its ordered leaves.
pub struct Context {
    /// Block height — the entry key.
    pub height: BlockHeight,
    /// This block's leaves, in chain order.
    pub leaves: Vec<u64>,
}

/// One height's entry: the frontier after this block.
pub struct TreeEntry {
    /// Block height (key).
    pub height: BlockHeight,
    /// Frontier after this block (value, and the carry to the next height).
    pub frontier: Frontier,
}

/// Toy commitment-tree index: `height → frontier`, carry is an ordered monoid.
pub struct ToyMerkleIndex;

/// Index identity.
pub const ID: IndexId = IndexId::new("toy_merkle");

impl IndexDef for ToyMerkleIndex {
    type Scope = SelfCumulative<OrderedMonoid>;
    type Composition = Append;
    type Delta = TreeEntry;
    type BlockContext = Context;

    const NAME: IndexId = ID;
}

impl ExtractCumulative for ToyMerkleIndex {
    type PriorState = Frontier;
    type Error = Infallible;

    // The serial step. The ordered-monoid bridge never calls this (it uses the
    // `OrderedMonoidCarry` methods below); it is kept both because the trait
    // requires it and because it is the ground-truth fold.
    fn extract(ctx: &Context, prior: &Frontier) -> Result<TreeEntry, Infallible> {
        Ok(TreeEntry {
            height: ctx.height,
            frontier: append_leaves(prior, &ctx.leaves),
        })
    }
}

impl MergeAppend for ToyMerkleIndex {}

impl CumulativeAppend for ToyMerkleIndex {
    fn initial_carry() -> Frontier {
        Frontier::empty()
    }

    fn carry(delta: &TreeEntry) -> Frontier {
        delta.frontier.clone()
    }
}

impl OrderedMonoidCarry for ToyMerkleIndex {
    type Measure = u64;

    fn measure_of(ctx: &Context) -> u64 {
        u64::try_from(ctx.leaves.len()).expect("leaf count fits u64")
    }

    fn measure_add(a: u64, b: u64) -> u64 {
        a + b
    }

    fn carry_measure(carry: &Frontier) -> u64 {
        carry.size
    }

    type Segment = ToySegment;

    fn lift(ctx: &Context, start: u64) -> Result<ToySegment, Infallible> {
        let mut nodes = BTreeMap::new();
        for (j, &leaf) in ctx.leaves.iter().enumerate() {
            let offset = u64::try_from(j).expect("leaf index fits u64");
            nodes.insert((0u8, start + offset), leaf_node(leaf));
        }
        let mut segment = ToySegment {
            start,
            len: u64::try_from(ctx.leaves.len()).expect("leaf count fits u64"),
            nodes,
        };
        segment.fill_complete();
        Ok(segment)
    }

    fn carry_segment(carry: &Frontier) -> ToySegment {
        // Place each peak at its `(level, index)`. The peaks' sub-nodes are not
        // retained (the frontier does not hold them), and are not needed:
        // `project` for a height ≥ `size` only ever reads a carry peak whole or
        // as a child of a seam node, never a node *inside* a peak.
        let mut nodes = BTreeMap::new();
        let mut position = 0u64;
        for peak in &carry.peaks {
            nodes.insert((peak.level, position >> peak.level), peak.node);
            position += 1u64 << peak.level;
        }
        ToySegment {
            start: 0,
            len: carry.size,
            nodes,
        }
    }

    fn identity() -> ToySegment {
        ToySegment {
            start: 0,
            len: 0,
            nodes: BTreeMap::new(),
        }
    }

    fn combine(a: ToySegment, b: ToySegment) -> ToySegment {
        if a.len == 0 {
            return b;
        }
        if b.len == 0 {
            return a;
        }
        debug_assert_eq!(a.start + a.len, b.start, "combine of non-contiguous runs");
        let mut nodes = a.nodes;
        nodes.extend(b.nodes);
        let mut merged = ToySegment {
            start: a.start,
            len: a.len + b.len,
            nodes,
        };
        // The seam hashes: at most one newly-complete node per level.
        merged.fill_complete();
        merged
    }

    fn project(full: &ToySegment, at: u64) -> Frontier {
        let mut peaks = Vec::new();
        let mut position = 0u64;
        for level in (0u8..=63).rev() {
            let span = 1u64 << level;
            if at & span != 0 {
                let index = position >> level;
                let node = *full.nodes.get(&(level, index)).expect(
                    "peak node present in combined segment (ordered-monoid lookup invariant)",
                );
                peaks.push(Peak { level, node });
                position += span;
            }
        }
        Frontier { size: at, peaks }
    }

    fn key_of(ctx: &Context) -> BlockHeight {
        ctx.height
    }
}

impl Schema<Vec<TreeEntry>> for ToyMerkleIndex {
    fn into_entries(entries: Vec<TreeEntry>) -> Vec<(Self::Key, Self::Value)> {
        entries
            .into_iter()
            .map(|e| (e.height, e.frontier))
            .collect()
    }

    fn from_entries(entries: Vec<(Self::Key, Self::Value)>) -> Vec<TreeEntry> {
        entries
            .into_iter()
            .map(|(height, frontier)| TreeEntry { height, frontier })
            .collect()
    }
}

impl EntryCodec for ToyMerkleIndex {
    type Key = BlockHeight;
    type Value = Frontier;
    type PersistentKey = HeightKey<BlockHeight>;
    type PersistentValue = PersistentFrontier;

    const KEY_ORDER: KeyOrder = KeyOrder::WalkOrdered;

    fn fingerprint_samples() -> Vec<(BlockHeight, Frontier)> {
        vec![(
            BlockHeight::new(1),
            Frontier {
                size: 1,
                peaks: vec![Peak {
                    level: 0,
                    node: leaf_node(0),
                }],
            },
        )]
    }
}

/// On-disk record for a [`Frontier`]: `size` little-endian, then each peak as
/// `level` (1 byte) followed by its 32-byte node.
#[derive(PersistentRecord)]
pub struct PersistentFrontier(Vec<u8>);

impl PersistentRecord for PersistentFrontier {
    type Domain = Frontier;

    fn from_domain(domain: &Frontier) -> Self {
        let mut bytes = Vec::with_capacity(8 + domain.peaks.len() * 33);
        bytes.extend_from_slice(&domain.size.to_le_bytes());
        for peak in &domain.peaks {
            bytes.push(peak.level);
            bytes.extend_from_slice(&peak.node);
        }
        Self(bytes)
    }

    fn into_domain(self) -> Result<Frontier, PersistDecodeError> {
        let bytes = self.0;
        if bytes.len() < 8 || !(bytes.len() - 8).is_multiple_of(33) {
            return Err(PersistDecodeError::Invalid(format!(
                "frontier record has invalid length {}",
                bytes.len()
            )));
        }
        let size = u64::from_le_bytes(bytes[..8].try_into().expect("8 bytes checked above"));
        let mut peaks = Vec::new();
        for chunk in bytes[8..].chunks_exact(33) {
            let level = chunk[0];
            let mut node = [0u8; 32];
            node.copy_from_slice(&chunk[1..]);
            peaks.push(Peak { level, node });
        }
        Ok(Frontier { size, peaks })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Algebra-level tests: the ordered monoid itself.
    // -----------------------------------------------------------------------

    /// Build a segment for a contiguous run of `count` leaves starting at
    /// `start`, with the same payloads the engine path uses (per some height).
    fn segment_from(height: u64, count: u32, start: u64) -> ToySegment {
        let ctx = Context {
            height: BlockHeight::new(height),
            leaves: leaves_for(height, count),
        };
        ToyMerkleIndex::lift(&ctx, start).expect("lift is infallible")
    }

    /// A reference frontier after folding `blocks` (each `(height, count)`)
    /// serially from empty.
    fn reference(blocks: &[(u64, u32)]) -> Vec<Frontier> {
        let mut frontier = Frontier::empty();
        let mut out = Vec::new();
        for &(height, count) in blocks {
            frontier = append_leaves(&frontier, &leaves_for(height, count));
            out.push(frontier.clone());
        }
        out
    }

    /// Run the ordered-monoid path (measure → lift → ordered reduce → project)
    /// over `blocks` with a given start carry, as the bridge's `merge` does,
    /// returning the per-block frontiers.
    fn ordered_path(start_carry: &Frontier, blocks: &[(u64, u32)]) -> Vec<Frontier> {
        let start0 = ToyMerkleIndex::carry_measure(start_carry);
        let mut starts = Vec::new();
        let mut ends = Vec::new();
        let mut acc = start0;
        for &(height, count) in blocks {
            let m = ToyMerkleIndex::measure_of(&Context {
                height: BlockHeight::new(height),
                leaves: leaves_for(height, count),
            });
            starts.push(acc);
            acc = ToyMerkleIndex::measure_add(acc, m);
            ends.push(acc);
        }
        let segments: Vec<ToySegment> = blocks
            .iter()
            .zip(&starts)
            .map(|(&(height, count), &start)| segment_from(height, count, start))
            .collect();
        let batch = segments
            .into_iter()
            .fold(ToyMerkleIndex::identity(), ToyMerkleIndex::combine);
        let full = ToyMerkleIndex::combine(ToyMerkleIndex::carry_segment(start_carry), batch);
        ends.iter()
            .map(|&end| ToyMerkleIndex::project(&full, end))
            .collect()
    }

    #[test]
    fn combine_is_non_commutative() {
        // a: leaves [0,2), b: leaves [2,4). a⊕b is a real tree; swapping the
        // operands (treating b's leaves as earlier) gives a different root.
        let a = segment_from(0, 2, 0);
        let b = segment_from(1, 2, 2);
        let ab = ToyMerkleIndex::combine(a, b);
        // Re-lift at swapped positions to form b⊕a over the same span.
        let b_first = segment_from(1, 2, 0);
        let a_second = segment_from(0, 2, 2);
        let ba = ToyMerkleIndex::combine(b_first, a_second);
        assert_ne!(
            ab.nodes.get(&(1, 0)),
            ba.nodes.get(&(1, 0)),
            "order-sensitive hash must make a⊕b differ from b⊕a"
        );
    }

    #[test]
    fn combine_is_associative() {
        let blocks = [(0u64, 3u32), (1, 2), (2, 4)];
        let mut starts = Vec::new();
        let mut acc = 0u64;
        for &(_, c) in &blocks {
            starts.push(acc);
            acc += u64::from(c);
        }
        let s: Vec<ToySegment> = blocks
            .iter()
            .zip(&starts)
            .map(|(&(h, c), &st)| segment_from(h, c, st))
            .collect();
        let left = ToyMerkleIndex::combine(
            ToyMerkleIndex::combine(s[0].clone(), s[1].clone()),
            s[2].clone(),
        );
        let right = ToyMerkleIndex::combine(
            s[0].clone(),
            ToyMerkleIndex::combine(s[1].clone(), s[2].clone()),
        );
        assert_eq!(left.nodes, right.nodes, "combine must be associative");
    }

    #[test]
    fn identity_is_a_unit() {
        let a = segment_from(0, 5, 0);
        let left = ToyMerkleIndex::combine(ToyMerkleIndex::identity(), a.clone());
        let right = ToyMerkleIndex::combine(a.clone(), ToyMerkleIndex::identity());
        assert_eq!(left.nodes, a.nodes);
        assert_eq!(right.nodes, a.nodes);
    }

    #[test]
    fn segment_node_count_within_twice_leaves() {
        for len in [1u32, 2, 3, 7, 16, 31, 100, 257] {
            let seg = segment_from(0, len, 0);
            assert!(
                seg.node_count() <= 2 * usize::try_from(len).expect("fits"),
                "len={len} held {} nodes, over the 2·leaves bound",
                seg.node_count()
            );
        }
    }

    // Step 1(d): a half-million-leaf batch completes and stays within 2·leaves.
    #[test]
    fn large_batch_completes_within_memory_bound() {
        // 50 blocks of 10,000 leaves = 500,000 leaves in one batch.
        let blocks: Vec<(u64, u32)> = (0u64..50).map(|h| (h, 10_000)).collect();
        let mut starts = Vec::new();
        let mut acc = 0u64;
        for &(_, c) in &blocks {
            starts.push(acc);
            acc += u64::from(c);
        }
        let total = usize::try_from(acc).expect("fits");
        let batch = blocks
            .iter()
            .zip(&starts)
            .map(|(&(h, c), &st)| segment_from(h, c, st))
            .fold(ToyMerkleIndex::identity(), ToyMerkleIndex::combine);
        assert_eq!(batch.len, acc);
        assert!(
            batch.node_count() <= 2 * total,
            "500k-leaf batch held {} nodes, over the 2·leaves bound",
            batch.node_count()
        );
        // And projection answers at the tip without hashing or panicking.
        let tip = ToyMerkleIndex::project(&batch, acc);
        assert_eq!(tip.size, acc);
    }

    // -----------------------------------------------------------------------
    // Step 1(a): the ordered-monoid path equals the sequential fold, for random
    // block shapes and a random (prior-block-built) start carry.
    // -----------------------------------------------------------------------
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn ordered_path_equals_sequential_fold(
            prior in prop::collection::vec((0u64..20, 0u32..300), 0..6),
            batch in prop::collection::vec((20u64..60, 0u32..300), 1..50),
        ) {
            // The start carry is whatever folding the `prior` blocks produces.
            let mut start_carry = Frontier::empty();
            for &(h, c) in &prior {
                start_carry = append_leaves(&start_carry, &leaves_for(h, c));
            }

            let got = ordered_path(&start_carry, &batch);

            // Reference: continue the serial fold from the same start carry.
            let mut frontier = start_carry.clone();
            let mut expected = Vec::new();
            for &(h, c) in &batch {
                frontier = append_leaves(&frontier, &leaves_for(h, c));
                expected.push(frontier.clone());
            }

            prop_assert_eq!(got, expected);
        }
    }

    #[test]
    fn ordered_path_matches_reference_from_empty() {
        let blocks = [(0u64, 1u32), (1, 2), (2, 3), (3, 0), (4, 5), (5, 300)];
        let got = ordered_path(&Frontier::empty(), &blocks);
        let expected = reference(&blocks);
        assert_eq!(got, expected);
    }

    // -----------------------------------------------------------------------
    // Engine-level tests: the real bridge, driven end to end through the
    // scheduler (which now emits an ordered-monoid batch block-parallel).
    // -----------------------------------------------------------------------
    use crate::engine::{EngineConfig, ReverseExtractionGuard, SyncEngine};
    use crate::index_pipelines::IndexPipelines;
    use crate::primitives::BlockHeight as EngineHeight;
    use crate::testing::{InMemoryBackend, TestBlockContext};
    use std::collections::BTreeMap;

    /// A deterministic, varied leaf count per height (includes zero).
    fn count_for(height: u64) -> u32 {
        u32::try_from((height * 13 + 5) % 23).expect("small")
    }

    /// Read the whole stored series as `height → frontier`.
    fn read_series(backend: &InMemoryBackend) -> BTreeMap<u64, Frontier> {
        backend
            .entries(ID.into())
            .into_iter()
            .map(|(key, value)| {
                let height =
                    u64::from_be_bytes(key.as_slice().try_into().expect("8-byte height key"));
                let frontier = zaino_persistence_codec::decode_value::<ToyMerkleIndex>(&value)
                    .expect("frontier decodes");
                (height, frontier)
            })
            .collect()
    }

    /// The reference series over heights `0..n`: fold `leaves_for` cumulatively.
    fn expected_series(n: u64) -> BTreeMap<u64, Frontier> {
        let mut frontier = Frontier::empty();
        let mut out = BTreeMap::new();
        for height in 0..n {
            frontier = append_leaves(&frontier, &leaves_for(height, count_for(height)));
            out.insert(height, frontier.clone());
        }
        out
    }

    fn blocks(n: u64) -> Vec<TestBlockContext> {
        (0..n)
            .map(|height| TestBlockContext {
                height,
                value: count_for(height),
            })
            .collect()
    }

    fn engine(
        backend: InMemoryBackend,
        batch_size: u32,
        start: u64,
    ) -> SyncEngine<TestBlockContext, InMemoryBackend> {
        SyncEngine::from_pipelines(
            IndexPipelines::new().with::<ToyMerkleIndex>(),
            backend,
            EngineConfig {
                batch_size,
                start_height: EngineHeight::new(start),
            },
        )
        .expect("valid index set")
    }

    // Step 1(a): every per-height frontier from the ordered-monoid bridge equals
    // the sequential fold, and is identical across batch boundaries (the carry
    // threads correctly). Batch sizes probe single-batch, many-batch, and a
    // subtree-straddling boundary.
    #[test]
    fn bridge_series_equals_fold_and_is_batch_invariant() {
        let n = 40;
        let expected = expected_series(n);
        for batch_size in [1u32, 2, 7, 50] {
            let backend = InMemoryBackend::new();
            let mut eng = engine(backend.clone(), batch_size, 0);
            eng.sync_range(blocks(n)).expect("sync succeeds");
            assert_eq!(
                read_series(&backend),
                expected,
                "ordered-monoid series must match the fold at batch_size={batch_size}"
            );
        }
    }

    // Step 1(b): the same, but with the batch's blocks fed to the bridge in
    // reversed job order. Correct only because `merge` sorts the buffer by
    // offset before the measure prefix and reduce; a bridge that reduced in
    // arrival order would hash a different (wrong) tree under the order-sensitive
    // combine.
    #[test]
    fn bridge_series_correct_under_reversed_job_order() {
        let n = 50;
        let expected = expected_series(n);
        let backend = InMemoryBackend::new();
        let mut eng = engine(backend.clone(), 7, 0);
        {
            let _reversed = ReverseExtractionGuard::new();
            eng.sync_range(blocks(n)).expect("sync succeeds");
        }
        assert_eq!(read_series(&backend), expected);
    }

    // Step 1(c): stop after some batches, rebuild the bridge on the same backend
    // (carry reloaded by point-reading the frontier at the watermark), and
    // continue. The result equals an uninterrupted run.
    #[test]
    fn bridge_resumes_carry_from_watermark() {
        let n = 30;
        let split = 12u64;
        let expected = expected_series(n);
        let backend = InMemoryBackend::new();

        // Phase 1: heights 0..split.
        {
            let mut eng = engine(backend.clone(), 5, 0);
            eng.sync_range(blocks(split))
                .expect("phase 1 sync succeeds");
        }
        let watermark = SyncEngine::<TestBlockContext, _>::committed_height(&backend)
            .expect("read succeeds")
            .expect("watermark exists");
        assert_eq!(watermark, EngineHeight::new(split - 1));

        // Phase 2: a fresh engine on the same backend continues from split.
        {
            let mut eng = engine(backend.clone(), 5, split);
            let rest: Vec<_> = (split..n)
                .map(|height| TestBlockContext {
                    height,
                    value: count_for(height),
                })
                .collect();
            eng.sync_range(rest).expect("phase 2 sync succeeds");
        }

        assert_eq!(
            read_series(&backend),
            expected,
            "resumed series must continue the fold byte-identically"
        );
    }
}
