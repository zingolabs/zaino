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
///
/// `deny_unknown_fields`: an unrecognized key under `[storage.database]` is a hard
/// error, not silently ignored. In particular the `sync_write_batch_size` key (the
/// GiB-newtype variant from zingolabs/zaino#1263, which this build does not adopt) is
/// rejected rather than dropped — otherwise an operator who set it would have it
/// silently ignored while `sync_write_batch_bytes` quietly kept its default, with no
/// signal the key was discarded. Failing loudly surfaces the key mismatch. (On this
/// build the silent fallback is to the conservative 128 MiB default, so the unflagged
/// failure mode is an under-budgeted / slower sync, not an OOM.)
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
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
    /// better. Defaults to 128 MiB; raise it on large-RAM hosts.
    #[serde(default = "default_sync_write_batch_bytes")]
    pub sync_write_batch_bytes: u64,
}

/// Default [`DatabaseConfig::sync_write_batch_bytes`]: 128 MiB.
fn default_sync_write_batch_bytes() -> u64 {
    128 * 1024 * 1024
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

#[cfg(test)]
mod database_config {
    use super::DatabaseConfig;

    /// Pins the conservative 128 MiB default chosen in `29ae2e52` (down from 4 GiB) so a
    /// refactor or a #1263-style config merge can't silently revert it. This is a *value*
    /// guard only — it runs no sync and observes no memory, so it does NOT establish that
    /// 128 MiB prevents (or 4 GiB causes) an OOM. The documented OOM root causes were the
    /// accumulator rebuild's in-RAM spent set (>16 GiB, #1260) and concurrent-backfill
    /// pileup (#1261), both fixed separately; 128 MiB is a precautionary bound per the
    /// "peak RAM ≈ budget + dirty pages" model (29ae2e52), not a measured threshold.
    #[test]
    fn sync_write_batch_default_is_128_mib() {
        assert_eq!(
            DatabaseConfig::default().sync_write_batch_bytes,
            128 * 1024 * 1024,
        );
    }
}
