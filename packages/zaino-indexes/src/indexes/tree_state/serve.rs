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
//! # Activation
//!
//! A pool is reported ([`Some`]) from its activation height on — including the
//! blocks between activation and its first note, where the tree is active but
//! empty and must serve the serialised empty tree, not absence. Below its
//! activation (or on a network where the pool is unscheduled) it is [`None`] (the
//! `""` a wallet reads as "pool not here"; pepper-sync rejects `""` for an active
//! pool, so this boundary must be exact). A tree size alone cannot tell the two
//! apart — both are size 0 — so the decision takes the per-pool activation
//! height from [`PoolActivations`], which Zaino learns from the validator's
//! reported upgrade schedule at boot rather than compiling one in.

use incrementalmerkletree::frontier::Frontier;
use incrementalmerkletree::Hashable;
use orchard::tree::MerkleHashOrchard;
use sapling_crypto::Node as SaplingNode;
use zaino_persistence_codec::DecodeError;
use zaino_primitives::types::{
    BlockHash, Height, PoolActivations, PoolTreestate, ShieldedPool, TreeRoot, Treestate,
};
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

/// Render one pool's frontier as a domain [`PoolTreestate`], or [`None`] when the
/// pool is not active at `at` (below its `activation` height, or unscheduled).
///
/// An active pool is reported even when its tree is empty: the empty frontier
/// renders to the serialised empty tree and the empty-tree root, which is what
/// `z_gettreestate` carries from a pool's activation height onward. The
/// `final_root` is the frontier's root (padded with empty subtrees to the pool
/// depth) in internal byte order; the `final_state` is zcashd's legacy
/// `CommitmentTree` serialization.
pub fn pool_treestate<H: HashSer + Hashable + Clone>(
    frontier: &Frontier<H, DEPTH>,
    activation: Option<Height>,
    at: Height,
) -> Option<PoolTreestate> {
    // Absent (unscheduled) or below its activation height: the pool is not here.
    if activation.is_none_or(|activation| at < activation) {
        return None;
    }
    Some(PoolTreestate {
        final_root: Some(TreeRoot::from(node_bytes(&frontier.root()))),
        final_state: legacy_tree_bytes(frontier),
    })
}

impl TreeStateValue {
    /// The three pools' domain treestates at height `at`, Sapling / Orchard /
    /// Ironwood, each rendered through [`pool_treestate`] against `activations`.
    pub fn pool_treestates(
        &self,
        activations: &PoolActivations,
        at: Height,
    ) -> (
        Option<PoolTreestate>,
        Option<PoolTreestate>,
        Option<PoolTreestate>,
    ) {
        (
            pool_treestate(&self.sapling, activations.sapling, at),
            pool_treestate(&self.orchard, activations.orchard, at),
            pool_treestate(&self.ironwood, activations.ironwood, at),
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
    /// The hash of the window block that completed it, in internal (unreversed)
    /// byte order.
    pub completing_block_hash: BlockHash,
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
                completing_block_hash: ctx.hash,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexes::tree_state::pools::{orchard_leaf, sapling_leaf};
    use incrementalmerkletree::frontier::Frontier;

    fn h(height: u32) -> Height {
        Height::try_from(height).expect("valid height")
    }

    /// A canonical leaf from a seed in the low 8 bytes.
    fn sapling_note(seed: u64) -> SaplingNode {
        let mut bytes = [0u8; 32];
        bytes[..8].copy_from_slice(&seed.to_le_bytes());
        sapling_leaf(bytes).expect("canonical sapling cmu")
    }
    fn orchard_note(seed: u64) -> MerkleHashOrchard {
        let mut bytes = [0u8; 32];
        bytes[..8].copy_from_slice(&seed.to_le_bytes());
        orchard_leaf(bytes).expect("canonical orchard cmx")
    }

    fn root_bytes(pool: &Option<PoolTreestate>) -> [u8; 32] {
        <[u8; 32]>::from(
            pool.as_ref()
                .expect("active pool")
                .final_root
                .expect("root present"),
        )
    }

    // Step 1(a): the activation boundary against zebra's z_gettreestate fixture
    // (packages/zaino-indexes/tests/fixtures/treestate/zebra-mainnet.json).
    //
    // Sapling activates at mainnet height 419200, with an empty tree (its first
    // note lands at 419201): below → absent, at activation → the serialised empty
    // tree and the empty-tree root, first note → a non-empty tree, still present.
    #[test]
    fn sapling_activation_boundary_matches_zebra() {
        let activation = Some(h(419_200));
        let empty = Frontier::<SaplingNode, DEPTH>::empty();

        // activation − 1: absent (the wire `""`).
        assert_eq!(pool_treestate(&empty, activation, h(419_199)), None);

        // activation: active but empty — finalState `000000`, the empty-tree root.
        let at = pool_treestate(&empty, activation, h(419_200)).expect("active at activation");
        assert_eq!(
            at.final_state,
            hex::decode("000000").expect("hex"),
            "the empty Sapling tree serialises to zebra's `000000`",
        );
        // z_gettreestate's sapling finalRoot is display (reversed) order; the
        // domain value is internal order, so it is the fixture reversed.
        let mut fixture =
            hex::decode("3e49b5f954aa9d3545bc6c37744661eea48d7c34e3000d82b7f0010c30f4c2fb")
                .expect("hex");
        fixture.reverse();
        assert_eq!(
            root_bytes(&Some(at)).to_vec(),
            fixture,
            "empty Sapling root"
        );

        // first note (419201): non-empty, still present.
        let mut one = empty.clone();
        assert!(one.append(sapling_note(1)));
        let first = pool_treestate(&one, activation, h(419_201)).expect("active");
        assert_ne!(
            first.final_state,
            hex::decode("000000").expect("hex"),
            "a funded Sapling tree is not the empty encoding",
        );
    }

    // Orchard activates at mainnet height 1687104 (NU5), also with an empty tree.
    // Its z_gettreestate finalRoot is not byte-reversed, so the domain (internal)
    // root equals the fixture directly.
    #[test]
    fn orchard_activation_boundary_matches_zebra() {
        let activation = Some(h(1_687_104));
        let empty = Frontier::<MerkleHashOrchard, DEPTH>::empty();

        assert_eq!(pool_treestate(&empty, activation, h(1_687_103)), None);

        let at = pool_treestate(&empty, activation, h(1_687_104)).expect("active at activation");
        assert_eq!(at.final_state, hex::decode("000000").expect("hex"));
        let fixture =
            hex::decode("ae2935f1dfd8a24aed7c70df7de3a668eb7a49b1319880dde2bbd9031ae5d82f")
                .expect("hex");
        assert_eq!(
            root_bytes(&Some(at)).to_vec(),
            fixture,
            "empty Orchard root"
        );

        let mut one = empty.clone();
        assert!(one.append(orchard_note(1)));
        let first = pool_treestate(&one, activation, h(1_687_105)).expect("active");
        assert_ne!(first.final_state, hex::decode("000000").expect("hex"));
    }

    // An unscheduled pool (regtest without NU6.3, say) is never reported, even
    // with notes present — there is no activation to be at or above.
    #[test]
    fn an_unscheduled_pool_is_absent() {
        let empty = Frontier::<MerkleHashOrchard, DEPTH>::empty();
        assert_eq!(pool_treestate(&empty, None, h(9_000_000)), None);
    }
}
