//! Mempool read-model configuration and cost accounting.
//!
//! These are Zaino public-service safety bounds, not validator mempool policy.

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::Duration;

/// Per-transaction cost floor, in bytes, mirroring Zebra's ZIP-401
/// `MEMPOOL_TRANSACTION_COST_THRESHOLD`. Every transaction costs at least this
/// much regardless of its serialized size, so a flood of tiny transactions is
/// bounded the same way the validator bounds it.
pub const MEMPOOL_TRANSACTION_COST_THRESHOLD: u64 = 10_000;

/// Default total mempool cost bound, in bytes (128 MiB).
///
/// Chosen deliberately *above* Zebra's default `tx_cost_limit` of 80,000,000 so
/// that, in healthy operation, the validator's own ZIP-401 eviction keeps its
/// mempool under Zaino's cap and this bound is never reached. It exists purely as
/// a denial-of-service backstop; hitting it marks the snapshot incomplete rather
/// than silently dropping data.
pub const DEFAULT_MAX_COST_BYTES: u64 = 128 * 1024 * 1024;

/// Default source poll cadence, and the default floor between metadata listings.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// The ZIP-401 cost of a single transaction: `max(serialized_size, threshold)`.
pub fn tx_cost(raw_len: u64) -> u64 {
    raw_len.max(MEMPOOL_TRANSACTION_COST_THRESHOLD)
}

/// Configuration and safety bounds for the mempool read model.
#[derive(Debug, Clone)]
pub struct MempoolConfig {
    /// Maximum total mempool cost, in bytes (sum of per-entry [`tx_cost`]).
    ///
    /// Held behind a shared atomic so it can be adjusted at runtime per process;
    /// all clones of a config observe the change.
    max_cost_bytes: Arc<AtomicU64>,

    /// Capacity of the bounded change-feed / event broadcast channels.
    ///
    /// This is a **lag-tolerance** knob, not a correctness one: it sets how many
    /// undelivered updates a subscriber may fall behind before it is told to
    /// resync (`MempoolUpdate::Lagged`). State-losslessness does not depend on it —
    /// a lagged consumer recovers the full set from `current()` — so it can be kept
    /// safely bounded. Since buffered updates carry no snapshots (only entries and
    /// small facts), memory is `~capacity` small slots regardless of subscriber
    /// count. The default trades a generous window for that bounded cost.
    pub event_buffer_len: usize,

    /// How often the update loop polls the source when no wake signal arrives.
    pub poll_interval: Duration,

    /// Minimum interval between per-entry metadata listings
    /// (`getrawmempool verbose`), which the source answers by walking its whole
    /// mempool.
    ///
    /// Additions cannot be admitted without their validator-sourced metadata, so
    /// a poll that finds additions before this interval has elapsed *defers only
    /// those additions* — marking the set [`IncompletePendingMetadata`] — while
    /// still publishing the poll's removals and tip re-tag. Raising it therefore
    /// trades **addition visibility** latency (up to this interval) for load on
    /// the validator, and carries **no coherence penalty**: because the re-tag is
    /// published on every poll, tip-coherent reads thaw after a block on the poll
    /// cadence regardless of this value. The default equals [`Self::poll_interval`],
    /// i.e. no additional coalescing.
    ///
    /// [`IncompletePendingMetadata`]: crate::snapshot::MempoolCompleteness::IncompletePendingMetadata
    pub metadata_min_interval: Duration,

    /// Maximum number of raw-transaction fetches issued concurrently when
    /// reconciling additions.
    pub max_concurrent_raw_fetches: usize,

    /// Maximum number of exclude suffixes a client may send to a filtered
    /// mempool read.
    pub max_exclude_count: usize,

    /// Minimum length (bytes) of a client-supplied exclude suffix. Rejects
    /// empty/near-empty suffixes that would match most of the mempool.
    pub min_exclude_suffix_len: usize,

    /// Maximum length (bytes) of a client-supplied exclude suffix.
    pub max_exclude_suffix_len: usize,
}

impl MempoolConfig {
    /// The current maximum total mempool cost, in bytes.
    pub fn max_cost_bytes(&self) -> u64 {
        self.max_cost_bytes.load(Ordering::Relaxed)
    }

    /// Update the maximum total mempool cost at runtime. Visible to every clone
    /// of this config (they share the underlying atomic).
    pub fn set_max_cost_bytes(&self, bytes: u64) {
        self.max_cost_bytes.store(bytes, Ordering::Relaxed);
    }
}

impl Default for MempoolConfig {
    fn default() -> Self {
        Self {
            max_cost_bytes: Arc::new(AtomicU64::new(DEFAULT_MAX_COST_BYTES)),
            event_buffer_len: 16_384,
            poll_interval: DEFAULT_POLL_INTERVAL,
            // Equal to the poll interval: no coalescing beyond the poll cadence
            // itself, so the default preserves minimum mempool latency.
            metadata_min_interval: DEFAULT_POLL_INTERVAL,
            max_concurrent_raw_fetches: 32,
            max_exclude_count: 1_024,
            min_exclude_suffix_len: 4,
            max_exclude_suffix_len: 32,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tx_cost_applies_the_floor() {
        // Below the threshold: floored.
        assert_eq!(tx_cost(500), MEMPOOL_TRANSACTION_COST_THRESHOLD);
        // At/above the threshold: the raw size.
        assert_eq!(tx_cost(10_000), 10_000);
        assert_eq!(tx_cost(50_000), 50_000);
    }

    #[test]
    fn default_bound_sits_above_zebras_cost_limit() {
        // The DoS backstop must sit above Zebra's ZIP-401 default (80_000_000)
        // so healthy operation never reaches it.
        const { assert!(DEFAULT_MAX_COST_BYTES > 80_000_000) };
    }

    #[test]
    fn max_cost_bytes_is_runtime_adjustable_across_clones() {
        let config = MempoolConfig::default();
        assert_eq!(config.max_cost_bytes(), DEFAULT_MAX_COST_BYTES);

        // Clones share the underlying atomic, so a runtime change is visible to
        // every holder of the config (subscribers, service, etc.).
        let clone = config.clone();
        config.set_max_cost_bytes(1_000_000);
        assert_eq!(clone.max_cost_bytes(), 1_000_000);
        assert_eq!(config.max_cost_bytes(), 1_000_000);
    }
}
