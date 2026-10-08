//! `subtrees_{sapling,orchard,ironwood}`: the per-pool subtree-roots indexes
//! backing `GetSubtreeRoots` / `z_getsubtreesbyindex`.
//!
//! Each completed subtree is the perfect subtree of `2^16` consecutive
//! note-commitment leaves; its root is a level-16 node, keyed by subtree index.
//! A subtree's root domain-depends on the commitment tree just before the block
//! that completes it *and* that block's own leaves, so this is a **cross index**
//! ([`CrossIndex`]): `root(k) = f(tree_state frontier at h−1, block h's leaves)`,
//! where `h` is the completing block. It declares `tree_state` and the pool's
//! compact index as [dependencies](IndexDef::DEPENDENCIES) and reads the former
//! through the [`DepsReader`]; composition is [`Append`], one disjoint entry per
//! completed subtree.
//!
//! One [`SubtreesIndex<P>`] serves all three pools through the [`Pool`] trait, so
//! keys stay walk-ordered (one namespace per pool) with no duplicated logic.
//!
//! # Cost
//!
//! Only the ~1,900 mainnet blocks that complete a subtree do any work, and each
//! does just this block's re-lift onto the carried frontier plus one frontier
//! read per completed subtree. That is negligible beside the main `tree_state`
//! fold — the overwhelming majority of blocks complete nothing and emit an empty
//! delta.

pub mod codec;
pub mod pool;

use std::marker::PhantomData;

use incrementalmerkletree::frontier::{Frontier, NonEmptyFrontier};
use incrementalmerkletree::{Hashable, Level};
use zaino_primitives::types::NoteCommitment;
use zaino_sync::descriptor::{Append, CrossIndex};
use zaino_sync::primitives::{BlockHeight, IndexId};
use zaino_sync::traits::{DepsReadError, DepsReader, ExtractCross, IndexDef, MergeAppend, Schema};

use crate::indexes::tree_state::codec::TreeStateIndex;
use crate::indexes::tree_state::segment::{TreeSegment, DEPTH};
use crate::indexes::tree_state::TreeStateCtx;

use self::codec::SubtreeRoot;
use self::pool::Pool;

pub use self::pool::{IronwoodPool, OrchardPool, SaplingPool};

/// The Sapling subtree-roots index (`subtrees_sapling`).
pub type SaplingSubtreesIndex = SubtreesIndex<SaplingPool>;
/// The Orchard subtree-roots index (`subtrees_orchard`).
pub type OrchardSubtreesIndex = SubtreesIndex<OrchardPool>;
/// The Ironwood subtree-roots index (`subtrees_ironwood`).
pub type IronwoodSubtreesIndex = SubtreesIndex<IronwoodPool>;

/// The subtree-roots index for pool `P`: subtree index → [`SubtreeRoot`].
///
/// A [`CrossIndex`] over `tree_state` and the pool's compact index; see the
/// module docs.
pub struct SubtreesIndex<P>(PhantomData<P>);

/// One completed subtree: its index and root.
pub struct SubtreeEntry {
    /// The subtree's index (its key).
    pub index: u32,
    /// The subtree's root and completing height.
    pub root: SubtreeRoot,
}

