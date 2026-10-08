//! The commitment-tree segment algebra: an ordered monoid over a contiguous run
//! of note-commitment leaves, generic over a pool's Merkle hash `H`.
//!
//! A [`TreeSegment`] summarises a contiguous run of leaves `[start, start + len)`
//! of an append-only binary Merkle tree. It retains every *complete* node wholly
//! inside its range, keyed by `(level, index)` where a node at `(level, index)`
//! is the root of the perfect subtree covering leaves
//! `[index · 2^level, (index + 1) · 2^level)`. A segment built from a carry
//! [`Frontier`] additionally retains that frontier's **peaks** — the roots of the
//! perfect subtrees that tile `[0, carry_size)` — so a later combine can stitch a
//! batch onto the carried state.
//!
//! # Why an ordered monoid
//!
//! The naive per-height fold needs block `h-1`'s frontier before it can hash
//! anything in block `h`. Instead each block's segment is [`lift`](TreeSegment::lift)ed
//! independently (every node hashed exactly once, parallel across a level), the
//! batch is combined by an order-preserving reduce, and every height's frontier
//! is a pure lookup ([`frontier_at`](TreeSegment::frontier_at)) into the combined
//! segment.
//!
//! [`combine`](TreeSegment::combine) is **associative** (its node set is "every
//! complete node in the joined range", independent of grouping) with the empty
//! segment as **identity**, and is **not commutative** (leaf order is chain
//! order; the Merkle hash is order-sensitive). The seam join costs **at most one
//! hash per level** — `≤ DEPTH` hashes — because the only complete nodes missing
//! from `a`'s and `b`'s union are the ≤ `DEPTH` nodes that straddle the
//! `a.end == b.start` boundary, one per level, which the seam walk builds
//! bottom-up along the boundary spine. It never rescans the carried state.
//!
//! # Memory
//!
//! A run of `m` leaves holds `m` leaf nodes and at most `m - 1` internal nodes,
//! so a segment retains **≤ 2·m nodes**. The node store is a [`BTreeMap`] keyed
//! by `(level, index)`; a denser per-level packing is a possible later
//! optimisation, but the load-bearing property (the `≤ DEPTH`-hash seam) is
//! independent of the container.

use std::collections::BTreeMap;

use incrementalmerkletree::frontier::{Frontier, NonEmptyFrontier};
use incrementalmerkletree::{Hashable, Level, Position, Source};
use rayon::prelude::*;

/// The note-commitment tree depth for the Zcash shielded pools (Sapling,
/// Orchard, Ironwood).
pub const DEPTH: u8 = 32;

/// A summary of a contiguous run of leaves `[start, start + len)` as an ordered
/// monoid under [`combine`](Self::combine).
///
/// Generic over a pool's Merkle hash `H`; see the module docs for the retained
/// node set and the algebra's laws.
#[derive(Debug, Clone)]
pub struct TreeSegment<H> {
    start: u64,
    len: u64,
    nodes: BTreeMap<(u8, u64), H>,
}

impl<H> TreeSegment<H> {
    /// The identity segment `ε`: no leaves, no nodes. `combine(ε, x) == x ==
    /// combine(x, ε)`.
    pub fn empty() -> Self {
        Self {
            start: 0,
            len: 0,
            nodes: BTreeMap::new(),
        }
    }

    /// The first leaf position this segment covers.
    pub fn start(&self) -> u64 {
        self.start
    }

    /// The number of leaves this segment covers.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether this is the identity (empty) segment.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// One past the last leaf position this segment covers.
    pub fn end(&self) -> u64 {
        self.start + self.len
    }

    /// The number of retained nodes — the figure the `≤ 2·leaves` memory bound is
    /// asserted against.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// The retained node at `(level, index)`, or `None` if this segment does not
    /// hold it (outside its range, incomplete, or a non-peak interior node of a
    /// carried frontier).
    pub fn node_at(&self, level: u8, index: u64) -> Option<&H> {
        self.nodes.get(&(level, index))
    }
}

