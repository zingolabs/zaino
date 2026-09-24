//! Type definitions for the chain index.
//!
//! This module provides types for blockchain indexing, organized into two main categories:
//!
//! ## Database Types
//! Types that implement `ZainoVersionedSerde` for database persistence.
//! Any change to their encoding bumps the schema version, which rebuilds existing databases.
//!
//! Currently organized in `db/legacy.rs` (pending refactoring into focused modules):
//! - Block types: BlockHash, BlockIndex, BlockData, IndexedBlock, etc.
//! - Transaction types: TransactionHash, CompactTxData, TransparentCompactTx, etc.
//! - Address types: AddrScript, Outpoint, AddrHistRecord, etc.
//! - Shielded types: SaplingCompactTx, OrchardCompactTx, etc.
//! - Primitives: Height, AbsoluteChainWork, ShardIndex, etc.
//!
//! ## Helper Types
//! Non-database types for in-memory operations and conversions:
//! - BestChainLocation, NonBestChainLocation - Transaction location tracking
//! - TreeRootData - Commitment tree roots wrapper
//! - BlockMetadata, BlockWithMetadata - Block construction helpers
//!
//! ## Module Organization Rules
//!
//! **Database Types (`db` module):**
//! 1. Must implement `ZainoVersionedSerde`
//! 2. Never use external types as fields directly - store fundamental data
//! 3. Never change an encoding without bumping the schema version
//! 4. Follow stringent versioning rules for backward compatibility
//!
//! **Helper Types (`helpers` module):**
//! 1. Do NOT implement `ZainoVersionedSerde`
//! 2. Used for in-memory operations, conversions, and coordination
//! 3. Can be changed more freely as they're not persisted

#[cfg(test)]
pub(crate) mod fixtures;

pub mod block_context;
pub mod db;
pub mod helpers;
pub mod primitives;

// Re-export database types for backward compatibility
pub use db::legacy::*;
pub use db::{CommitmentTreeData, CommitmentTreeRoots, CommitmentTreeSizes};

// Re-export business-layer primitives and containers
pub use block_context::BlockContext;
pub use primitives::{
    AbsoluteChainWork, BlockIndex, CompactDifficulty, CompactDifficultyError, SingleBlockWork,
};

// Re-export helper types
pub use helpers::{
    BestChainLocation, BlockMetadata, BlockWithMetadata, ChainScope, NonBestChainLocation,
    TreeRootData,
};
