//! `tree_state`: the commitment-tree (treestate) index's domain layer.
//!
//! Three parts, pool-agnostic where it can be:
//!
//! - [`segment`] — the [`TreeSegment`] ordered-monoid algebra over a contiguous
//!   run of note-commitment leaves (lift, combine, per-height frontier lookup),
//!   generic over a pool's Merkle hash.
//! - [`pools`] — the per-pool leaf conversions (Sapling `cmu`, Orchard /
//!   Ironwood `cmx`) from commitment bytes to the tree's leaf hash.
//! - [`codec`] — the on-disk frontier record ([`PersistentTreeStateValue`]) and
//!   zcashd's legacy `CommitmentTree` wire encoding ([`legacy_tree_bytes`]) used
//!   by `z_gettreestate`'s `finalState`.
//!
//! The sync-engine wiring (the `SelfCumulative<OrderedMonoid>` × Append index
//! that drives this algebra during a build) lands in a later task.
//!
//! [`PersistentTreeStateValue`]: codec::PersistentTreeStateValue
//! [`legacy_tree_bytes`]: codec::legacy_tree_bytes

pub mod codec;
pub mod pools;
pub mod segment;

pub use codec::{legacy_tree_bytes, legacy_tree_from_bytes, TreeStateValue};
pub use pools::{ironwood_leaf, orchard_leaf, sapling_leaf};
pub use segment::TreeSegment;
