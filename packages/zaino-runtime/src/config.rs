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
    /// Fetches kept in flight by the provisioner. Concurrent fetch keeps the
    /// parallel engine fed rather than paced by a one-at-a-time loop. A
    /// `concurrency = 0` in the config is rejected at parse time (non-zero type).
    pub concurrency: FetchConcurrency,
    /// The consensus reorg depth, in blocks: the number below the chain tip that
    /// is still reorg-able and so must stay in the volatile tier. It is the one
    /// value the [`Seam`](zaino_finality::Seam) is built from, and *both* tiers
    /// derive their boundary from it *through* the seam — the chain head
    /// publishes the horizon `tip - reorg_depth`, and the finalised store commits
    /// no further than that. There is no second constant for the durable tier to
    /// drift against; a single value, read once, at the one construction site.
    ///
    /// Defaults to [`MAX_BLOCK_REORG_HEIGHT`], the consensus bound. A deployment
    /// may lower it — regtest sets it to `0`, so the horizon is the tip and the
    /// finalised store builds all the way up, which is what keeps FS-backed reads
    /// exercised on short test chains.
    ///
    /// The seam guarantees *coherence*, not safety at any depth. One owner, both
    /// tiers derived from this single value, and — because the volatile tier's
    /// trim floor is `min(reorg_safety_floor, w - margin)`, so `floor <= w` holds
    /// structurally — no serving gap between the tiers for any value this knob
    /// takes. What the seam does **not** do is make the value itself safe: this
    /// is a finalisation-depth knob, and lowering it below the consensus reorg
    /// bound lets the append-only store durably commit heights that are still
    /// reorg-able. Those heights stay retained by the volatile tier (whose own
    /// retention keeps the full consensus window regardless of this value), so it
    /// is not happening behind that tier's back and no read is starved — but the
    /// store has no rewind path, so a reorg deeper than `reorg_depth` leaves it
    /// holding a block from an abandoned branch. That is the ordinary
    /// finalisation-depth risk the old `finalised_depth` already carried, not a
    /// new hazard and not the per-tier drift the seam removed; the default is the
    /// consensus bound for that reason, and lowering it trades durability safety
    /// for index latency. Keep it a knob; do not "simplify" it back to a
    /// hardcoded constant.
    pub reorg_depth: u32,
}

impl Default for IndexerConfig {
    fn default() -> Self {
        Self {
            fetch: FetchStrategy::default(),
            batch_size: 1000,
            channel_capacity: 256,
            concurrency: FetchConcurrency::new(NonZeroUsize::new(16).expect("16 is non-zero")),
            reorg_depth: MAX_BLOCK_REORG_HEIGHT,
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
