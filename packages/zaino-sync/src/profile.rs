//! Batch/phase sync profiling (feature `sync-profile`).
//!
//! In-process attribution of where each persistence batch's wall time goes,
//! compiled out entirely when the `sync-profile` feature is off. The engine
//! feeds phase durations into a `SyncProfile`, which emits one structured
//! `tracing` event per committed batch (message `sync batch profile`).
//!
//! Mechanism: phase durations accumulate across a *window* (the span between
//! two batch commits). `wait` and `extract` accumulate window-globally;
//! `merge_persist` samples are keyed by the batch each index persisted, so a
//! batch's per-index op counts are exactly what it committed. On each commit,
//! `record_commit` emits the window, computes `residual_ms` against
//! `window_ms`, and resets.
//!
//! The attribution rule — **a phase is attributed to the batch that commits
//! next** — and the meaning of each emitted field are documented for the
//! operator in the crate's `usage.md`.

/// A phase timer.
///
/// Real (wraps [`std::time::Instant`]) under `sync-profile`; a zero-sized
/// no-op otherwise, so timing sites compile to nothing when the feature is
/// off and never call [`Instant::now`](std::time::Instant::now).
#[cfg(feature = "sync-profile")]
pub(crate) struct PhaseTimer(std::time::Instant);

#[cfg(feature = "sync-profile")]
impl PhaseTimer {
    /// Start timing from now.
    pub(crate) fn start() -> Self {
        Self(std::time::Instant::now())
    }

    /// Stop and return the elapsed duration.
    pub(crate) fn stop(self) -> std::time::Duration {
        self.0.elapsed()
    }
}

/// Zero-sized no-op timer used when `sync-profile` is off.
#[cfg(not(feature = "sync-profile"))]
pub(crate) struct PhaseTimer;

#[cfg(not(feature = "sync-profile"))]
impl PhaseTimer {
    pub(crate) fn start() -> Self {
        Self
    }

    pub(crate) fn stop(self) {}
}

#[cfg(feature = "sync-profile")]
pub(crate) use enabled::SyncProfile;

#[cfg(all(test, feature = "sync-profile"))]
pub(crate) use enabled::BatchProfileRecord;

#[cfg(feature = "sync-profile")]
mod enabled {
    use std::collections::HashMap;
    use std::fmt::Write as _;
    use std::time::{Duration, Instant};

    use crate::primitives::{BatchIndex, IndexId};

    /// Milliseconds as `f64` from a [`Duration`].
    fn ms(d: Duration) -> f64 {
        d.as_secs_f64() * 1000.0
    }

    /// One index's merge+persist timing for a specific batch.
    struct MergePersistSample {
        index: IndexId,
        merge_persist: Duration,
        ops: usize,
    }

    /// A fully-formed per-batch profile, retained for assertions in tests.
    #[cfg(test)]
    #[derive(Debug, Clone)]
    pub(crate) struct BatchProfileRecord {
        pub(crate) batch: u32,
        pub(crate) committed_height: u64,
        pub(crate) blocks: u64,
        pub(crate) wait_ms: f64,
        pub(crate) extract_ms: f64,
        /// Per index: `(id, merge+persist ms, op count)`.
        pub(crate) merge_persist: Vec<(IndexId, f64, usize)>,
        pub(crate) commit_ms: f64,
        pub(crate) window_ms: f64,
        pub(crate) residual_ms: f64,
    }

    /// Accumulates phase timings across a window and emits one event per
    /// committed batch. See the [module docs](super) for the attribution rule.
    pub(crate) struct SyncProfile {
        /// First block height of this sync run — seeds the block count of the
        /// first committed batch.
        start_height: u64,
        /// Wall-clock base of the current window (previous commit, or start).
        window_base: Instant,
        /// Time blocked awaiting blocks since the last emit (window-global).
        wait: Duration,
        /// Extraction wall time since the last emit (window-global).
        extract: Duration,
        /// Merge+persist samples keyed by the batch the index persisted.
        merge_persist: HashMap<BatchIndex, Vec<MergePersistSample>>,
        /// Highest committed height of the previously emitted batch.
        last_committed_height: Option<u64>,
        #[cfg(test)]
        records: Vec<BatchProfileRecord>,
    }

