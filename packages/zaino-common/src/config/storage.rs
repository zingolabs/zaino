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
    /// Database size limit. Defaults to [`DatabaseSize::default`].
    #[serde(default)]
    pub size: DatabaseSize,
    /// Heap budget for one bulk-sync write batch (one durable LMDB commit): a bound on the
    /// buffered `Vec<IndexedBlock>`. Peak resident RAM per batch is **~2–3× this**, because
    /// the buffer, its encoded `BlockWriteData`, and the pending overlay are all live at
    /// flush. Larger values amortise the per-commit fsync over more blocks and sort more keys
    /// per batch, at the cost of that peak RAM; under WRITE_MAP the dirty write-set is
    /// reclaimable file cache (NOMETASYNC-flushed per batch), so the buffer is the binding
    /// hard-RAM constraint.
    ///
    /// Default 6 GiB is the marginally-safe budget for a ~64 GiB host: with the ~20 GiB
    /// transparent UTXO cache and ~2 GiB process baseline fixed, a 6 GiB budget peaks at
    /// ~18 GiB hard (~40 GiB hard total) plus a transient ~8 GiB reclaimable dirty set —
    /// ~48 GiB peak resident, leaving ~16 GiB for OS, the sorted sweep's page-cache window,
    /// and the multiplier estimate's error (write volume ~8.5 GiB stays under the
    /// vm.dirty_ratio throttle). Lower it on smaller-RAM hosts; raise toward ~8 GiB only once
    /// measurement confirms the ~3× peak multiplier. (Estimate-based.)
    #[serde(default = "default_sync_write_batch_bytes")]
    pub sync_write_batch_bytes: u64,
}

/// Default [`DatabaseConfig::sync_write_batch_bytes`]: 6 GiB (marginally safe on a ~64 GiB host).
fn default_sync_write_batch_bytes() -> u64 {
    6 * 1024 * 1024 * 1024
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            path: resolve_path_with_xdg_cache_defaults("zaino"),
            size: DatabaseSize::default(),
            sync_write_batch_bytes: default_sync_write_batch_bytes(),
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
