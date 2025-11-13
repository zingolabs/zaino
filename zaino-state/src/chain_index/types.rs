//! Type definitions for the chain index.
//!
//! This module provides types for blockchain indexing, organized into two main categories:
//!
//! ## Database Types
//! Types that implement `ZainoVersionedSerde` for database persistence.
//! These types follow strict versioning rules and require migrations for any changes.
//!
//! Currently organized in `db/legacy.rs` (pending refactoring into focused modules):
//! - Block types: BlockHash, BlockIndex, BlockData, IndexedBlock, etc.
//! - Transaction types: TransactionHash, CompactTxData, TransparentCompactTx, etc.
//! - Address types: AddrScript, Outpoint, AddrHistRecord, etc.
//! - Shielded types: SaplingCompactTx, OrchardCompactTx, etc.
//! - Primitives: Height, ChainWork, ShardIndex, etc.
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
//! 3. Never change without implementing a new version and database migration
//! 4. Follow stringent versioning rules for backward compatibility
//!
//! **Helper Types (`helpers` module):**
//! 1. Do NOT implement `ZainoVersionedSerde`
//! 2. Used for in-memory operations, conversions, and coordination
//! 3. Can be changed more freely as they're not persisted

pub mod db;
pub mod helpers;
pub mod primitives;

// Re-export database types for backward compatibility
pub use db::legacy::*;
pub use db::{CommitmentTreeData, CommitmentTreeRoots, CommitmentTreeSizes};

// Re-export helper types
pub use helpers::{
    BestChainLocation, BlockMetadata, BlockWithMetadata, NonBestChainLocation, TreeRootData,
};