impl<H: Hashable + Clone + Send + Sync> TreeSegment<H> {
    /// Build the segment for a contiguous run of `leaves` starting at absolute
    /// leaf position `start`. Every complete node in `[start, start + leaves.len)`
    /// is hashed here, exactly once; the hashing for one level runs in parallel
    /// across that level's nodes.
    ///
    /// Pure given `(leaves, start)`, so an entire batch lifts in parallel.
    pub fn lift(leaves: &[H], start: u64) -> Self {
        let len = u64::try_from(leaves.len()).expect("leaf count fits u64");
        let mut nodes: BTreeMap<(u8, u64), H> = BTreeMap::new();
        if len == 0 {
            return Self {
                start,
                len: 0,
                nodes,
            };
        }
        let end = start + len;
        for (i, leaf) in leaves.iter().enumerate() {
            let index = start + u64::try_from(i).expect("leaf index fits u64");
            nodes.insert((0, index), leaf.clone());
        }
        let mut level = 0u8;
        while level < DEPTH {
            let parent_level = level + 1;
            let span = 1u64 << u32::from(parent_level);
            let first = start.div_ceil(span);
            let last = end / span;
            if first >= last {
                break;
            }
            // Each parent node reads only the (immutable) level below, so the
            // level hashes in parallel.
            let built: Vec<((u8, u64), H)> = (first..last)
                .into_par_iter()
                .map(|j| {
                    let left = nodes
                        .get(&(level, 2 * j))
                        .expect("left child of a complete node is present");
                    let right = nodes
                        .get(&(level, 2 * j + 1))
                        .expect("right child of a complete node is present");
                    (
                        (parent_level, j),
                        H::combine(Level::from(level), left, right),
                    )
                })
                .collect();
            for (key, node) in built {
                nodes.insert(key, node);
            }
            level = parent_level;
        }
        Self { start, len, nodes }
    }

    /// Render a carried [`Frontier`] as a segment covering `[0, carry_size)`.
    ///
    /// The frontier stores its right spine (leaf + ommers); this folds that spine
    /// into the carry's **peaks** — the roots of the perfect subtrees that tile
    /// `[0, carry_size)`, at the set-bit levels of `carry_size` — which are the
    /// left children a subsequent [`combine`](Self::combine) stitches the batch
    /// onto. The empty frontier yields the identity segment.
    pub fn carry_segment(frontier: &Frontier<H, DEPTH>) -> Self {
        match frontier.value() {
            None => Self::empty(),
            Some(nonempty) => {
                let size = u64::from(nonempty.position()) + 1;
                let mut peaks = frontier_to_peaks(nonempty);
                // Place peaks largest-level-first (leftmost), each at its absolute
                // `(level, index)`.
                peaks.sort_by_key(|peak| std::cmp::Reverse(peak.0));
                let mut nodes: BTreeMap<(u8, u64), H> = BTreeMap::new();
                let mut position = 0u64;
                for (level, node) in peaks {
                    nodes.insert((level, position >> u32::from(level)), node);
                    position += 1u64 << u32::from(level);
                }
                debug_assert_eq!(position, size, "peaks must tile [0, carry_size)");
                Self {
                    start: 0,
                    len: size,
                    nodes,
                }
            }
        }
    }

