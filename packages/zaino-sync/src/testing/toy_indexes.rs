//! Toy index set: four indexes demonstrating composition and scope types.
//!
//! Each index lives in its own sub-module and declares a narrow
//! [`BlockContext`](crate::traits::IndexDef::BlockContext). The set-wide
//! `TestBlockContext` projects into each via [`ProvideContext`].
//!
//! BlockLocal indexes: [`ValueIndex`](value_index), [`CountIndex`](count_index),
//! [`RunningSumIndex`](running_sum_index).
//!
//! SelfCumulative indexes: [`CumulativeSumIndex`](cumulative_sum_index) (×Monoidal,
//! collapsed to a tip total) and [`CumulativeSeriesIndex`](cumulative_series_index)
//! (×Append, a retained per-height series).
//!
//! [`ProvideContext`]: crate::traits::ProvideContext

pub mod concat_fold_index;
pub mod concat_index;
pub mod count_index;
pub mod cumulative_series_index;
pub mod cumulative_sum_index;
pub mod running_sum_index;
pub mod value_index;

use crate::primitives::BlockHeight;
use crate::traits::ProvideContext;

use super::TestBlockContext;

// ---------------------------------------------------------------------------
// Shared support for the two non-commutative concat toy indexes
// ---------------------------------------------------------------------------

/// A chain-ordered concatenation of block heights, shared by the `Monoidal`
/// ([`concat_index`]) and `Fold` ([`concat_fold_index`]) concat toys.
///
/// Its combine is deliberately **non-commutative**: `A` followed by `B` lays
/// `A`'s heights before `B`'s. That is what makes the two toys detect a merge
/// that folds deltas in completion order rather than chain order — an ordinary
/// `+`/count accumulator stays correct under reordering and so hides the bug.
///
/// It records `first` (the chain-earliest height folded in) so the collapsed
/// batch value can be keyed by it: each batch persists one entry
/// `first_height -> joined_text`, and reading the namespace in key order
/// reassembles the whole chain.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConcatAcc {
    first: Option<BlockHeight>,
    text: String,
}

impl ConcatAcc {
    /// The identity: an empty concatenation.
    pub fn empty() -> Self {
        Self::default()
    }

    /// A single height.
    pub fn singleton(height: BlockHeight) -> Self {
        Self {
            first: Some(height),
            text: height.to_string(),
        }
    }

    /// `self` followed by `next` — associative, **not** commutative.
    pub fn followed_by(self, next: Self) -> Self {
        match (self.first, next.first) {
            (None, _) => next,
            (_, None) => self,
            (Some(first), _) => Self {
                first: Some(first),
                text: format!("{},{}", self.text, next.text),
            },
        }
    }

    /// Append one more height on the right (the `Fold` step).
    pub fn push_height(&mut self, height: BlockHeight) {
        if self.first.is_none() {
            self.first = Some(height);
            self.text = height.to_string();
        } else {
            self.text.push(',');
            self.text.push_str(&height.to_string());
        }
    }

    /// The chain-earliest height, if any — the key this batch persists under.
    pub fn first(&self) -> Option<BlockHeight> {
        self.first
    }

    /// The joined text (`"3,4,5"`).
    pub fn into_text(self) -> String {
        self.text
    }

    /// Reassemble an accumulator from persisted `(first_height, text)` entries,
    /// in chain order. The mechanical inverse of persisting each batch's
    /// collapsed value; only entry-order matters, so it sorts by height first.
    pub fn from_entries(mut entries: Vec<(BlockHeight, String)>) -> Self {
        entries.sort_by_key(|(height, _)| *height);
        entries
            .into_iter()
            .fold(Self::empty(), |acc, (first, text)| {
                acc.followed_by(Self {
                    first: Some(first),
                    text,
                })
            })
    }
}

/// On-disk record for a concat toy's joined-text value: the UTF-8 bytes,
/// length-framed.
#[derive(zaino_persistence_codec::PersistentRecord)]
pub struct PersistentConcat(Vec<u8>);

impl zaino_persistence_codec::PersistentRecord for PersistentConcat {
    type Domain = String;