/// Why extracting a block's subtree roots failed.
#[derive(Debug, thiserror::Error)]
pub enum SubtreesError {
    /// Reading the `tree_state` dependency failed.
    #[error("reading the tree_state dependency")]
    Deps(#[source] DepsReadError),
    /// `tree_state` had no entry at `h−1`, though the pipelined gate opens only
    /// after `tree_state` has committed this batch and every earlier one — so a
    /// missing predecessor is a definition/engine bug, not a data condition.
    #[error("tree_state has no entry at height {height}")]
    MissingTreeState {
        /// The predecessor height that had no `tree_state` entry.
        height: BlockHeight,
    },
    /// A note commitment's 32 bytes were not a canonical field element for the
    /// pool. A canonical source never produces it (and `tree_state` would have
    /// already rejected the block), so this is the wire→domain validation step.
    #[error("non-canonical {pool} note commitment")]
    NonCanonicalCommitment {
        /// The pool whose commitment failed to decode.
        pool: &'static str,
    },
    /// A subtree index exceeded `u32`. Unreachable on any real chain (mainnet has
    /// ~1,900 subtrees), but a `u32` key cannot silently truncate it.
    #[error("subtree index {index} exceeds u32")]
    SubtreeIndexOverflow {
        /// The offending subtree index.
        index: u64,
    },
}

impl From<DepsReadError> for SubtreesError {
    fn from(err: DepsReadError) -> Self {
        Self::Deps(err)
    }
}

impl<P: Pool> IndexDef for SubtreesIndex<P> {
    type Scope = CrossIndex;
    type Composition = Append;
    type Delta = Vec<SubtreeEntry>;
    type BlockContext = TreeStateCtx;

    const NAME: IndexId = P::NAME;
    // `tree_state` supplies the frontier at h−1; the pool's compact index supplies
    // the block's leaves (projected into the context), so both gate this index.
    const DEPENDENCIES: &'static [IndexId] = &[crate::indexes::tree_state::codec::ID, P::COMPACT];
}

impl<P: Pool> ExtractCross for SubtreesIndex<P> {
    type Error = SubtreesError;

    fn extract(
        ctx: &TreeStateCtx,
        deps: &DepsReader<'_>,
    ) -> Result<Vec<SubtreeEntry>, SubtreesError> {
        let height = u64::from(ctx.height);

        // The commitment-tree frontier just before this block. At genesis there
        // is no h−1 and the pool's tree is empty; otherwise it is `tree_state`'s
        // entry at h−1, which the pipelined gate guarantees is readable (committed
        // in an earlier batch, or persisted into this batch's pending commit).
        let before: Frontier<P::Leaf, DEPTH> = if height == 0 {
            Frontier::empty()
        } else {
            let predecessor = BlockHeight::new(height - 1);
            let prior = deps.get::<TreeStateIndex>(&predecessor)?.ok_or(
                SubtreesError::MissingTreeState {
                    height: predecessor,
                },
            )?;
            P::frontier(&prior).clone()
        };

        let leaves = convert_leaves::<P>(P::commitments(ctx))?;

        completed_subtrees(&before, &leaves, P::SUBTREE_LEVEL)
            .into_iter()
            .map(|(subtree, node)| {
                let index = u32::try_from(subtree)
                    .map_err(|_| SubtreesError::SubtreeIndexOverflow { index: subtree })?;
                Ok(SubtreeEntry {
                    index,
                    root: SubtreeRoot {
                        root: P::root_bytes(&node),
                        completing_height: ctx.height,
                    },
                })
            })
            .collect()
    }
}

impl<P: Pool> MergeAppend for SubtreesIndex<P> {}

impl<P: Pool> Schema<Vec<Vec<SubtreeEntry>>> for SubtreesIndex<P> {
    fn into_entries(merged: Vec<Vec<SubtreeEntry>>) -> Vec<(u32, SubtreeRoot)> {
        // Each block contributes 0+ entries, already in ascending-index order;
        // flattening block-by-block keeps the whole sequence ascending.
        merged
            .into_iter()
            .flatten()
            .map(|entry| (entry.index, entry.root))
            .collect()
    }

