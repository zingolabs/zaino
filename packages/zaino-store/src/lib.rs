//! Zaino Block Store
//!
//! Two-tier block store: in-memory persistent chain for the last N blocks
//! plus an on-disk LMDB for everything older.
//!
//! # Architecture
//!
//! ```text
//!                   chain tip
//!                      │
//!   ┌──────────────────┼──────────────────┐
//!   │  Memory (Chain)                     │
//!   │  up to MAX_REORG_DEPTH blocks       │
//!   │  Persistent, structural sharing     │
//!   └──────────────────┼──────────────────┘
//!                      │ freeze_horizon = tip.height - MAX_REORG_DEPTH
//!   ┌──────────────────┼──────────────────┐
//!   │  LMDB                              │
//!   │  best-chain only, append-only       │
//!   │  height-keyed, hash+block stored    │
//!   └─────────────────────────────────────┘
//! ```
//!
//! # Concurrency
//!
//! - **Writer** builds a new Chain from a snapshot, then swaps under `RwLock`.
//! - **Reader** clones an `Arc<Chain>` under read lock (pointer bump), then
//!   iterates lock-free.
//! - **ChainStream** cursor: an `Arc<Chain>` + cursor position. Materialization
//!   is a forward for-loop. O(1) memory.

pub mod error;
pub mod types;

pub mod chain;
pub mod chain_stream;
pub mod block_iter;
pub mod state;

pub mod lmdb;

pub mod fetcher;
pub mod sync;

// Public API
pub use block_iter::BlockIter;
pub use chain_stream::ChainStream;
pub use fetcher::BlockFetcher;
pub use state::ChainState;
pub use sync::{sync_step, BlockStoreSync, SyncTimings};
pub use error::{StoreError, SyncError};
pub use types::{Block, BlockHash, Height};
