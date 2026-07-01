//! Foundational primitive types for the chain index.
//!
//! Business-layer primitives that are *not* persisted directly. DB-serializable
//! primitives (the ones that implement `ZainoVersionedSerde`) live under
//! `types/db/` — this module is reserved for types whose role is purely
//! in-memory / business-logic vocabulary.

use crate::chain_index::types::{BlockHash, Height};

/// The internal `(height, hash)` primitive that uniquely identifies a block.
///
/// Business-layer type. It is neither persisted nor serialized directly —
/// persistence goes through a database-adjacent helper
/// (`PersistentBlockContext` in `types/db/legacy.rs`), and the wire/gRPC
/// boundary converts via `From<proto::BlockId>` (the conversion is the
/// validation step).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BlockIndex {
    /// Height of the block.
    pub height: Height,
    /// Hash of the block.
    pub hash: BlockHash,
}