    fn from_entries(entries: Vec<(u32, SubtreeRoot)>) -> Vec<Vec<SubtreeEntry>> {
        // The mechanical inverse of `into_entries`: one singleton block per entry
        // re-flattens to the same sequence. (Append never reconstructs deltas in
        // the cross bridge; this exists only to satisfy the `Schema` contract.)
        entries
            .into_iter()
            .map(|(index, root)| vec![SubtreeEntry { index, root }])
            .collect()
    }
}

/// Convert a pool's note commitments to its leaves, failing on the first
/// non-canonical encoding.
fn convert_leaves<P: Pool>(commitments: &[NoteCommitment]) -> Result<Vec<P::Leaf>, SubtreesError> {
    commitments
        .iter()
        .map(|commitment| {
            P::leaf((*commitment).into())
                .ok_or(SubtreesError::NonCanonicalCommitment { pool: P::POOL })
        })
        .collect()
}

/// The subtrees a block completes, as `(subtree index, root node)` in ascending
/// index order, given the pool's frontier `before` the block and the block's
/// `leaves`, at subtree `level`.
///
/// A subtree `k` completes when the tree first reaches `(k+1)·2^level` leaves, so
/// the completing block is the one whose leaves carry the size across that
/// boundary. For each such boundary the root is read from the frontier at the
/// exact completion size — reconstructed by appending this block's leaves onto
/// the carried frontier through the [`TreeSegment`] algebra — and folded up to
/// `level`, which reconstructs the straddling subtree even when its left half
/// lies in the carried (pre-block) state.
fn completed_subtrees<H: Hashable + Clone + Send + Sync>(
    before: &Frontier<H, DEPTH>,
    leaves: &[H],
    level: u8,
) -> Vec<(u64, H)> {
    let size_before = before.tree_size();
    let added = u64::try_from(leaves.len()).expect("leaf count fits u64");
    let size_after = size_before
        .checked_add(added)
        .expect("a pool's tree size fits u64");
    let span = 1u64 << u32::from(level);

    // Completion boundaries in `(size_before, size_after]` are at `m·span` for
    // `m` in `first..=last`; subtree index is `m − 1`.
    let first = size_before / span + 1;
    let last = size_after / span;
    if first > last {
        return Vec::new();
    }

    // Append this block's leaves onto the carried frontier once; every
    // completion then reads the frontier at its exact size as a pure lookup.
    let combined = TreeSegment::combine(
        TreeSegment::carry_segment(before),
        TreeSegment::lift(leaves, size_before),
    );

    (first..=last)
        .map(|m| {
            let completion_size = m * span;
            let frontier = combined
                .frontier_at(completion_size)
                .expect("a completed subtree's completion frontier was retained by lift/combine");
            (m - 1, subtree_root(&frontier, level))
        })
        .collect()
}

/// The root of the perfect subtree whose last leaf is `frontier`'s tip: fold the
/// tip leaf with the left ommer at each level `0..level`.
///
/// At a completion size `(k+1)·2^level` the frontier's position has its low
/// `level` bits all set, so the first `level` ommers are the left siblings at
/// levels `0..level`; folding the tip up through them yields the level-`level`
/// node at index `k` — the subtree root — regardless of the subtree's parity.
fn subtree_root<H: Hashable + Clone>(frontier: &NonEmptyFrontier<H>, level: u8) -> H {
    let mut acc = frontier.leaf().clone();
    let mut ommers = frontier.ommers().iter();
    for ommer_level in 0..level {
        let ommer = ommers.next().expect(
            "a completed subtree's frontier has a left ommer at every level below the subtree level",
        );
        acc = H::combine(Level::from(ommer_level), ommer, &acc);
    }
    acc
}

#[cfg(test)]
mod tests {
    use super::*;
    use incrementalmerkletree::frontier::Frontier;
    use sapling_crypto::Node as SaplingNode;

    use crate::indexes::tree_state::pools::sapling_leaf;

    /// A canonical Sapling leaf from a seed in the low 8 bytes.
    fn leaf(seed: u64) -> SaplingNode {
        let mut bytes = [0u8; 32];
        bytes[..8].copy_from_slice(&seed.to_le_bytes());
        sapling_leaf(bytes).expect("canonical sapling cmu")
    }

