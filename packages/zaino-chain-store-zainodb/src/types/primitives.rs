//! Foundational primitive types for the chain index.
//!
//! Business-layer primitives that are *not* persisted directly. DB-serializable
//! primitives (the ones that implement `ZainoVersionedSerde`) live under
//! `types/db/` — this module is reserved for types whose role is purely
//! in-memory / business-logic vocabulary.

mod block_index;

pub use block_index::BlockIndex;
// These are the vocabulary primitives: this store validates, folds and
// persists the same quantities every other layer reads, so there is nothing
// store-specific to add to them.
pub use zaino_primitives::types::{
    BlockWork, ChainWork, CompactDifficulty, CompactDifficultyError,
};
