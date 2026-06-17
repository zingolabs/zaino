//! Storage configuration types shared across Zaino services.

use std::path::PathBuf;

use crate::xdg::resolve_path_with_xdg_cache_defaults;

/// Cache configuration for DashMaps.
///
/// Used by the mempool and BlockCache non-finalized state (FetchService backend).
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct CacheConfig {
    /// Capacity of the DashMaps used for caching.
    pub capacity: usize,
    /// Power of 2 for number of shards (e.g., 4 means 16 shards).
    ///
    /// The actual shard count will be 2^shard_power.
    /// Valid range is typically 0-8 (1 to 256 shards).
    /// Must be greater than 0.
    pub shard_power: u8,
}

impl CacheConfig {
    /// Get the actual number of shards (2^shard_power)
    pub fn shard_count(&self) -> u32 {
        // // 'a<<b' works by shifting the binary representation of a, b postions to the left
        // 1 << self.shard_power // 2^shard_power
        2u32.pow(self.shard_power.into())
    }
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            capacity: 10000, // Default capacity
            shard_power: 4,  // Default to 16 shards
        }
    }
}

/// Database size limit in gigabytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(transparent)]
pub struct DatabaseSize(pub usize);

impl Default for DatabaseSize {
    fn default() -> Self {
        DatabaseSize(384) // Default to 384 GB
    }
}

impl DatabaseSize {
    /// Convert to bytes.
    pub fn to_byte_count(&self) -> usize {
        self.0 * 1024 * 1024 * 1024
    }
}

/// Database configuration.
///
/// Configures the file path and size limits for persistent storage
/// used by Zaino services.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct DatabaseConfig {
    /// Database file path.
    pub path: PathBuf,
    /// Database size limit. Defaults to 128 GB.
    #[serde(default)]
    pub size: DatabaseSize,
    /// Approximate in-memory byte budget for the finalised-state bulk-sync write batch.
    ///
    /// Bulk sync buffers fetched blocks up to this many bytes, then writes the whole batch in one
    /// LMDB transaction with the random-keyed `spent` / `txid_location` entries inserted in **sorted**
    /// key order. Sorting turns the random B-tree leaf faults (which dominate once the DB exceeds
    /// RAM) into a sequential sweep; larger batches mean fewer sweeps.
    ///
    /// NOTE: peak RAM is roughly this budget (buffered blocks) plus the transaction's dirty pages,
    /// and it competes with the OS page cache the sorted sweep relies on — larger is not always
    /// better. Defaults to 4 GiB; raise it on large-RAM hosts.
    #[serde(default = "default_sync_write_batch_bytes")]
    pub sync_write_batch_bytes: u64,
    /// Number of blocks fetched concurrently during finalised-state bulk sync.
    ///
    /// The bulk-sync ingestion loop is fetch-latency-bound: each block requires sequential
    /// round-trips to the source (block + commitment-tree roots), and a strictly serial loop leaves
    /// both zaino and the validator idle while it waits on the network. This knob runs up to this
    /// many block fetches in flight at once (yielded back in strict height order, so the writer is
    /// unaffected), overlapping the latency and multiplying fetch throughput until the source's RPC
    /// handler saturates.
    ///
    /// Peak fetch memory is roughly this many buffered blocks, so keep it modest. Defaults to 32;
    /// tune up toward the source's RPC capacity.
    #[serde(default = "default_sync_fetch_concurrency")]
    pub sync_fetch_concurrency: usize,
}

/// Default [`DatabaseConfig::sync_write_batch_bytes`]: 4 GiB.
fn default_sync_write_batch_bytes() -> u64 {
    4 * 1024 * 1024 * 1024
}

/// Default [`DatabaseConfig::sync_fetch_concurrency`]: 32 concurrent block fetches.
fn default_sync_fetch_concurrency() -> usize {
    32
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            path: resolve_path_with_xdg_cache_defaults("zaino"),
            size: DatabaseSize::default(),
            sync_write_batch_bytes: default_sync_write_batch_bytes(),
            sync_fetch_concurrency: default_sync_fetch_concurrency(),
        }
    }
}

/// Storage configuration combining cache and database settings.
///
/// This is used by services that need both in-memory caching and persistent storage.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, Default)]
pub struct StorageConfig {
    /// Cache configuration. Uses defaults if not specified in TOML.
    #[serde(default)]
    pub cache: CacheConfig,
    /// Database configuration
    pub database: DatabaseConfig,
}