    /// Join two adjacent segments, with `a` the chain-earlier operand. Defined
    /// when `b` starts where `a` ends. Associative, not commutative; at most one
    /// hash per level (`≤ DEPTH`).
    pub fn combine(a: Self, b: Self) -> Self {
        if a.is_empty() {
            return b;
        }
        if b.is_empty() {
            return a;
        }
        debug_assert_eq!(a.end(), b.start, "combine of non-contiguous runs");
        let start = a.start;
        let seam = a.end();
        let len = a.len + b.len;
        let end = start + len;
        // Fold the smaller node set into the larger; the keys are disjoint except
        // for a carry segment's peaks, which are idempotent, so either direction
        // yields the same map.
        let (mut nodes, rest) = if a.nodes.len() >= b.nodes.len() {
            (a.nodes, b.nodes)
        } else {
            (b.nodes, a.nodes)
        };
        nodes.extend(rest);
        // The only complete nodes missing from the union straddle `seam`: one per
        // level, the node containing both leaf `seam - 1` and leaf `seam`. Build
        // them bottom-up along the seam spine so each one's children are ready.
        for parent_level in 1u8..=DEPTH {
            let span = 1u64 << u32::from(parent_level);
            if seam.is_multiple_of(span) {
                // The boundary is aligned at this level: the two sides already hold
                // complete nodes here, nothing straddles.
                continue;
            }
            let index = seam / span;
            if (index + 1) * span > end {
                // The straddling node extends past the combined range — not yet
                // complete, and no higher level can be either.
                break;
            }
            if index * span < start {
                // Its left part lies before this segment's data; it completes only
                // once the carried state is stitched on.
                continue;
            }
            if nodes.contains_key(&(parent_level, index)) {
                continue;
            }
            let child_level = parent_level - 1;
            let left = nodes.get(&(child_level, 2 * index)).cloned();
            let right = nodes.get(&(child_level, 2 * index + 1)).cloned();
            if let (Some(left), Some(right)) = (left, right) {
                nodes.insert(
                    (parent_level, index),
                    H::combine(Level::from(child_level), &left, &right),
                );
            }
        }
        Self { start, len, nodes }
    }

    /// The frontier of the tree after exactly `size` leaves, read from the
    /// retained nodes — a pure lookup, no hashing. `None` for `size == 0` (the
    /// empty tree) or if a required node is not retained (outside the segment's
    /// coverage).
    pub fn frontier_at(&self, size: u64) -> Option<NonEmptyFrontier<H>> {
        if size == 0 {
            return None;
        }
        let position = Position::from(size - 1);
        let leaf = self.node_at(0, size - 1)?.clone();
        let mut ommers = Vec::new();
        for (address, source) in position.witness_addrs(position.root_level()) {
            if let Source::Past(_) = source {
                let node = self.node_at(u8::from(address.level()), address.index())?;
                ommers.push(node.clone());
            }
        }
        NonEmptyFrontier::from_parts(position, leaf, ommers).ok()
    }
}

