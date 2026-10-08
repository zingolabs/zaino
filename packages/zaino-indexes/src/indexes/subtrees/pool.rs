//! The per-pool parameter of the generic [`SubtreesIndex`](super::SubtreesIndex).
//!
//! A subtree-roots index is identical across the three shielded pools apart from
//! the leaf hash, which of a block's note commitments feed it, and which
//! `tree_state` frontier it reads. [`Pool`] captures exactly that difference, so
//! one generic `SubtreesIndex<P>` serves all three ([`SaplingPool`],
//! [`OrchardPool`], [`IronwoodPool`]) with no duplicated extraction logic.

use incrementalmerkletree::frontier::Frontier;
use incrementalmerkletree::Hashable;
use orchard::tree::MerkleHashOrchard;
use sapling_crypto::Node as SaplingNode;
use zaino_primitives::types::NoteCommitment;
use zaino_sync::primitives::IndexId;

use crate::indexes::tree_state::pools::{ironwood_leaf, orchard_leaf, sapling_leaf};
use crate::indexes::tree_state::segment::DEPTH;
use crate::indexes::tree_state::{TreeStateCtx, TreeStateValue};

/// The note-commitment-tree subtree level: a completed subtree is the perfect
/// subtree of `2^16` consecutive leaves, so its root is a level-16 node. This is
/// the `GetSubtreeRoots` / `z_getsubtreesbyindex` unit fixed by the Zcash
/// protocol. A [`Pool`] may lower it (see [`Pool::SUBTREE_LEVEL`]) only in tests,
/// where `2^16` leaves are impractical to synthesise.
pub const SUBTREE_LEVEL: u8 = 16;

/// What distinguishes one pool's subtree-roots index from another's: its leaf
/// hash, its index identity and compact dependency, which of a block's
/// commitments it consumes, and how to read its frontier from a
/// [`TreeStateValue`].
pub trait Pool: Send + Sync + 'static {
    /// The pool's Merkle-tree leaf/node hash.
    type Leaf: Hashable + Clone + Send + Sync;

    /// This pool's subtree-roots index namespace (e.g. `subtrees_sapling`).
    const NAME: IndexId;

    /// The compact index that carries this pool's per-block commitments — a
    /// declared dependency so the subtree index extracts only after it has
    /// committed the batch (the subtree root domain-depends on the block's
    /// leaves).
    const COMPACT: IndexId;

    /// The pool's human name, for error messages.
    const POOL: &'static str;

    /// The subtree level. Const 16 in production ([`SUBTREE_LEVEL`]); a test pool
    /// lowers it so completions can be exercised with a handful of leaves.
    const SUBTREE_LEVEL: u8 = SUBTREE_LEVEL;

    /// This block's note commitments for the pool, in chain order.
    fn commitments(ctx: &TreeStateCtx) -> &[NoteCommitment];

    /// Convert a note commitment's bytes to the pool's leaf hash, or `None` for a
    /// non-canonical field encoding.
    fn leaf(bytes: [u8; 32]) -> Option<Self::Leaf>;

    /// The pool's frontier within a height's [`TreeStateValue`].
    fn frontier(value: &TreeStateValue) -> &Frontier<Self::Leaf, DEPTH>;

    /// A node's 32 bytes in internal (unreversed) order — the orientation subtree
    /// roots are stored and served in, for every pool.
    fn root_bytes(node: &Self::Leaf) -> [u8; 32];
}

/// The Sapling subtree-roots pool.
pub struct SaplingPool;

impl Pool for SaplingPool {
    type Leaf = SaplingNode;

    const NAME: IndexId = IndexId::new("subtrees_sapling");
    const COMPACT: IndexId = crate::indexes::sapling::ID;
    const POOL: &'static str = "sapling";

    fn commitments(ctx: &TreeStateCtx) -> &[NoteCommitment] {
        &ctx.sapling_cmus
    }

    fn leaf(bytes: [u8; 32]) -> Option<SaplingNode> {
        sapling_leaf(bytes)
    }

    fn frontier(value: &TreeStateValue) -> &Frontier<SaplingNode, DEPTH> {
        &value.sapling
    }

    fn root_bytes(node: &SaplingNode) -> [u8; 32] {
        node.to_bytes()
    }
}

/// The Orchard subtree-roots pool.
pub struct OrchardPool;

impl Pool for OrchardPool {
    type Leaf = MerkleHashOrchard;

    const NAME: IndexId = IndexId::new("subtrees_orchard");
    const COMPACT: IndexId = crate::indexes::orchard::ID;
    const POOL: &'static str = "orchard";

    fn commitments(ctx: &TreeStateCtx) -> &[NoteCommitment] {
        &ctx.orchard_cmxs
    }

    fn leaf(bytes: [u8; 32]) -> Option<MerkleHashOrchard> {
        orchard_leaf(bytes)
    }

    fn frontier(value: &TreeStateValue) -> &Frontier<MerkleHashOrchard, DEPTH> {
        &value.orchard
    }

    fn root_bytes(node: &MerkleHashOrchard) -> [u8; 32] {
        node.to_bytes()
    }
}

/// The Ironwood subtree-roots pool (shares Orchard's Pallas leaf encoding, its
/// own separate tree).
pub struct IronwoodPool;

impl Pool for IronwoodPool {
    type Leaf = MerkleHashOrchard;

    const NAME: IndexId = IndexId::new("subtrees_ironwood");
    const COMPACT: IndexId = crate::indexes::ironwood::ID;
    const POOL: &'static str = "ironwood";

    fn commitments(ctx: &TreeStateCtx) -> &[NoteCommitment] {
        &ctx.ironwood_cmxs
    }

    fn leaf(bytes: [u8; 32]) -> Option<MerkleHashOrchard> {
        ironwood_leaf(bytes)
    }

    fn frontier(value: &TreeStateValue) -> &Frontier<MerkleHashOrchard, DEPTH> {
        &value.ironwood
    }

    fn root_bytes(node: &MerkleHashOrchard) -> [u8; 32] {
        node.to_bytes()
    }
}
