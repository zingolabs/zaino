//! Toy index set: four indexes demonstrating composition and scope types.
//!
//! Each index lives in its own sub-module and declares a narrow
//! [`BlockContext`](crate::traits::IndexDef::BlockContext). The set-wide
//! `TestBlockContext` projects into each via [`ProvideContext`].
//!
//! BlockLocal indexes: [`ValueIndex`](value_index), [`CountIndex`](count_index),
//! [`RunningSumIndex`](running_sum_index).
//!
//! SelfCumulative indexes: [`CumulativeSumIndex`](cumulative_sum_index).
//!
//! [`ProvideContext`]: crate::traits::ProvideContext

pub mod count_index;
pub mod cumulative_sum_index;
pub mod running_sum_index;
pub mod value_index;

use crate::primitives::BlockHeight;
use crate::traits::ProvideContext;

use super::TestBlockContext;

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

// ---------------------------------------------------------------------------
// End-to-end tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{EngineConfig, SyncEngine};
    use crate::index_set::IndexSet;
    use crate::primitives::BlockHeight;
    use crate::provisioner::Provisioner;
    use crate::testing::{InMemoryBackend, MockProvisioner};

    use crate::primitives::BatchIndex;

    use count_index::CountIndex;
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
        let set = IndexSet::new()
            .with::<ValueIndex>()
            .with::<CountIndex>()
            .with::<RunningSumIndex>();

        SyncEngine::from_index_set(
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
        let set = IndexSet::new()
            .with::<ValueIndex>()
            .with::<CountIndex>()
            .with::<RunningSumIndex>()
            .with::<CumulativeSumIndex>();

        SyncEngine::from_index_set(
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
            let stored = values.get(&h.to_le_bytes().to_vec()).expect("key exists");
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

    #[test]
    fn watermark_none_on_fresh_backend() {
        let backend = InMemoryBackend::new();
        let watermark = SyncEngine::<TestBlockContext, InMemoryBackend>::committed_height(&backend)
            .expect("read succeeds");
        assert!(watermark.is_none());
    }
}
