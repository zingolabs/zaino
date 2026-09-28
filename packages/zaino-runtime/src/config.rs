//! The typed parameters a deployment's assembly consumes.
//!
//! These are sections of an operator's config, not the config itself: the
//! daemon owns the file, the layering and the defaults that depend on where
//! it runs (paths, addresses), and hands the assembly these typed pieces.

use std::num::NonZeroUsize;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use zaino_consensus::MAX_BLOCK_REORG_HEIGHT;
use zaino_indexer::FetchConcurrency;

/// The finalised index store (LMDB).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StoreConfig {
    /// Directory holding the LMDB environment (created if absent). No default:
    /// where the store lives depends on where the daemon runs.
    pub path: PathBuf,
    /// Maximum on-disk size in GiB, reserved up front (LMDB requires a map size
    /// bound at open time).
    #[serde(default = "StoreConfig::default_map_size_gb")]
    pub map_size_gb: usize,
}

impl StoreConfig {
    /// The map size a store gets when the config names none.
    pub const fn default_map_size_gb() -> usize {
        16
    }
}

/// Index-build tuning.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct IndexerConfig {
    /// Blocks committed per atomic batch.
    pub batch_size: u32,
    /// Contexts buffered between the provisioner and the engine.
    pub channel_capacity: usize,
    /// Depth below the tip treated as still volatile; only `tip − depth` and
    /// below is indexed.
    pub finalised_depth: u32,
    /// Fetches kept in flight by the provisioner. Concurrent fetch keeps the
    /// parallel engine fed rather than paced by a one-at-a-time loop. A
    /// `concurrency = 0` in the config is rejected at parse time (non-zero type).
    pub concurrency: FetchConcurrency,
}

impl Default for IndexerConfig {
    fn default() -> Self {
        Self {
            batch_size: 1000,
            channel_capacity: 256,
            finalised_depth: MAX_BLOCK_REORG_HEIGHT,
            concurrency: FetchConcurrency::new(NonZeroUsize::new(16).expect("16 is non-zero")),
        }
    }
}

/// What a deployment that builds a local finalised index needs to be told:
/// where the store lives and how the indexer paces itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedDeploymentConfig {
    /// The finalised index store.
    pub store: StoreConfig,
    /// Index-build tuning.
    pub indexer: IndexerConfig,
}