    fn from_domain(domain: &String) -> Self {
        Self(domain.clone().into_bytes())
    }

    fn into_domain(self) -> Result<String, zaino_persistence_codec::DecodeError> {
        String::from_utf8(self.0)
            .map_err(|err| zaino_persistence_codec::DecodeError::Invalid(err.to_string()))
    }
}

// ---------------------------------------------------------------------------
// ProvideContext projections: set-wide → per-index
// ---------------------------------------------------------------------------

impl ProvideContext<value_index::Context> for TestBlockContext {
    fn context(&self) -> value_index::Context {
        value_index::Context {
            height: BlockHeight::new(self.height),
            value: value_index::BlockValue::new(self.value),
        }
    }
}

impl ProvideContext<()> for TestBlockContext {
    fn context(&self) {}
}

impl ProvideContext<running_sum_index::Context> for TestBlockContext {
    fn context(&self) -> running_sum_index::Context {
        running_sum_index::Context { value: self.value }
    }
}

impl ProvideContext<cumulative_sum_index::Context> for TestBlockContext {
    fn context(&self) -> cumulative_sum_index::Context {
        cumulative_sum_index::Context { value: self.value }
    }
}

impl ProvideContext<cumulative_series_index::Context> for TestBlockContext {
    fn context(&self) -> cumulative_series_index::Context {
        cumulative_series_index::Context {
            height: BlockHeight::new(self.height),
            value: self.value,
        }
    }
}

impl ProvideContext<concat_index::Context> for TestBlockContext {
    fn context(&self) -> concat_index::Context {
        concat_index::Context {
            height: BlockHeight::new(self.height),
        }
    }
}

impl ProvideContext<concat_fold_index::Context> for TestBlockContext {
    fn context(&self) -> concat_fold_index::Context {
        concat_fold_index::Context {
            height: BlockHeight::new(self.height),
        }
    }
}

/// Sync `n_blocks` (heights `0..n_blocks`) through a single concat toy index in
/// batches of `batch_size`, then read its namespace back as the heights joined
/// in chain order.
///
/// Extraction runs reversed (via [`ReverseExtractionGuard`]) so the merge sees
/// deltas out of chain order on every batch — the result equals the chain-order
/// concatenation only if the bridge reorders by offset before merging. Because
/// each batch collapses to one `first_height -> text` entry, reading the
/// namespace in key order reassembles the whole chain across batches.
///
/// [`ReverseExtractionGuard`]: crate::engine::ReverseExtractionGuard
#[cfg(test)]
pub(crate) fn run_toy_sync<I>(n_blocks: u64, batch_size: u32) -> String
where
    I: crate::pipeline::IntoIndexPipeline<TestBlockContext>
        + crate::traits::IndexDef
        + zaino_persistence_codec::EntryCodec<Key = BlockHeight, Value = String>,
{
    use crate::engine::{EngineConfig, ReverseExtractionGuard, SyncEngine};
    use crate::index_pipelines::IndexPipelines;
    use crate::testing::InMemoryBackend;

    let backend = InMemoryBackend::new();
    let set = IndexPipelines::new().with::<I>();
    let mut engine = SyncEngine::from_pipelines(
        set,
        backend.clone(),
        EngineConfig {
            batch_size,
            start_height: BlockHeight::new(0),
        },
    )
    .expect("valid index set");

    let blocks: Vec<_> = (0..n_blocks)
        .map(|height| TestBlockContext { height, value: 0 })
        .collect();

    {
        let _reversed = ReverseExtractionGuard::new();
        engine.sync_range(blocks).expect("sync succeeds");
    }

    let mut entries: Vec<(Vec<u8>, Vec<u8>)> =
        backend.entries(I::NAME.into()).into_iter().collect();
    // Keys are big-endian heights, so byte order is chain order.
    entries.sort_by(|(left, _), (right, _)| left.cmp(right));
    entries
        .into_iter()
        .map(|(_, value)| {
            zaino_persistence_codec::decode_value::<I>(&value).expect("concat value decodes")
        })
        .collect::<Vec<_>>()
        .join(",")
}

