//! Test utilities: in-memory backend, mock provisioner, and toy indexes.
//!
//! Everything in this module is `#[cfg(test)]`-gated.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::backend::{Backend, BackendError, BackendReader, BackendWriter, WriterTopology};
use crate::primitives::{BlockHeight, IndexId};
use crate::traits::WriteOp;

// ===========================================================================
// In-memory backend
// ===========================================================================

/// In-memory backend for tests. Stores key-value pairs per index.
///
/// Thread-safe via `Arc<Mutex<...>>` — readers and writers share the
/// same underlying map. Not performant, but correct for test assertions.
#[derive(Clone)]
pub struct InMemoryBackend {
    data: Arc<Mutex<HashMap<IndexId, HashMap<Vec<u8>, Vec<u8>>>>>,
}

impl InMemoryBackend {
    pub fn new() -> Self {
        Self {
            data: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Read all entries for a given index. For test assertions.
    pub fn entries(&self, index: IndexId) -> HashMap<Vec<u8>, Vec<u8>> {
        let guard = self.data.lock().expect("test mutex poisoned");
        guard.get(&index).cloned().unwrap_or_default()
    }

    /// Read a single value. For test assertions.
    pub fn get_value(&self, index: IndexId, key: &[u8]) -> Option<Vec<u8>> {
        let guard = self.data.lock().expect("test mutex poisoned");
        guard.get(&index).and_then(|m| m.get(key).cloned())
    }
}

impl Backend for InMemoryBackend {
    type Reader = InMemoryReader;
    type Writer = InMemoryWriter;

    fn reader(&self) -> Result<Self::Reader, BackendError> {
        Ok(InMemoryReader {
            data: Arc::clone(&self.data),
        })
    }

    fn writer(&self) -> Result<Self::Writer, BackendError> {
        Ok(InMemoryWriter {
            data: Arc::clone(&self.data),
        })
    }

    fn flush(&self) -> Result<(), BackendError> {
        Ok(())
    }

    fn topology(&self) -> WriterTopology {
        WriterTopology::SharedWriter
    }
}

/// Read handle for the in-memory backend.
pub struct InMemoryReader {
    data: Arc<Mutex<HashMap<IndexId, HashMap<Vec<u8>, Vec<u8>>>>>,
}

impl BackendReader for InMemoryReader {
    fn get(&self, index: IndexId, key: &[u8]) -> Result<Option<Vec<u8>>, BackendError> {
        let guard = self.data.lock().expect("test mutex poisoned");
        Ok(guard.get(&index).and_then(|m| m.get(key).cloned()))
    }
}

/// Write handle for the in-memory backend.
pub struct InMemoryWriter {
    data: Arc<Mutex<HashMap<IndexId, HashMap<Vec<u8>, Vec<u8>>>>>,
}

impl BackendWriter for InMemoryWriter {
    fn commit(&mut self, ops: Vec<WriteOp>) -> Result<(), BackendError> {
        let mut guard = self.data.lock().expect("test mutex poisoned");
        for op in ops {
            match op {
                WriteOp::Put { index, key, value } => {
                    guard.entry(index).or_default().insert(key, value);
                }
                WriteOp::Delete { index, key } => {
                    if let Some(map) = guard.get_mut(&index) {
                        map.remove(&key);
                    }
                }
            }
        }
        Ok(())
    }
}

// ===========================================================================
// Mock provisioner
// ===========================================================================

use crate::descriptor::SourceRequirements;
use crate::provisioner::{ProvisionError, Provisioner};

/// Minimal block context for tests.
#[derive(Debug, Clone)]
pub struct TestBlockContext {
    /// Block height.
    pub height: u64,
    /// Arbitrary value carried by this block.
    pub value: u32,
}

/// Mock provisioner that generates `TestBlockContext`s with predictable values.
pub struct MockProvisioner {
    /// Function that produces the value for a given height.
    value_fn: Box<dyn Fn(u64) -> u32 + Send + Sync>,
}

impl MockProvisioner {
    /// Create a provisioner where each block's value equals its height.
    pub fn identity() -> Self {
        Self {
            value_fn: Box::new(|h| h as u32),
        }
    }
}

impl Provisioner for MockProvisioner {
    type BlockContext = TestBlockContext;

    fn configure(&mut self, _requirements: SourceRequirements) {}

    fn provision_range(
        &self,
        from: BlockHeight,
        to: BlockHeight,
    ) -> Result<Vec<Self::BlockContext>, ProvisionError> {
        let blocks = (from.value()..=to.value())
            .map(|h| TestBlockContext {
                height: h,
                value: (self.value_fn)(h),
            })
            .collect();
        Ok(blocks)
    }
}

// ===========================================================================
// Test indexes
// ===========================================================================

use crate::descriptor::{
    Append, BlockLocal, CompositionType, Descriptor, Fold, InputScope, Monoidal, SourceAccess,
};
use crate::traits::{ExtractError, ExtractLocal, IndexDef, MergeAppend, MergeFold, MergeMonoidal};

/// BlockLocal × Append: stores (height → value) for each block.
pub struct ValueIndex;

const VALUE_INDEX_ID: IndexId = IndexId::new("value");

impl IndexDef for ValueIndex {
    type Scope = BlockLocal;
    type Composition = Append;
    type Delta = Vec<(Vec<u8>, Vec<u8>)>;
    type BlockContext = TestBlockContext;

    fn descriptor() -> Descriptor {
        Descriptor {
            name: VALUE_INDEX_ID,
            scope: InputScope::BlockLocal,
            composition: CompositionType::Append,
            dependencies: &[],
            requirements: SourceRequirements::BLOCK,
            source_access: SourceAccess::None,
        }
    }
}

impl ExtractLocal for ValueIndex {
    fn extract(ctx: &TestBlockContext) -> Result<Self::Delta, ExtractError> {
        Ok(vec![(
            ctx.height.to_le_bytes().to_vec(),
            ctx.value.to_le_bytes().to_vec(),
        )])
    }
}

impl MergeAppend for ValueIndex {
    fn to_write_ops(delta: Self::Delta) -> Vec<WriteOp> {
        delta
            .into_iter()
            .map(|(key, value)| WriteOp::Put {
                index: VALUE_INDEX_ID,
                key,
                value,
            })
            .collect()
    }
}

/// BlockLocal × Monoidal: counts total blocks seen in each batch.
pub struct CountIndex;

const COUNT_INDEX_ID: IndexId = IndexId::new("count");

impl IndexDef for CountIndex {
    type Scope = BlockLocal;
    type Composition = Monoidal;
    type Delta = u64;
    type BlockContext = TestBlockContext;

    fn descriptor() -> Descriptor {
        Descriptor {
            name: COUNT_INDEX_ID,
            scope: InputScope::BlockLocal,
            composition: CompositionType::Monoidal,
            dependencies: &[],
            requirements: SourceRequirements::BLOCK,
            source_access: SourceAccess::None,
        }
    }
}

impl ExtractLocal for CountIndex {
    fn extract(_ctx: &TestBlockContext) -> Result<Self::Delta, ExtractError> {
        Ok(1)
    }
}

impl MergeMonoidal for CountIndex {
    type Accumulator = u64;

    fn identity() -> Self::Accumulator {
        0
    }

    fn lift(delta: Self::Delta) -> Self::Accumulator {
        delta
    }

    fn combine(a: Self::Accumulator, b: Self::Accumulator) -> Self::Accumulator {
        a + b
    }

    fn to_write_ops(merged: Self::Accumulator) -> Vec<WriteOp> {
        vec![WriteOp::Put {
            index: COUNT_INDEX_ID,
            key: b"total".to_vec(),
            value: merged.to_le_bytes().to_vec(),
        }]
    }
}

/// BlockLocal × Fold: running sum of values across blocks in a batch.
pub struct RunningSumIndex;

const RUNNING_SUM_INDEX_ID: IndexId = IndexId::new("running_sum");

impl IndexDef for RunningSumIndex {
    type Scope = BlockLocal;
    type Composition = Fold;
    type Delta = u64;
    type BlockContext = TestBlockContext;

    fn descriptor() -> Descriptor {
        Descriptor {
            name: RUNNING_SUM_INDEX_ID,
            scope: InputScope::BlockLocal,
            composition: CompositionType::Fold,
            dependencies: &[],
            requirements: SourceRequirements::BLOCK,
            source_access: SourceAccess::None,
        }
    }
}

impl ExtractLocal for RunningSumIndex {
    fn extract(ctx: &TestBlockContext) -> Result<Self::Delta, ExtractError> {
        Ok(u64::from(ctx.value))
    }
}

impl MergeFold for RunningSumIndex {
    type FoldState = u64;

    fn initial_state() -> Self::FoldState {
        0
    }

    fn fold(state: &mut Self::FoldState, delta: Self::Delta) {
        *state += delta;
    }

    fn to_write_ops(state: Self::FoldState) -> Vec<WriteOp> {
        vec![WriteOp::Put {
            index: RUNNING_SUM_INDEX_ID,
            key: b"sum".to_vec(),
            value: state.to_le_bytes().to_vec(),
        }]
    }
}

// ===========================================================================
// End-to-end tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{EngineConfig, SyncEngine};
    use crate::index_set::IndexSet;

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

        engine.sync_range(&blocks).expect("sync succeeds");

        // ValueIndex: 5 entries (heights 0..=4), each height → height as value
        let values = backend.entries(VALUE_INDEX_ID);
        assert_eq!(values.len(), 5);
        for h in 0u64..=4 {
            let stored = values.get(&h.to_le_bytes().to_vec()).expect("key exists");
            let val = u32::from_le_bytes(stored.as_slice().try_into().expect("4 bytes"));
            assert_eq!(val, h as u32);
        }

        // CountIndex: one entry "total" = 5
        let count_bytes = backend
            .get_value(COUNT_INDEX_ID, b"total")
            .expect("count exists");
        let count = u64::from_le_bytes(count_bytes.as_slice().try_into().expect("8 bytes"));
        assert_eq!(count, 5);

        // RunningSumIndex: one entry "sum" = 0+1+2+3+4 = 10
        let sum_bytes = backend
            .get_value(RUNNING_SUM_INDEX_ID, b"sum")
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

        engine.sync_range(&blocks).expect("sync succeeds");

        // ValueIndex: 10 entries, all correct (append across batches)
        let values = backend.entries(VALUE_INDEX_ID);
        assert_eq!(values.len(), 10);

        // CountIndex: last batch was [9] (1 block), so count = 1.
        // Monoidal merge runs per-batch, and each batch overwrites the
        // same "total" key — the final value reflects the last batch.
        let count_bytes = backend
            .get_value(COUNT_INDEX_ID, b"total")
            .expect("count exists");
        let count = u64::from_le_bytes(count_bytes.as_slice().try_into().expect("8 bytes"));
        assert_eq!(count, 1);

        // RunningSumIndex: last batch was [9], fold sum = 9.
        // Same overwrite semantics as CountIndex.
        let sum_bytes = backend
            .get_value(RUNNING_SUM_INDEX_ID, b"sum")
            .expect("sum exists");
        let sum = u64::from_le_bytes(sum_bytes.as_slice().try_into().expect("8 bytes"));
        assert_eq!(sum, 9);
    }
}
