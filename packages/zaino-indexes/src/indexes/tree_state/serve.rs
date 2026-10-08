//! Serve-time projection of a stored frontier onto the domain treestate.
//!
//! The finalised store and the non-finalised window both answer a treestate
//! read from a per-pool [`Frontier`] — the store reads one straight out of the
//! `tree_state` index, the window folds one forward from the finalised frontier
//! at the watermark. Both then render it the same way, so the projection lives
//! here once and both tiers call it. Sharing it is what makes the two sides of
//! the seam agree node-for-node: identical frontiers render to identical
//! [`PoolTreestate`]s.
//!
//! # Activation, as a note-presence proxy
//!
//! A pool is reported ([`Some`]) only when its tree holds at least one note at
//! the height; an empty tree renders as [`None`] (the `""` a wallet reads as
//! "pool not here"). Zcash reports a pool from its activation height on, even
//! across the handful of early blocks before its first note — the exact
//! boundary needs the validator's reported activation schedule
//! ([`BlockchainInfo::upgrades`](zaino_primitives::types::BlockchainInfo)), which
//! is not threaded into the local serve path. Note-presence is the proxy until
//! it is: it agrees with the schedule everywhere a note exists — every height a
//! wallet actually witnesses against — and differs only on pre-first-note blocks
//! of an active pool, which carry no shielded data to witness. Wiring the
//! reported schedule through to this projection is the remaining step for
//! byte-exactness at those boundary heights.

use incrementalmerkletree::frontier::Frontier;
use incrementalmerkletree::Hashable;
use orchard::tree::MerkleHashOrchard;
use sapling_crypto::Node as SaplingNode;
use zaino_persistence_codec::DecodeError;
use zaino_primitives::types::{PoolTreestate, ShieldedPool, TreeRoot, Treestate};
use zaino_sync::primitives::BlockHeight;
use zaino_sync::traits::ExtractCumulative;
use zcash_primitives::merkle_tree::HashSer;

use super::codec::{legacy_tree_bytes, legacy_tree_from_bytes, TreeStateIndex, TreeStateValue};
use super::segment::DEPTH;
use super::sync::{TreeStateCtx, TreeStateError};
use crate::indexes::subtrees::completed_subtrees;
use crate::indexes::subtrees::pool::{IronwoodPool, OrchardPool, Pool, SaplingPool};

/// The 32-byte internal-order serialization of a pool node hash.
///
/// `HashSer` is the pools' canonical (unreversed) encoding — the orientation
/// every root and frontier is stored and served in; the display reversal is the
/// wire adapter's business.
fn node_bytes<H: HashSer>(node: &H) -> [u8; 32] {
    let mut buf = Vec::with_capacity(32);
    node.write(&mut buf)
        .expect("writing a pool node hash to a Vec is infallible");
    buf.try_into()
        .expect("a pool node hash serializes to exactly 32 bytes")
}

/// Render one pool's frontier as a domain [`PoolTreestate`], or [`None`] when
/// the tree is empty (the note-presence activation proxy; see the module docs).
///
/// The `final_root` is the frontier's root (padded with empty subtrees to the
/// pool depth), in internal byte order; the `final_state` is zcashd's legacy
/// `CommitmentTree` serialization, the exact bytes `z_gettreestate` carries.
pub fn pool_treestate<H: HashSer + Hashable + Clone>(
    frontier: &Frontier<H, DEPTH>,
) -> Option<PoolTreestate> {
    if frontier.tree_size() == 0 {
        return None;
    }
    Some(PoolTreestate {
        final_root: Some(TreeRoot::from(node_bytes(&frontier.root()))),
        final_state: legacy_tree_bytes(frontier),
    })
}

impl TreeStateValue {
    /// The three pools' domain treestates, Sapling / Orchard / Ironwood, each
    /// rendered through [`pool_treestate`].
    pub fn pool_treestates(
        &self,
    ) -> (
        Option<PoolTreestate>,
        Option<PoolTreestate>,
        Option<PoolTreestate>,
    ) {
        (
            pool_treestate(&self.sapling),
            pool_treestate(&self.orchard),
            pool_treestate(&self.ironwood),
        )
    }
}

