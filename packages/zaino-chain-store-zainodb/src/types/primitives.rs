//! Foundational primitive types for the chain index.
//!
//! Business-layer primitives that are *not* persisted directly. DB-serializable
//! primitives (the ones that implement `ZainoVersionedSerde`) live under
//! `types/db/` — this module is reserved for types whose role is purely
//! in-memory / business-logic vocabulary.

mod block_index;
mod chain_work;
mod compact_difficulty;

pub use block_index::BlockIndex;
pub use chain_work::{ChainWork, ChainWorkError};
pub use compact_difficulty::{CompactDifficulty, CompactDifficultyError};