    impl SyncProfile {
        /// A fresh accumulator for a sync run starting at `start_height`.
        pub(crate) fn new(start_height: u64) -> Self {
            Self {
                start_height,
                window_base: Instant::now(),
                wait: Duration::ZERO,
                extract: Duration::ZERO,
                merge_persist: HashMap::new(),
                last_committed_height: None,
                #[cfg(test)]
                records: Vec::new(),
            }
        }

        /// Add time blocked awaiting blocks (fetch starvation).
        pub(crate) fn add_wait(&mut self, d: Duration) {
            self.wait += d;
        }

        /// Add extraction wall time.
        pub(crate) fn add_extract(&mut self, d: Duration) {
            self.extract += d;
        }

        /// Record one index's merge+persist for a batch.
        pub(crate) fn add_merge_persist(
            &mut self,
            index: IndexId,
            batch: BatchIndex,
            d: Duration,
            ops: usize,
        ) {
            self.merge_persist
                .entry(batch)
                .or_default()
                .push(MergePersistSample {
                    index,
                    merge_persist: d,
                    ops,
                });
        }

        /// Emit the profile for a committed batch and reset the window.
        ///
        /// `commit` is the engine-measured wall time of the atomic
        /// `writer.commit(ops)` call (put loop + flush); the LMDB backend
        /// emits its own split of that figure, correlated by the span the
        /// engine opens around the commit.
        pub(crate) fn record_commit(
            &mut self,
            batch: BatchIndex,
            committed_height: u64,
            commit: Duration,
        ) {
            let window = self.window_base.elapsed();
            let samples = self.merge_persist.remove(&batch).unwrap_or_default();

            // Blocks in this batch: heights are contiguous and batches commit
            // in order, so the span to the previous committed height is exact.
            let blocks = match self.last_committed_height {
                Some(prev) => committed_height.saturating_sub(prev),
                None => committed_height.saturating_sub(self.start_height) + 1,
            };

            let mut per_index = String::new();
            let mut merge_persist_total = Duration::ZERO;
            #[cfg(test)]
            let mut record_samples = Vec::with_capacity(samples.len());
            for sample in &samples {
                let sample_ms = ms(sample.merge_persist);
                if !per_index.is_empty() {
                    per_index.push(' ');
                }
                // Infallible: writing into a String never errors.
                let _ = write!(
                    per_index,
                    "{}={sample_ms:.3}/{}",
                    sample.index.as_str(),
                    sample.ops
                );
                merge_persist_total += sample.merge_persist;
                #[cfg(test)]
                record_samples.push((sample.index, sample_ms, sample.ops));
            }

            let wait_ms = ms(self.wait);
            let extract_ms = ms(self.extract);
            let commit_ms = ms(commit);
            let window_ms = ms(window);
            let residual_ms =
                window_ms - wait_ms - extract_ms - ms(merge_persist_total) - commit_ms;

            tracing::info!(
                batch = batch.value(),
                committed_height,
                blocks,
                wait_ms,
                extract_ms,
                merge_persist = %per_index,
                commit_ms,
                window_ms,
                residual_ms,
                "sync batch profile"
            );

            #[cfg(test)]
            self.records.push(BatchProfileRecord {
                batch: batch.value(),
                committed_height,
                blocks,
                wait_ms,
                extract_ms,
                merge_persist: record_samples,
                commit_ms,
                window_ms,
                residual_ms,
            });

            // Reset the window.
            self.wait = Duration::ZERO;
            self.extract = Duration::ZERO;
            self.window_base = Instant::now();
            self.last_committed_height = Some(committed_height);
        }

        /// The per-batch records emitted so far (test-only inspection hook).
        #[cfg(test)]
        pub(crate) fn records(&self) -> &[BatchProfileRecord] {
            &self.records
        }
    }
}