/// Reconstruct the finalised per-pool frontier from the `seed` treestate the
/// composer read at the watermark — the starting point a window fold builds on.
///
/// A pool absent from the seed (below activation, or active-but-empty, both the
/// `None` the store reports) starts from the empty frontier. The legacy
/// `finalState` bytes round-trip exactly to the frontier they were written from,
/// so the reconstructed value equals the one the store holds at the watermark.
pub fn seed_value(seed: Option<&Treestate>) -> Result<TreeStateValue, DecodeError> {
    fn pool<H: HashSer + Hashable + Clone>(
        pool: Option<&PoolTreestate>,
    ) -> Result<Frontier<H, DEPTH>, DecodeError> {
        match pool {
            Some(pool) => legacy_tree_from_bytes::<H>(&pool.final_state),
            None => Ok(Frontier::empty()),
        }
    }
    Ok(TreeStateValue {
        sapling: pool::<SaplingNode>(seed.and_then(|s| s.sapling.as_ref()))?,
        orchard: pool::<MerkleHashOrchard>(seed.and_then(|s| s.orchard.as_ref()))?,
        ironwood: pool::<MerkleHashOrchard>(seed.and_then(|s| s.ironwood.as_ref()))?,
    })
}

/// Fold `seed` — the finalised per-pool value at the watermark — forward over
/// `blocks` (each block's note commitments, in ascending contiguous height just
/// above the watermark) and return the per-pool value after the last block.
///
/// The non-finalised window holds no commitment tree of its own, so it answers a
/// treestate by re-walking its blocks onto the finalised frontier. This reuses
/// the `tree_state` index's own per-block [`extract`](ExtractCumulative::extract)
/// — the sequential ground truth — so a window value is byte-identical to the one
/// the store would hold once the same blocks finalise. In-window batches are
/// small, so the serial fold is fine.
pub fn fold_window(
    seed: &TreeStateValue,
    blocks: &[TreeStateCtx],
) -> Result<TreeStateValue, TreeStateError> {
    let mut carry = seed.clone();
    for ctx in blocks {
        carry = TreeStateIndex::extract(ctx, &carry)?.value;
    }
    Ok(carry)
}

/// One subtree a window block completes above the finalised frontier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowSubtree {
    /// The subtree's global index (shared numbering with the finalised tier).
    pub index: u32,
    /// The level-`SUBTREE_LEVEL` root, in internal (unreversed) byte order.
    pub root: [u8; 32],
    /// The window block that completed it.
    pub completing_height: BlockHeight,
}

/// The subtree roots `pool` completes across `blocks`, folding each onto the
/// finalised frontier in `seed` in ascending height.
///
/// A subtree completes at exactly one height, so a window completion has a
/// higher global index than every finalised one and the composer can simply
/// concatenate the two. Reuses the subtree index's own completion detection per
/// block, so a window root equals the one the store would store once finalised.
pub fn window_subtree_roots(
    seed: &TreeStateValue,
    blocks: &[TreeStateCtx],
    pool: ShieldedPool,
) -> Result<Vec<WindowSubtree>, TreeStateError> {
    match pool {
        ShieldedPool::Sapling => pool_window_roots::<SaplingPool>(seed, blocks),
        ShieldedPool::Orchard => pool_window_roots::<OrchardPool>(seed, blocks),
        ShieldedPool::Ironwood => pool_window_roots::<IronwoodPool>(seed, blocks),
    }
}

/// The per-pool body of [`window_subtree_roots`], generic over the pool.
fn pool_window_roots<P: Pool>(
    seed: &TreeStateValue,
    blocks: &[TreeStateCtx],
) -> Result<Vec<WindowSubtree>, TreeStateError> {
    let mut carry = seed.clone();
    let mut out = Vec::new();
    for ctx in blocks {
        let leaves = pool_leaves::<P>(ctx)?;
        for (index, node) in completed_subtrees(P::frontier(&carry), &leaves, P::SUBTREE_LEVEL) {
            out.push(WindowSubtree {
                // DEPTH bounds the tree to 2^(DEPTH-SUBTREE_LEVEL) subtrees, far
                // inside `u32`, so this conversion cannot truncate a real index.
                index: u32::try_from(index)
                    .expect("a subtree index fits u32 within the pool depth"),
                root: P::root_bytes(&node),
                completing_height: ctx.height,
            });
        }
        carry = TreeStateIndex::extract(ctx, &carry)?.value;
    }
    Ok(out)
}

/// A block's note commitments for pool `P`, decoded to leaves.
fn pool_leaves<P: Pool>(ctx: &TreeStateCtx) -> Result<Vec<P::Leaf>, TreeStateError> {
    P::commitments(ctx)
        .iter()
        .map(|commitment| {
            P::leaf((*commitment).into()).ok_or(TreeStateError::NonCanonicalCommitment {
                pool: pool_marker::<P>(),
            })
        })
        .collect()
}

/// The `sync::Pool` marker matching the subtree pool `P`, for error reporting.
fn pool_marker<P: Pool>() -> super::sync::Pool {
    match P::NAME {
        n if n == SaplingPool::NAME => super::sync::Pool::Sapling,
        n if n == OrchardPool::NAME => super::sync::Pool::Orchard,
        _ => super::sync::Pool::Ironwood,
    }
}
