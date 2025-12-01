//! Storage configuration types shared across Zaino services.

use std::path::PathBuf;

/// Cache configuration for DashMaps.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct CacheConfig {
    /// Capacity of the DashMaps used for caching
    pub capacity: usize,
    /// Power of 2 for number of shards (e.g., 4 means 16 shards)
    ///
    /// The actual shard count will be 2^shard_power.
    /// Valid range is typically 0-8 (1 to 256 shards).
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

/// Database size limit configuration.
///
/// This enum provides a clean TOML interface and easy extensibility for different units.
#[derive(Debug, Clone, Copy, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DatabaseSize {
    /// Limited to a specific size in GB
    Gb(usize),
    // Future: easy to add Mb(usize), Tb(usize), etc.
}

impl Default for DatabaseSize {
    fn default() -> Self {
        DatabaseSize::Gb(128) // Default to 128 GB
    }
}

impl PartialEq for DatabaseSize {
    fn eq(&self, other: &Self) -> bool {
        self.to_byte_count() == other.to_byte_count()
    }
}

impl DatabaseSize {
    /// Convert to bytes
    pub fn to_byte_count(&self) -> usize {
        match self {
            DatabaseSize::Gb(gb) => gb * 1024 * 1024 * 1024,
        }
    }
}

/// Database configuration.
///
/// Configures the file path and size limits for persistent storage
/// used by Zaino services.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct DatabaseConfig {
    /// Database file path.
    pub path: PathBuf,
    /// Database size limit. Defaults to 128 GB.
    #[serde(default)]
    pub size: DatabaseSize,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            path: PathBuf::from("./zaino_cache"),
            size: DatabaseSize::default(),
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
