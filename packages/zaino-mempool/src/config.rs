//! Mempool read-model configuration and cost accounting.
//!
//! These are Zaino public-service safety bounds, not validator mempool policy.

use std::num::{NonZeroU64, NonZeroUsize};
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

/// Default source poll cadence, in milliseconds, and the default floor between
/// metadata listings.
pub const DEFAULT_POLL_INTERVAL_MS: u64 = 500;

/// Default source poll cadence, and the default floor between metadata listings.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(DEFAULT_POLL_INTERVAL_MS);

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
    /// Zero is unrepresentable: `broadcast::channel(0)` panics.
    event_buffer_len: NonZeroUsize,

    /// How often the update loop polls the source when no wake signal arrives,
    /// in milliseconds.
    ///
    /// Held as `NonZeroU64` millis rather than a `Duration` because zero must be
    /// unrepresentable: `tokio::time::interval` panics on a zero period, which
    /// takes the process down at spawn rather than at a read. `Duration` has no
    /// non-zero form, so the guarantee has to live in the stored type — read it
    /// back as a `Duration` via [`poll_interval`](Self::poll_interval).
    poll_interval_ms: NonZeroU64,

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
    /// Zero is **legal** here, unlike [`poll_interval`](Self::poll_interval):
    /// this is a floor compared with `>=`, so zero simply means "no floor beyond
    /// the poll cadence" — a meaningful setting, not a broken one. A plain
    /// `Duration`, deliberately.
    ///
    /// [`IncompletePendingMetadata`]: crate::snapshot::MempoolCompleteness::IncompletePendingMetadata
    metadata_min_interval: Duration,

    /// Maximum number of raw-transaction fetches issued concurrently when
    /// reconciling additions.
    ///
    /// Zero is unrepresentable: it would mean "reconcile with no concurrency at
    /// all", which stalls rather than throttles. Previously guarded by a
    /// `.max(1)` at the point of use, which silently rewrote the operator's
    /// value instead of rejecting it.
    max_concurrent_raw_fetches: NonZeroUsize,

    /// Maximum number of exclude suffixes a client may send to a filtered
    /// mempool read. Zero is legal: it disables client-supplied exclusion.
    max_exclude_count: usize,

    /// Minimum length (bytes) of a client-supplied exclude suffix. Rejects
    /// empty/near-empty suffixes that would match most of the mempool.
    min_exclude_suffix_len: usize,

    /// Maximum length (bytes) of a client-supplied exclude suffix.
    max_exclude_suffix_len: usize,
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

    /// Capacity of the bounded change-feed / event broadcast channels.
    pub fn event_buffer_len(&self) -> usize {
        self.event_buffer_len.get()
    }

    /// Set the change-feed capacity. Non-zero by type: zero panics
    /// `broadcast::channel`.
    pub fn set_event_buffer_len(&mut self, len: NonZeroUsize) {
        self.event_buffer_len = len;
    }

    /// How often the update loop polls the source when no wake signal arrives.
    pub fn poll_interval(&self) -> Duration {
        Duration::from_millis(self.poll_interval_ms.get())
    }

    /// Set the poll cadence, in milliseconds. Non-zero by type: a zero period
    /// panics `tokio::time::interval` at spawn.
    pub fn set_poll_interval_ms(&mut self, millis: NonZeroU64) {
        self.poll_interval_ms = millis;
    }

    /// Minimum interval between per-entry metadata listings.
    pub fn metadata_min_interval(&self) -> Duration {
        self.metadata_min_interval
    }

    /// Set the metadata-listing floor. Zero is accepted and means "no floor
    /// beyond the poll cadence" — see the field documentation.
    pub fn set_metadata_min_interval(&mut self, interval: Duration) {
        self.metadata_min_interval = interval;
    }

    /// Maximum number of raw-transaction fetches issued concurrently.
    pub fn max_concurrent_raw_fetches(&self) -> usize {
        self.max_concurrent_raw_fetches.get()
    }

    /// Set the raw-fetch concurrency. Non-zero by type: zero would stall
    /// reconciliation rather than throttle it.
    pub fn set_max_concurrent_raw_fetches(&mut self, fetches: NonZeroUsize) {
        self.max_concurrent_raw_fetches = fetches;
    }

    /// Maximum number of exclude suffixes a client may send.
    pub fn max_exclude_count(&self) -> usize {
        self.max_exclude_count
    }

    /// Set the exclude-suffix count bound. Zero disables client exclusion.
    pub fn set_max_exclude_count(&mut self, count: usize) {
        self.max_exclude_count = count;
    }

    /// Minimum length (bytes) of a client-supplied exclude suffix.
    pub fn min_exclude_suffix_len(&self) -> usize {
        self.min_exclude_suffix_len
    }

    /// Maximum length (bytes) of a client-supplied exclude suffix.
    pub fn max_exclude_suffix_len(&self) -> usize {
        self.max_exclude_suffix_len
    }
}

impl Default for MempoolConfig {
    fn default() -> Self {
        Self {
            max_cost_bytes: Arc::new(AtomicU64::new(DEFAULT_MAX_COST_BYTES)),
            event_buffer_len: NonZeroUsize::new(16_384).expect("16384 is non-zero"),
            poll_interval_ms: NonZeroU64::new(DEFAULT_POLL_INTERVAL_MS)
                .expect("the default poll interval is non-zero"),
            // Equal to the poll interval: no coalescing beyond the poll cadence
            // itself, so the default preserves minimum mempool latency.
            metadata_min_interval: DEFAULT_POLL_INTERVAL,
            max_concurrent_raw_fetches: NonZeroUsize::new(32).expect("32 is non-zero"),
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
