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
    /// Measured **resident anonymous memory (RssAnon) ceiling** for accumulating one
    /// bulk-sync write batch (one durable LMDB commit). The batcher polls the process's
    /// real RssAnon (from `/proc/self/status`) while a batch fills and flushes when it
    /// reaches this value — a measurement, not an estimate. Default 6 GiB.
    ///
    /// This bounds the *accumulation* phase, not the peak. During the subsequent flush the
    /// encoded write-data and the in-memory pending overlay coexist with the buffer, so the
    /// real peak runs moderately above this value (visible in the per-batch
    /// "Committed batch … RssAnon now … MiB" log). Size the budget to leave page-cache
    /// headroom: a batch large enough to consume RAM starves the DB working-set cache, which
    /// usually *hurts* throughput — so bigger is not better. On a large-RAM host raise it
    /// cautiously while watching the logged RssAnon and `free -g` buff/cache; lower it on
    /// smaller-RAM hosts. On non-Linux hosts or when `/proc` is unreadable, the batcher falls
    /// back to an (undercounting) size estimate.
    #[serde(default = "default_sync_write_batch_bytes")]
    pub sync_write_batch_bytes: u64,
    /// Open the finalised database with `WRITE_MAP` for a fast, operator-initiated bulk
    /// catch-up. Default `false`: serving and tests open durable copy-on-write.
    ///
    /// `WRITE_MAP` maps the DB read-write so bulk writes skip the per-page copy and the
    /// dirty-set spill ceiling, but the on-disk DB becomes a writable mapping (a stray
    /// process write can corrupt it) and LMDB extends the file toward the full `size`
    /// (384 GB default), which SIGBUSes or exceeds quota on a small or quota'd disk.
    /// Enable it only for a deliberate catch-up run on a host sized for it, then restart
    /// with it off to serve durably.
    #[serde(default)]
    pub bulk_sync: bool,
    /// Sync fetch-pipeline depth: how many blocks to fetch concurrently *ahead* of
    /// build/write during catch-up. Fetch (`getblock` + a per-block treestate) is the sync
    /// bottleneck, so running this many fetch units concurrently overlaps them with the
    /// strictly-ordered build/write and lifts throughput toward the build-bound ceiling.
    /// Larger saturates the validator and hides more latency, but holds that many blocks in
    /// flight — real memory on fat (sandblast-era) blocks, on top of the write batch — so
    /// raise it cautiously. Read through
    /// [`effective_sync_fetch_lookahead`](Self::effective_sync_fetch_lookahead), which floors
    /// it at 1. Default 8.
    #[serde(default = "default_sync_fetch_lookahead")]
    pub sync_fetch_lookahead: usize,
}

/// Default [`DatabaseConfig::sync_write_batch_bytes`]: 6 GiB of measured RssAnon during
/// batch accumulation. The real flush peak runs moderately above this; size the budget to
/// leave page-cache headroom (see the field doc).
fn default_sync_write_batch_bytes() -> u64 {
    6 * 1024 * 1024 * 1024
}

/// Default [`DatabaseConfig::sync_fetch_lookahead`]: 8 fetch units run concurrently ahead of
/// the strictly-ordered build/write (see the field doc).
fn default_sync_fetch_lookahead() -> usize {
    8
}

impl DatabaseConfig {
    /// The fetch-pipeline depth the sync loop actually uses: the configured
    /// [`sync_fetch_lookahead`](Self::sync_fetch_lookahead) floored at 1, so it is always a
    /// valid (non-zero) concurrent-stream width even if a config sets it to 0. The single
    /// home for that floor — the sync loop and its tests both read it here rather than
    /// re-applying `.max(1)`.
    pub fn effective_sync_fetch_lookahead(&self) -> usize {
        self.sync_fetch_lookahead.max(1)
    }
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            path: resolve_path_with_xdg_cache_defaults("zaino"),
            size: DatabaseSize::default(),
            sync_write_batch_bytes: default_sync_write_batch_bytes(),
            bulk_sync: false,
            sync_fetch_lookahead: default_sync_fetch_lookahead(),
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
