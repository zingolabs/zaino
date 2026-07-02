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

/// Finalised-state bulk-sync write-batch memory budget, in gibibytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(transparent)]
pub struct SyncWriteBatchSize(pub usize);

impl Default for SyncWriteBatchSize {
    fn default() -> Self {
        // 8 GiB. This is a *heap* budget for buffered blocks only; lower it on a memory-constrained
        // host (or a cgroup-limited container). A larger value does not make a small host faster — it
        // just risks the OOM-killer, and a kill under `NO_SYNC` is what then corrupts the on-disk DB.
        // (Previously 32 GiB, which OOM-killed small hosts.)
        SyncWriteBatchSize(8)
    }
}

impl SyncWriteBatchSize {
    /// Convert to bytes, saturating instead of overflowing on an absurd configured value.
    pub fn to_byte_count(&self) -> usize {
        self.0.saturating_mul(1024 * 1024 * 1024)
    }
}

/// Memory budget (in gibibytes) for the from-genesis txout-set accumulator rebuild's in-RAM spent
/// set.
///
/// Deliberately separate from [`SyncWriteBatchSize`]: the accumulator rebuild and the bulk-sync
/// write batch are different operations with different peak-memory shapes. Coupling them caused a
/// large block-buffer budget to be silently reused as the accumulator's per-shard cap, OOM-killing
/// small hosts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(transparent)]
pub struct AccumulatorRebuildMemorySize(pub usize);

impl Default for AccumulatorRebuildMemorySize {
    fn default() -> Self {
        // 8 GiB. The rebuild auto-shards the spent set to keep each shard's `HashSet` within this
        // budget, so raising it only trades fewer/larger passes (faster) for more peak RAM, and
        // lowering it bounds peak RAM at the cost of more passes. Over-sharding is safe; a too-large
        // budget on a small host is not — lower this on a memory-constrained host.
        AccumulatorRebuildMemorySize(8)
    }
}

impl AccumulatorRebuildMemorySize {
    /// Convert to bytes, saturating instead of overflowing on an absurd configured value.
    pub fn to_byte_count(&self) -> usize {
        self.0.saturating_mul(1024 * 1024 * 1024)
    }
}

/// Database configuration.
///
/// Configures the file path and size limits for persistent storage
/// used by Zaino services.
///
/// `deny_unknown_fields`: an unrecognized key under `[storage.database]` is a hard error rather
/// than being silently ignored. This guards against the exact footgun that motivated this struct's
/// last revision — a config still using the old `sync_write_batch_bytes` key would otherwise be
/// dropped silently and the renamed `sync_write_batch_size` would fall back to its (large) default,
/// OOM-killing the node. Failing loudly forces the operator to migrate the key.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct DatabaseConfig {
    /// Database file path.
    pub path: PathBuf,
    /// Database size limit. Defaults to 128 GB.
    #[serde(default)]
    pub size: DatabaseSize,
    /// Approximate in-memory budget (in GiB) for the finalised-state bulk-sync write batch.
    ///
    /// Bulk sync buffers fetched blocks up to this budget, then writes the whole batch in one
    /// LMDB transaction with the random-keyed `spent` / `txid_location` entries inserted in **sorted**
    /// key order. Sorting turns the random B-tree leaf faults (which dominate once the DB exceeds
    /// RAM) into a sequential sweep; larger batches mean fewer sweeps.
    ///
    /// NOTE: this is a heap budget for buffered blocks; peak RAM is roughly this budget plus the
    /// transaction's dirty pages, and it competes with the OS page cache the sorted sweep relies on
    /// — larger is not always better, and on a memory-constrained host it risks the OOM-killer.
    /// Defaults to 8 GiB; lower it on memory-constrained hosts. The txout-set accumulator rebuild
    /// has its own, separate budget ([`DatabaseConfig::accumulator_rebuild_memory_size`]).
    #[serde(default)]
    pub sync_write_batch_size: SyncWriteBatchSize,

    /// In-memory budget (in GiB) for the from-genesis txout-set accumulator rebuild's spent set.
    ///
    /// The rebuild auto-shards by creating-txid prefix so the per-shard in-RAM spent set stays
    /// within this budget. Kept separate from [`DatabaseConfig::sync_write_batch_size`] so the two
    /// unrelated operations cannot inflate each other's peak memory. Defaults to 8 GiB; lower it on
    /// memory-constrained hosts to bound peak rebuild RAM (at the cost of more, smaller passes).
    #[serde(default)]
    pub accumulator_rebuild_memory_size: AccumulatorRebuildMemorySize,

    /// Maximum wall-clock time (in seconds) spent buffering a single bulk-sync write batch before
    /// flushing, so commits (and durability) happen regularly even when block fetches are slow.
    ///
    /// Because the environment runs with `NO_SYNC`, this also bounds the window of unflushed writes
    /// at risk on a hard kill / eviction. Raise it to make sync less reactive but faster on large-RAM
    /// hosts where the memory budget is never reached before this interval; lower it to shrink the
    /// crash-loss / corruption window. Defaults to 120 seconds (2 minutes).
    #[serde(default = "default_sync_checkpoint_interval")]
    pub sync_checkpoint_interval: u64,
}

/// Default [`DatabaseConfig::sync_checkpoint_interval`]: 120 seconds (2 minutes).
fn default_sync_checkpoint_interval() -> u64 {
    120
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            path: resolve_path_with_xdg_cache_defaults("zaino"),
            size: DatabaseSize::default(),
            sync_write_batch_size: SyncWriteBatchSize::default(),
            accumulator_rebuild_memory_size: AccumulatorRebuildMemorySize::default(),
            sync_checkpoint_interval: default_sync_checkpoint_interval(),
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
mod tests {
    use super::*;

    #[test]
    fn default_database_budgets_are_the_safe_values() {
        let database = DatabaseConfig::default();
        assert_eq!(database.sync_write_batch_size, SyncWriteBatchSize(8));
        assert_eq!(
            database.accumulator_rebuild_memory_size,
            AccumulatorRebuildMemorySize(8)
        );
        assert_eq!(database.sync_checkpoint_interval, 120);
    }

    #[test]
    fn budget_byte_counts_saturate_instead_of_overflowing() {
        assert_eq!(
            SyncWriteBatchSize(8).to_byte_count(),
            8 * 1024 * 1024 * 1024
        );
        assert_eq!(
            AccumulatorRebuildMemorySize(8).to_byte_count(),
            8 * 1024 * 1024 * 1024
        );
        // An absurd configured value saturates rather than wrapping to a tiny (or zero) budget.
        assert_eq!(SyncWriteBatchSize(usize::MAX).to_byte_count(), usize::MAX);
    }
}
