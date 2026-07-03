//! Toy index set: three indexes demonstrating the three composition types.
//!
//! Each index lives in its own sub-module and declares a narrow
//! [`BlockContext`](crate::traits::IndexDef::BlockContext). The set-wide
//! `TestBlockContext` projects into each via [`ProvideContext`].
//!
//! [`ProvideContext`]: crate::traits::ProvideContext

pub mod count_index;
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
        running_sum_index::Context {
            value: self.value,
        }
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

    use count_index::CountIndex;
    use running_sum_index::RunningSumIndex;
    use value_index::ValueIndex;

    /// Helper: build an engine from the three toy indexes.
    fn build_engine(
        backend: InMemoryBackend,
        batch_size: u32,
    ) -> SyncEngine<TestBlockContext, InMemoryBackend> {
        let set = IndexSet::new()
            .with::<ValueIndex>()
            .with::<CountIndex>()
            .with::<RunningSumIndex>();

        SyncEngine::from_index_set(set, backend, EngineConfig { batch_size })
            .expect("valid index set")
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
        let values = backend.entries(value_index::ID);
        assert_eq!(values.len(), 5);
        for h in 0u64..=4 {
            let stored = values.get(&h.to_le_bytes().to_vec()).expect("key exists");
            let val = u32::from_le_bytes(stored.as_slice().try_into().expect("4 bytes"));
            assert_eq!(val, h as u32);
        }

        // CountIndex: one entry "total" = 5
        let count_bytes = backend
            .get_value(count_index::ID, b"total")
            .expect("count exists");
        let count = u64::from_le_bytes(count_bytes.as_slice().try_into().expect("8 bytes"));
        assert_eq!(count, 5);

        // RunningSumIndex: one entry "sum" = 0+1+2+3+4 = 10
        let sum_bytes = backend
            .get_value(running_sum_index::ID, b"sum")
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
        let values = backend.entries(value_index::ID);
        assert_eq!(values.len(), 10);

        // CountIndex: last batch was [9] (1 block), so count = 1.
        // Monoidal merge runs per-batch, and each batch overwrites the
        // same "total" key — the final value reflects the last batch.
        let count_bytes = backend
            .get_value(count_index::ID, b"total")
            .expect("count exists");
        let count = u64::from_le_bytes(count_bytes.as_slice().try_into().expect("8 bytes"));
        assert_eq!(count, 1);

        // RunningSumIndex: last batch was [9], fold sum = 9.
        // Same overwrite semantics as CountIndex.
        let sum_bytes = backend
            .get_value(running_sum_index::ID, b"sum")
            .expect("sum exists");
        let sum = u64::from_le_bytes(sum_bytes.as_slice().try_into().expect("8 bytes"));
        assert_eq!(sum, 9);
    }
}
