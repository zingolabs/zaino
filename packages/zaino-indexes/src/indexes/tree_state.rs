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
//! - [`sync`] — the sync-engine wiring: [`TreeStateIndex`] as the
//!   `SelfCumulative<OrderedMonoid>` × Append index that drives the segment
//!   algebra over the three pools during a build.
//!
//! [`PersistentTreeStateValue`]: codec::PersistentTreeStateValue
//! [`legacy_tree_bytes`]: codec::legacy_tree_bytes
//! [`TreeStateIndex`]: codec::TreeStateIndex

pub mod codec;
pub mod pools;
pub mod segment;
pub mod sync;

pub use codec::{legacy_tree_bytes, legacy_tree_from_bytes, TreeStateIndex, TreeStateValue};
pub use pools::{ironwood_leaf, orchard_leaf, sapling_leaf};
pub use segment::TreeSegment;
pub use sync::{TreeStateCtx, TreeStateEntry};
