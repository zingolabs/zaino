//! Foundational primitive types for the chain index.
//!
//! Business-layer primitives that are *not* persisted directly. DB-serializable
//! primitives (the ones that implement `ZainoVersionedSerde`) live under
//! `types/db/` — this module is reserved for types whose role is purely
//! in-memory / business-logic vocabulary.

mod block_index;
mod compact_difficulty;

pub use block_index::BlockIndex;
pub use compact_difficulty::{CompactDifficulty, CompactDifficultyError};
// The work family is the vocabulary primitive: this store folds and persists
// the same quantity every other layer compares, so there is nothing
// store-specific to add to it.
pub use zaino_primitives::types::{BlockWork, ChainWork};
