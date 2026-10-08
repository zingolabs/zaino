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

/// Re-exported so a config holder constructing a [`StoreConfig`] can name the
/// deferral policy without reaching into `zaino-indexer`.
pub use zaino_indexer::DeferralPolicy;

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
    /// Whether the initial index catch-up may defer its scattered (hash-keyed)
    /// writes to sorted run logs and bulk-load them in key order — `auto` (the
    /// default) to defer once the catch-up gap warrants it, `off` to reproduce
    /// the direct write path exactly. `off` trades a faster first sync for
    /// reach-climb availability, or suits a deployment without the temporary disk
    /// the run logs need.
    #[serde(default)]
    pub deferred_writes: DeferralPolicy,
}

impl StoreConfig {
    /// The map size a store gets when the config names none.
    pub const fn default_map_size_gb() -> usize {
        16
    }
}

/// What the indexer asks the validator for at each height.
///
/// Both reads project to the same provisioning context, so the index built is
/// identical either way; they differ in where the work of skipping what the
/// indexes never read is done, and in which validators can answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum FetchStrategy {
    /// The whole block over the standard block read, projected locally by
    /// walking the encoding: the fields the index keeps are read as bytes and
    /// proofs, signatures and scripts are stepped over, so nothing the indexes
    /// discard is ever deserialised. Any validator answers this read, which is
    /// why it is the default.
    #[default]
    Full,
    /// The pre-index compact block: the same fields, skipped on the validator's
    /// side instead of ours. Needs a validator that serves the compact read
    /// (zaino's zebra fork), so it is opt-in.
    Compact,
}

/// Index-build tuning.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct IndexerConfig {
    /// What the provisioner fetches per height.
    pub fetch: FetchStrategy,
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
            fetch: FetchStrategy::default(),
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