/// Fold a frontier's right spine (leaf + ommers) into the carry's peaks — the
/// perfect-subtree roots that tile `[0, position + 1)`, returned as
/// `(level, node)` pairs.
///
/// The ommers are left siblings at the set-bit levels of `position`, ascending.
/// Walking them low-to-high with the leaf as the initial rightmost subtree, a
/// same-level neighbour combines (binary-counter carry) while a gap closes off
/// the accumulated subtree as a peak — exactly the `+1` that turns the set bits
/// of `position` into the set bits of `position + 1`.
fn frontier_to_peaks<H: Hashable + Clone>(frontier: &NonEmptyFrontier<H>) -> Vec<(u8, H)> {
    let position = u64::from(frontier.position());
    let ommer_levels = (0u8..64).filter(|level| (position >> level) & 1 == 1);
    let mut peaks: Vec<(u8, H)> = Vec::new();
    let mut acc: (u8, H) = (0, frontier.leaf().clone());
    for (ommer_level, ommer) in ommer_levels.zip(frontier.ommers().iter()) {
        if ommer_level == acc.0 {
            // Same level: the ommer is the left sibling of the accumulated
            // (rightmost) subtree.
            acc = (acc.0 + 1, H::combine(Level::from(acc.0), ommer, &acc.1));
        } else {
            // A gap: the accumulated subtree is a finished peak, and the ommer
            // (a larger subtree to its left) becomes the new rightmost subtree.
            peaks.push(acc);
            acc = (ommer_level, ommer.clone());
        }
    }
    peaks.push(acc);
    peaks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexes::tree_state::pools::{orchard_leaf, sapling_leaf};
    use orchard::tree::MerkleHashOrchard;
    use proptest::prelude::*;
    use sapling_crypto::Node as SaplingNode;

    /// A sapling leaf node from a deterministic 32-byte payload that is a
    /// canonical field element (top bits cleared so `from_bytes` succeeds).
    fn sapling_leaf_of(seed: u64) -> SaplingNode {
        let mut bytes = [0u8; 32];
        bytes[..8].copy_from_slice(&seed.to_le_bytes());
        sapling_leaf(bytes).expect("canonical sapling cmu")
    }

    /// An orchard leaf node from a deterministic canonical 32-byte payload.
    fn orchard_leaf_of(seed: u64) -> MerkleHashOrchard {
        let mut bytes = [0u8; 32];
        bytes[..8].copy_from_slice(&seed.to_le_bytes());
        orchard_leaf(bytes).expect("canonical orchard cmx")
    }

    /// The reference frontier after appending `leaves` sequentially from empty.
    fn sequential_frontier<H: Hashable + Clone>(leaves: &[H]) -> Frontier<H, DEPTH> {
        let mut frontier = Frontier::<H, DEPTH>::empty();
        for leaf in leaves {
            assert!(frontier.append(leaf.clone()), "append within depth");
        }
        frontier
    }

    /// Lift `leaves` as a batch split at `split_points` (ascending, exclusive of
    /// 0 and len), each run starting at `start + offset`, then reduce in chain
    /// order.
    fn lift_and_combine<H: Hashable + Clone + Send + Sync>(
        leaves: &[H],
        start: u64,
        split_points: &[usize],
    ) -> TreeSegment<H> {
        let mut bounds = vec![0usize];
        bounds.extend_from_slice(split_points);
        bounds.push(leaves.len());
        let mut segment = TreeSegment::empty();
        for window in bounds.windows(2) {
            let (lo, hi) = (window[0], window[1]);
            let run_start = start + u64::try_from(lo).expect("fits");
            let run = TreeSegment::lift(&leaves[lo..hi], run_start);
            segment = TreeSegment::combine(segment, run);
        }
        segment
    }

    // Step 1(a): combine of lifted parts, then frontier_at(n), equals a
    // sequential Frontier::append for every n — for both pool node types, with a
    // random non-zero start carried from prior leaves.
    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]

        #[test]
        fn sapling_combine_matches_sequential_append(
            prior_count in 0usize..40,
            batch_count in 1usize..60,
            raw_splits in prop::collection::vec(any::<prop::sample::Index>(), 0..5),
        ) {
            let prior: Vec<SaplingNode> = (0..prior_count)
                .map(|i| sapling_leaf_of(0xA000 + u64::try_from(i).expect("fits")))
                .collect();
            let batch: Vec<SaplingNode> = (0..batch_count)
                .map(|i| sapling_leaf_of(0xB000 + u64::try_from(i).expect("fits")))
                .collect();

            let carry = sequential_frontier(&prior);
            let carry_segment = TreeSegment::carry_segment(&carry);
            let start = u64::try_from(prior_count).expect("fits");

            let mut splits: Vec<usize> =
                raw_splits.iter().map(|ix| 1 + ix.index(batch_count)).collect();
            splits.retain(|&s| s < batch_count);
            splits.sort_unstable();
            splits.dedup();

            let batch_segment = lift_and_combine(&batch, start, &splits);
            let full = TreeSegment::combine(carry_segment, batch_segment);

            // All prior+batch leaves, for the sequential reference.
            let mut all = prior.clone();
            all.extend(batch.iter().cloned());

            for extra in 1..=batch_count {
                let size = start + u64::try_from(extra).expect("fits");
                let got = full.frontier_at(size).expect("frontier present");
                let reference = sequential_frontier(&all[..usize::try_from(size).expect("fits")]);
                prop_assert_eq!(&got, reference.value().expect("non-empty"));
            }
        }

        #[test]
        fn orchard_combine_matches_sequential_append(
            batch_count in 1usize..50,
            raw_splits in prop::collection::vec(any::<prop::sample::Index>(), 0..4),
        ) {
            let batch: Vec<MerkleHashOrchard> = (0..batch_count)
                .map(|i| orchard_leaf_of(0xC000 + u64::try_from(i).expect("fits")))
                .collect();
            let mut splits: Vec<usize> =
                raw_splits.iter().map(|ix| 1 + ix.index(batch_count)).collect();
            splits.retain(|&s| s < batch_count);
            splits.sort_unstable();
            splits.dedup();

            let segment = lift_and_combine(&batch, 0, &splits);
            for size in 1..=batch_count {
                let got = segment.frontier_at(u64::try_from(size).expect("fits"))
                    .expect("frontier present");
                let reference = sequential_frontier(&batch[..size]);
                prop_assert_eq!(&got, reference.value().expect("non-empty"));
            }
        }
    }

    // Step 1(b): associativity holds node-for-node, and non-commutativity is
    // witnessed.
    #[test]
    fn combine_is_associative_node_for_node() {
        let leaves: Vec<SaplingNode> = (0..11).map(sapling_leaf_of).collect();
        let a = TreeSegment::lift(&leaves[0..3], 0);
        let b = TreeSegment::lift(&leaves[3..7], 3);
        let c = TreeSegment::lift(&leaves[7..11], 7);

        let left = TreeSegment::combine(TreeSegment::combine(a.clone(), b.clone()), c.clone());
        let right = TreeSegment::combine(a, TreeSegment::combine(b, c));
        assert_eq!(left.nodes, right.nodes, "combine must be associative");
    }

    #[test]
    fn combine_is_not_commutative() {
        let l0 = sapling_leaf_of(1);
        let l1 = sapling_leaf_of(2);
        // a ⊕ b over leaves [l0, l1]; b ⊕ a over [l1, l0]. The order-sensitive
        // Merkle hash makes the roots differ.
        let ab = TreeSegment::combine(
            TreeSegment::lift(std::slice::from_ref(&l0), 0),
            TreeSegment::lift(std::slice::from_ref(&l1), 1),
        );
        let ba = TreeSegment::combine(
            TreeSegment::lift(std::slice::from_ref(&l1), 0),
            TreeSegment::lift(std::slice::from_ref(&l0), 1),
        );
        assert_ne!(
            ab.node_at(1, 0),
            ba.node_at(1, 0),
            "order-sensitive hash must make a⊕b differ from b⊕a"
        );
    }

    #[test]
    fn identity_is_a_unit() {
        let leaves: Vec<SaplingNode> = (0..5).map(sapling_leaf_of).collect();
        let a = TreeSegment::lift(&leaves, 0);
        let left = TreeSegment::combine(TreeSegment::empty(), a.clone());
        let right = TreeSegment::combine(a.clone(), TreeSegment::empty());
        assert_eq!(left.nodes, a.nodes);
        assert_eq!(right.nodes, a.nodes);
    }

    // Review focus #5: a spam-era batch (500k leaves) completes and stays within
    // the 2·leaves node bound; the seam combine does not fall back to a rescan.
    #[test]
    fn large_batch_completes_within_memory_bound() {
        let total = 500_000usize;
        let leaves: Vec<MerkleHashOrchard> = (0..total)
            .map(|i| orchard_leaf_of(u64::try_from(i).expect("fits")))
            .collect();
        // 50 runs of 10,000 leaves, reduced in chain order.
        let mut segment = TreeSegment::empty();
        for chunk_start in (0..total).step_by(10_000) {
            let end = (chunk_start + 10_000).min(total);
            let run = TreeSegment::lift(
                &leaves[chunk_start..end],
                u64::try_from(chunk_start).expect("fits"),
            );
            segment = TreeSegment::combine(segment, run);
        }
        assert_eq!(segment.len(), u64::try_from(total).expect("fits"));
        assert!(
            segment.node_count() <= 2 * total,
            "{} nodes exceeds the 2·leaves bound",
            segment.node_count()
        );
        let tip = segment
            .frontier_at(u64::try_from(total).expect("fits"))
            .expect("tip frontier present");
        assert_eq!(
            u64::from(tip.position()) + 1,
            u64::try_from(total).expect("fits")
        );
    }
}
