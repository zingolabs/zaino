//! Type aliases for fields not yet promoted to newtypes.
//!
//! Each alias is a grep target: when misuse surfaces, promote
//! the alias to a newtype with private inner + constructor.

/// Output index within a transaction.
pub type OutputIndex = u32;

/// Number of confirmations (depth from tip). Ephemeral, query-time only.
pub type Confirmations = i64;

/// Difficulty target. Protocol-specific float representation.
pub type Difficulty = f64;

/// Cumulative number of notes in a commitment tree.
pub type TreeSize = u64;

/// Subtree index within a shielded pool's commitment tree.
pub type SubtreeIndex = u16;