    /// An independent reference: the Merkle root of exactly `2^level` leaves,
    /// hashed as a balanced binary tree with `incrementalmerkletree`'s own
    /// `Hashable::combine`. A completed subtree is perfect (all its leaves
    /// present), so this is its root.
    fn perfect_root(leaves: &[SaplingNode], level: u8) -> SaplingNode {
        assert_eq!(leaves.len(), 1usize << level, "a perfect subtree is full");
        let mut nodes = leaves.to_vec();
        for l in 0..level {
            nodes = nodes
                .chunks_exact(2)
                .map(|pair| SaplingNode::combine(Level::from(l), &pair[0], &pair[1]))
                .collect();
        }
        nodes.into_iter().next().expect("one root remains")
    }

    /// A frontier after appending `leaves` sequentially from empty — the carried
    /// pre-block state.
    fn frontier_of(leaves: &[SaplingNode]) -> Frontier<SaplingNode, DEPTH> {
        let mut frontier = Frontier::empty();
        for leaf in leaves {
            assert!(frontier.append(*leaf), "append within depth");
        }
        frontier
    }

    /// Drive `completed_subtrees` for a carry of `prior` leaves and a block of
    /// `block` leaves at `level`, and check each emitted root against the
    /// independent `perfect_root` over the corresponding leaf slice.
    fn check(prior: u64, block: u64, level: u8) -> Vec<u64> {
        let all: Vec<SaplingNode> = (0..prior + block).map(leaf).collect();
        let before = frontier_of(&all[..usize::try_from(prior).expect("fits")]);
        let block_leaves = &all[usize::try_from(prior).expect("fits")..];

        let completed = completed_subtrees(&before, block_leaves, level);
        let span = 1u64 << level;
        for (k, root) in &completed {
            let start = usize::try_from(k * span).expect("fits");
            let end = usize::try_from((k + 1) * span).expect("fits");
            assert_eq!(
                root,
                &perfect_root(&all[start..end], level),
                "subtree {k} root must equal the sequential tree's level-{level} node",
            );
        }
        completed.into_iter().map(|(k, _)| k).collect()
    }

    // Step 1(a), level 2 (span 4): a subtree completing mid-block, at a block
    // boundary, and two in one block; each emitted root equals the independent
    // perfect-subtree computation.

    #[test]
    fn completes_nothing_below_the_first_boundary() {
        // 3 leaves, span 4: no subtree completes.
        assert_eq!(check(0, 3, 2), Vec::<u64>::new());
    }

    #[test]
    fn completes_one_subtree_mid_block() {
        // Empty carry, 5 leaves: subtree 0 completes at size 4, mid-block.
        assert_eq!(check(0, 5, 2), vec![0]);
    }

    #[test]
    fn completes_one_subtree_at_a_block_boundary() {
        // Empty carry, exactly 4 leaves: subtree 0 completes at the block's end.
        assert_eq!(check(0, 4, 2), vec![0]);
    }

    #[test]
    fn completes_two_subtrees_in_one_block() {
        // Carry 3 leaves, block adds 6 (size 3 → 9): subtrees 0 (size 4) and 1
        // (size 8) both complete in this block, their left halves straddling the
        // carry boundary.
        assert_eq!(check(3, 6, 2), vec![0, 1]);
    }

    #[test]
    fn completes_a_subtree_whose_boundary_falls_on_the_carry() {
        // Carry exactly at a boundary (4 leaves), block completes the next.
        assert_eq!(check(4, 5, 2), vec![1]);
    }

    #[test]
    fn empty_block_completes_nothing() {
        assert_eq!(check(6, 0, 2), Vec::<u64>::new());
    }

    // A deeper level across a longer run: every boundary in a 40-leaf chain is
    // reported exactly once, with the right root.
    #[test]
    fn reports_every_boundary_once_over_a_long_run() {
        // span 8, size 5 → 40: boundaries 8,16,24,32,40 (the last lands on the
        // block's final leaf), so subtrees 0..=4 complete.
        let indices = check(5, 35, 3);
        assert_eq!(indices, vec![0, 1, 2, 3, 4]);
    }
}
