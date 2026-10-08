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
use zaino_primitives::types::{PoolTreestate, TreeRoot};
use zcash_primitives::merkle_tree::HashSer;

use super::codec::{legacy_tree_bytes, TreeStateValue};
use super::segment::DEPTH;

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