// ---------------------------------------------------------------------------
// End-to-end tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{EngineConfig, SyncEngine};
    use crate::index_pipelines::IndexPipelines;
    use crate::primitives::BlockHeight;
    use crate::provisioner::Provisioner;
    use crate::testing::{InMemoryBackend, MockProvisioner};

    use crate::primitives::BatchIndex;

    use count_index::CountIndex;
    use cumulative_series_index::CumulativeSeriesIndex;
    use cumulative_sum_index::CumulativeSumIndex;
    use running_sum_index::RunningSumIndex;
    use value_index::ValueIndex;

    /// Helper: build an engine from the three BlockLocal toy indexes.
    fn build_engine(
        backend: InMemoryBackend,
        batch_size: u32,
    ) -> SyncEngine<TestBlockContext, InMemoryBackend> {
        build_engine_at(backend, batch_size, BlockHeight::new(0))
    }

    /// Helper: build an engine from the three BlockLocal toy indexes
    /// starting at a given height.
    fn build_engine_at(
        backend: InMemoryBackend,
        batch_size: u32,
        start_height: BlockHeight,
    ) -> SyncEngine<TestBlockContext, InMemoryBackend> {
        let set = IndexPipelines::new()
            .with::<ValueIndex>()
            .with::<CountIndex>()
            .with::<RunningSumIndex>();

        SyncEngine::from_pipelines(
            set,
            backend,
            EngineConfig {
                batch_size,
                start_height,
            },
        )
        .expect("valid index set")
    }

    /// Helper: build an engine that includes the CumulativeSumIndex.
    fn build_engine_with_cumulative(
        backend: InMemoryBackend,
        batch_size: u32,
    ) -> SyncEngine<TestBlockContext, InMemoryBackend> {
        build_engine_with_cumulative_at(backend, batch_size, BlockHeight::new(0))
    }

    /// Helper: build an engine with cumulative index starting at a
    /// given height.
    fn build_engine_with_cumulative_at(
        backend: InMemoryBackend,
        batch_size: u32,
        start_height: BlockHeight,
    ) -> SyncEngine<TestBlockContext, InMemoryBackend> {
        let set = IndexPipelines::new()
            .with::<ValueIndex>()
            .with::<CountIndex>()
            .with::<RunningSumIndex>()
            .with::<CumulativeSumIndex>();

        SyncEngine::from_pipelines(
            set,
            backend,
            EngineConfig {
                batch_size,
                start_height,
            },
        )
        .expect("valid index set")
    }

    /// Read the cumulative sum from the backend.
    fn read_cumulative_sum(backend: &InMemoryBackend) -> u64 {
        let bytes = backend
            .get_value(cumulative_sum_index::ID.into(), b"sum")
            .expect("cumulative sum exists");
        u64::from_le_bytes(bytes.as_slice().try_into().expect("8 bytes"))
    }

    /// Helper: build an engine with the per-height (S, A) series index,
    /// starting at a given height.
    fn build_engine_with_series_at(
        backend: InMemoryBackend,
        batch_size: u32,
        start_height: BlockHeight,
    ) -> SyncEngine<TestBlockContext, InMemoryBackend> {
        let set = IndexPipelines::new().with::<CumulativeSeriesIndex>();
        SyncEngine::from_pipelines(
            set,
            backend,
            EngineConfig {
                batch_size,
                start_height,
            },
        )
        .expect("valid index set")
    }

    /// Read the whole (S, A) series as `height → running total`.
    fn read_series(backend: &InMemoryBackend) -> std::collections::BTreeMap<u64, u64> {
        backend
            .entries(cumulative_series_index::ID.into())
            .into_iter()
            .map(|(k, v)| {
                // The key is a `HeightKey`, which is big-endian (byte order == chain order).
                let height = u64::from_be_bytes(k.as_slice().try_into().expect("8-byte key"));
                let total = u64::from_le_bytes(v.as_slice().try_into().expect("8-byte value"));
                (height, total)
            })
            .collect()
    }

    #[test]
    fn end_to_end_single_batch() {
        let provisioner = MockProvisioner::identity();
        let blocks = provisioner
            .provision_range(BlockHeight::new(0), BlockHeight::new(4))
            .expect("provisioning succeeds");

        let backend = InMemoryBackend::new();
        let mut engine = build_engine(backend.clone(), 10);

        engine.sync_range(blocks).expect("sync succeeds");

        // ValueIndex: 5 entries (heights 0..=4), each height → height as value
        let values = backend.entries(value_index::ID.into());
        assert_eq!(values.len(), 5);
        for h in 0u64..=4 {
            // `HeightKey` encodes the height big-endian.
            let stored = values.get(h.to_be_bytes().as_slice()).expect("key exists");
            let val = u32::from_le_bytes(stored.as_slice().try_into().expect("4 bytes"));
            assert_eq!(val, h as u32);
        }

        // CountIndex: one entry "total" = 5
        let count_bytes = backend
            .get_value(count_index::ID.into(), b"total")
            .expect("count exists");
        let count = u64::from_le_bytes(count_bytes.as_slice().try_into().expect("8 bytes"));
        assert_eq!(count, 5);

        // RunningSumIndex: one entry "sum" = 0+1+2+3+4 = 10
        let sum_bytes = backend
            .get_value(running_sum_index::ID.into(), b"sum")
            .expect("sum exists");
        let sum = u64::from_le_bytes(sum_bytes.as_slice().try_into().expect("8 bytes"));
        assert_eq!(sum, 10);
    }

    #[test]
    fn multi_batch_splits_correctly() {
        let provisioner = MockProvisioner::identity();
        let blocks = provisioner
            .provision_range(BlockHeight::new(0), BlockHeight::new(9))
            .expect("provisioning succeeds");

        let backend = InMemoryBackend::new();
        // Batch size 3: blocks [0,1,2], [3,4,5], [6,7,8], [9]
        let mut engine = build_engine(backend.clone(), 3);

        engine.sync_range(blocks).expect("sync succeeds");

        // ValueIndex: 10 entries, all correct (append across batches)
        let values = backend.entries(value_index::ID.into());
        assert_eq!(values.len(), 10);

        // CountIndex: last batch was [9] (1 block), so count = 1.
        // Monoidal merge runs per-batch, and each batch overwrites the
        // same "total" key — the final value reflects the last batch.
        let count_bytes = backend
            .get_value(count_index::ID.into(), b"total")
            .expect("count exists");
        let count = u64::from_le_bytes(count_bytes.as_slice().try_into().expect("8 bytes"));
        assert_eq!(count, 1);

        // RunningSumIndex: last batch was [9], fold sum = 9.
        // Same overwrite semantics as CountIndex.
        let sum_bytes = backend
            .get_value(running_sum_index::ID.into(), b"sum")
            .expect("sum exists");
        let sum = u64::from_le_bytes(sum_bytes.as_slice().try_into().expect("8 bytes"));
        assert_eq!(sum, 9);
    }

    #[test]
    fn streaming_iterator_produces_same_results() {
        let backend = InMemoryBackend::new();
        let mut engine = build_engine(backend.clone(), 3);

        let blocks = (0u64..=9).map(|h| TestBlockContext {
            height: h,
            value: h as u32,
        });

        engine.sync_streaming(blocks).expect("sync succeeds");

        // Incremental arrival produces the same entry count as pre-loaded.
        assert_eq!(backend.entries(value_index::ID.into()).len(), 10);
        assert!(backend
            .get_value(count_index::ID.into(), b"total")
            .is_some());
        assert!(backend
            .get_value(running_sum_index::ID.into(), b"sum")
            .is_some());

        assert_eq!(engine.buffer_len(), 0);
        assert_eq!(engine.evicted_through(), Some(BatchIndex::new(3)));
    }

    #[tokio::test]
    async fn async_channel_produces_same_results() {
        let backend = InMemoryBackend::new();
        let mut engine = build_engine(backend.clone(), 3);

        let (tx, rx) = tokio::sync::mpsc::channel(16);

        tokio::spawn(async move {
            for h in 0u64..=9 {
                tx.send(TestBlockContext {
                    height: h,
                    value: h as u32,
                })
                .await
                .expect("channel open");
            }
        });

        engine.sync_channel(rx).await.expect("sync succeeds");

        assert_eq!(backend.entries(value_index::ID.into()).len(), 10);
        assert!(backend
            .get_value(count_index::ID.into(), b"total")
            .is_some());
        assert!(backend
            .get_value(running_sum_index::ID.into(), b"sum")
            .is_some());
        assert_eq!(engine.buffer_len(), 0);
        assert_eq!(engine.evicted_through(), Some(BatchIndex::new(3)));
    }

    /// Under `sync-profile`, a streamed multi-batch sync emits exactly one
    /// per-batch profile per committed batch, and the per-index op counts
    /// recorded match what each batch actually committed.
    #[cfg(feature = "sync-profile")]
    #[tokio::test]
    async fn sync_profile_records_one_entry_per_committed_batch() {
        let backend = InMemoryBackend::new();
        // Batch size 3 over heights 0..=9: batches [0,1,2] [3,4,5] [6,7,8] [9].
        let mut engine = build_engine(backend.clone(), 3);

        let (tx, rx) = tokio::sync::mpsc::channel(16);
        tokio::spawn(async move {
            for h in 0u64..=9 {
                tx.send(TestBlockContext {
                    height: h,
                    value: h as u32,
                })
                .await
                .expect("channel open");
            }
        });

        engine.sync_channel(rx).await.expect("sync succeeds");

        /// The op count recorded for `id` in a batch's per-index samples.
        fn ops_for(
            record: &crate::profile::BatchProfileRecord,
            id: crate::primitives::IndexId,
        ) -> usize {
            record
                .merge_persist
                .iter()
                .find(|(index, _, _)| *index == id)
                .map(|(_, _, ops)| *ops)
                .unwrap_or_else(|| panic!("batch {} has no sample for {id}", record.batch))
        }

        let records = engine.profile_records();

        // One profile per committed batch — four batches, in order.
        let batches: Vec<u32> = records.iter().map(|r| r.batch).collect();
        assert_eq!(batches, vec![0, 1, 2, 3], "one entry per committed batch");

        // Block counts and watermarks of each batch.
        let blocks: Vec<u64> = records.iter().map(|r| r.blocks).collect();
        assert_eq!(blocks, vec![3, 3, 3, 1]);
        let heights: Vec<u64> = records.iter().map(|r| r.committed_height).collect();
        assert_eq!(heights, vec![2, 5, 8, 9]);

        // Per-index op counts match what each batch committed. Every batch's
        // persist prepends one per-namespace version stamp, then the data
        // entries: ValueIndex appends one entry per block; the Monoidal count
        // and Fold sum each collapse to a single entry.
        for record in records {
            let blocks = usize::try_from(record.blocks).expect("block count fits usize");
            assert_eq!(
                ops_for(record, value_index::ID),
                blocks + 1,
                "value index writes a stamp + one op per block in batch {}",
                record.batch
            );
            assert_eq!(ops_for(record, count_index::ID), 2);
            assert_eq!(ops_for(record, running_sum_index::ID), 2);
            assert_eq!(
                record.merge_persist.len(),
                3,
                "all three indexes sampled for batch {}",
                record.batch
            );
            // Every timing field is populated and sane.
            assert!(record.committed_height >= u64::from(record.batch));
            assert!(record.wait_ms >= 0.0);
            assert!(record.extract_ms >= 0.0);
            assert!(record.merge_persist_wall_ms >= 0.0);
            assert!(record.commit_ms >= 0.0);
            assert!(record.window_ms >= 0.0);
            // Residual is computed against the merge+persist WALL time, not the
            // sum of the (overlapping) per-index samples. With three indexes
            // running in parallel that sum exceeds the wall time, so a
            // sum-based residual would go sharply negative; the wall-based one
            // stays non-negative but for sub-millisecond scheduling slop.
            assert!(
                record.residual_ms >= -0.5,
                "wall-based residual stays ~non-negative, got {} for batch {}",
                record.residual_ms,
                record.batch
            );
        }
    }

    #[test]
    fn buffer_evicted_during_multi_batch_sync() {
        let provisioner = MockProvisioner::identity();
        let blocks = provisioner
            .provision_range(BlockHeight::new(0), BlockHeight::new(9))
            .expect("provisioning succeeds");

        let backend = InMemoryBackend::new();
        // Batch size 3: batches [0,1,2], [3,4,5], [6,7,8], [9].
        let mut engine = build_engine(backend, 3);

        engine.sync_range(blocks).expect("sync succeeds");

        // All blocks should be evicted — buffer empty.
        assert_eq!(engine.buffer_len(), 0);
        // Eviction frontier covers all 4 batches (0..=3).
        assert_eq!(engine.evicted_through(), Some(BatchIndex::new(3)));
    }

    // -----------------------------------------------------------------------
    // SelfCumulative tests
    // -----------------------------------------------------------------------

    #[test]
    fn cumulative_sum_single_batch() {
        // Blocks 0..=6, values = heights, all in one batch.
        // Threshold = 10. Prior sums: 0,0,1,3,6,10,15
        //   block 0: prior=0,  delta=0          → sum=0
        //   block 1: prior=0,  delta=1          → sum=1
        //   block 2: prior=1,  delta=2          → sum=3
        //   block 3: prior=3,  delta=3          → sum=6
        //   block 4: prior=6,  delta=4          → sum=10
        //   block 5: prior=10, delta=5          → sum=15
        //   block 6: prior=15, delta=6*2=12     → sum=27
        //
        // Only block 6 exceeds the threshold (prior=15 > 10).
        let provisioner = MockProvisioner::identity();
        let blocks = provisioner
            .provision_range(BlockHeight::new(0), BlockHeight::new(6))
            .expect("provisioning succeeds");

        let backend = InMemoryBackend::new();
        let mut engine = build_engine_with_cumulative(backend.clone(), 20);

        engine.sync_range(blocks).expect("sync succeeds");

        assert_eq!(read_cumulative_sum(&backend), 27);
    }

    #[test]
    fn cumulative_sum_deterministic_across_batch_sizes() {
        // The cumulative result must be identical regardless of batch
        // boundaries. This is the key property of SelfCumulative: the
        // running state threads correctly across batches.
        let provisioner = MockProvisioner::identity();

        let expected = {
            let blocks = provisioner
                .provision_range(BlockHeight::new(0), BlockHeight::new(6))
                .expect("provisioning succeeds");
            let backend = InMemoryBackend::new();
            let mut engine = build_engine_with_cumulative(backend.clone(), 20);
            engine.sync_range(blocks).expect("sync succeeds");
            read_cumulative_sum(&backend)
        };

        for batch_size in [1, 2, 3, 4, 5, 7] {
            let blocks = provisioner
                .provision_range(BlockHeight::new(0), BlockHeight::new(6))
                .expect("provisioning succeeds");
            let backend = InMemoryBackend::new();
            let mut engine = build_engine_with_cumulative(backend.clone(), batch_size);
            engine.sync_range(blocks).expect("sync succeeds");

            assert_eq!(
                read_cumulative_sum(&backend),
                expected,
                "batch_size={batch_size} produced different result"
            );
        }
    }

    #[test]
    fn cumulative_sum_state_threads_across_batches() {
        // Batch size 3: batches [0,1,2], [3,4,5], [6].
        // After batch 0: sum = 0+1+2 = 3 (no doubling, all priors ≤ 10)
        // After batch 1: sum = 3+3+4+5 = 15 (no doubling, priors 3,6,10 ≤ 10)
        // After batch 2: sum = 15 + 6*2 = 27 (block 6: prior=15 > 10, doubled)
        let provisioner = MockProvisioner::identity();
        let blocks = provisioner
            .provision_range(BlockHeight::new(0), BlockHeight::new(6))
            .expect("provisioning succeeds");

        let backend = InMemoryBackend::new();
        let mut engine = build_engine_with_cumulative(backend.clone(), 3);

        engine.sync_range(blocks).expect("sync succeeds");

        assert_eq!(read_cumulative_sum(&backend), 27);
    }

    #[test]
    fn cumulative_sum_resumes_from_backend() {
        // Sync blocks 0..=4, drop the engine, build a new one on the
        // same backend, sync blocks 5..=6. The new engine must load
        // the committed accumulator so that extraction sees the correct
        // prior state (and triggers doubling at the right threshold).
        //
        // Phase 1 (blocks 0..=4):
        //   block 0: prior=0,  delta=0  → sum=0
        //   block 1: prior=0,  delta=1  → sum=1
        //   block 2: prior=1,  delta=2  → sum=3
        //   block 3: prior=3,  delta=3  → sum=6
        //   block 4: prior=6,  delta=4  → sum=10
        //
        // Phase 2 (blocks 5..=6, new engine, loaded prior=10):
        //   block 5: prior=10, delta=5  → sum=15   (10 is NOT > 10)
        //   block 6: prior=15, delta=12 → sum=27   (15 > 10, doubled)
        //
        // Without load_state the new engine would start from prior=0,
        // and the result would be 0+5+6 = 11 — wrong.
        let backend = InMemoryBackend::new();

        // Phase 1.
        {
            let blocks: Vec<_> = (0u64..=4)
                .map(|h| TestBlockContext {
                    height: h,
                    value: h as u32,
                })
                .collect();
            let mut engine = build_engine_with_cumulative(backend.clone(), 20);
            engine.sync_range(blocks).expect("phase 1 sync succeeds");
            assert_eq!(read_cumulative_sum(&backend), 10);
        }

        // Watermark should reflect phase 1.
        let watermark = SyncEngine::<TestBlockContext, _>::committed_height(&backend)
            .expect("read succeeds")
            .expect("watermark exists");
        assert_eq!(watermark, BlockHeight::new(4));

        // Phase 2: new engine, same backend, starting from watermark + 1.
        {
            let start = BlockHeight::new(watermark.value() + 1);
            let blocks: Vec<_> = (5u64..=6)
                .map(|h| TestBlockContext {
                    height: h,
                    value: h as u32,
                })
                .collect();
            let mut engine = build_engine_with_cumulative_at(backend.clone(), 20, start);
            engine.sync_range(blocks).expect("phase 2 sync succeeds");
            assert_eq!(read_cumulative_sum(&backend), 27);
        }

        // Watermark should now reflect phase 2.
        let watermark = SyncEngine::<TestBlockContext, _>::committed_height(&backend)
            .expect("read succeeds")
            .expect("watermark exists");
        assert_eq!(watermark, BlockHeight::new(6));
    }

    #[test]
    fn watermark_advances_per_batch() {
        // Batch size 3, blocks 0..=9 → batches [0,1,2], [3,4,5], [6,7,8], [9].
        // After sync, watermark should be 9.
        let backend = InMemoryBackend::new();
        let mut engine = build_engine(backend.clone(), 3);

        let blocks: Vec<_> = (0u64..=9)
            .map(|h| TestBlockContext {
                height: h,
                value: h as u32,
            })
            .collect();
        engine.sync_range(blocks).expect("sync succeeds");

        let watermark = SyncEngine::<TestBlockContext, _>::committed_height(&backend)
            .expect("read succeeds")
            .expect("watermark exists");
        assert_eq!(watermark, BlockHeight::new(9));
    }

    /// One engine, two channel syncs, the first ending on a partial batch: the
    /// watermark after the second is the last height actually indexed. This is
    /// the steady-state follow shape — catch up, then extend a few blocks per
    /// tip change — and the stamp must come from heights, not buffer offsets,
    /// because eviction rounds the buffer floor up to a batch boundary.
    #[tokio::test]
    async fn watermark_stays_a_height_across_channel_syncs() {
        let backend = InMemoryBackend::new();
        let mut engine = build_engine(backend.clone(), 4);

        for range in [0u64..=2, 3u64..=4, 5u64..=5] {
            let (tx, rx) = tokio::sync::mpsc::channel(16);
            let blocks: Vec<_> = range
                .map(|h| TestBlockContext {
                    height: h,
                    value: h as u32,
                })
                .collect();
            tokio::spawn(async move {
                for block in blocks {
                    tx.send(block).await.expect("channel open");
                }
            });
            engine.sync_channel(rx).await.expect("sync succeeds");
        }

        let watermark = SyncEngine::<TestBlockContext, _>::committed_height(&backend)
            .expect("read succeeds")
            .expect("watermark exists");
        assert_eq!(watermark, BlockHeight::new(5));
        assert_eq!(backend.entries(value_index::ID.into()).len(), 6);
    }

    #[test]
    fn watermark_none_on_fresh_backend() {
        let backend = InMemoryBackend::new();
        let watermark = SyncEngine::<TestBlockContext, InMemoryBackend>::committed_height(&backend)
            .expect("read succeeds");
        assert!(watermark.is_none());
    }

    /// The (S, A) bridge keeps the *whole* per-height series (not a collapsed
    /// total), and the result is identical whichever batch size the run uses —
    /// proof the carry threads across batch boundaries without the series being
    /// rewritten or lost.
    #[test]
    fn series_retains_per_height_and_is_batch_invariant() {
        // value(h) = h, so running(h) = 0+1+...+h.
        let blocks = || -> Vec<_> {
            (0u64..=5)
                .map(|h| TestBlockContext {
                    height: h,
                    value: u32::try_from(h).expect("height fits u32"),
                })
                .collect()
        };
        let expected: std::collections::BTreeMap<u64, u64> =
            [(0, 0), (1, 1), (2, 3), (3, 6), (4, 10), (5, 15)].into();

        for batch_size in [2, 10] {
            let backend = InMemoryBackend::new();
            let mut engine =
                build_engine_with_series_at(backend.clone(), batch_size, BlockHeight::new(0));
            engine.sync_range(blocks()).expect("sync succeeds");
            assert_eq!(
                read_series(&backend),
                expected,
                "per-height series must survive batch_size={batch_size}"
            );
        }
    }

    /// A restart resumes the carry by point-reading the value at the watermark
    /// height — phase 2 continues the running total from where phase 1 stopped,
    /// and phase 1's entries are untouched.
    #[test]
    fn series_resumes_carry_from_watermark() {
        let backend = InMemoryBackend::new();

        // Phase 1: heights 0..=2, values 0,1,2 → totals 0,1,3.
        {
            let blocks: Vec<_> = (0u64..=2)
                .map(|h| TestBlockContext {
                    height: h,
                    value: u32::try_from(h).expect("height fits u32"),
                })
                .collect();
            let mut engine = build_engine_with_series_at(backend.clone(), 10, BlockHeight::new(0));
            engine.sync_range(blocks).expect("phase 1 sync succeeds");
        }
        let watermark = SyncEngine::<TestBlockContext, _>::committed_height(&backend)
            .expect("read succeeds")
            .expect("watermark exists");
        assert_eq!(watermark, BlockHeight::new(2));

        // Phase 2: new engine on the same backend resumes the carry (=3, the
        // value at height 2) and continues: h3 = 3+3 = 6, h4 = 6+4 = 10.
        {
            let start = BlockHeight::new(watermark.value() + 1);
            let blocks: Vec<_> = (3u64..=4)
                .map(|h| TestBlockContext {
                    height: h,
                    value: u32::try_from(h).expect("height fits u32"),
                })
                .collect();
            let mut engine = build_engine_with_series_at(backend.clone(), 10, start);
            engine.sync_range(blocks).expect("phase 2 sync succeeds");
        }

        let expected: std::collections::BTreeMap<u64, u64> =
            [(0, 0), (1, 1), (2, 3), (3, 6), (4, 10)].into();
        assert_eq!(
            read_series(&backend),
            expected,
            "resumed series must continue the running total, not restart from zero"
        );
    }
}
