//! Test utilities: in-memory backend, mock provisioner, and demo index sets.
//!
//! General-purpose utilities (`InMemoryBackend`, `MockProvisioner`,
//! `TestBlockContext`) are defined here. Specific index sets live in
//! sub-modules (e.g. [`toy_indexes`]).

mod toy_indexes;

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
// Set-wide block context and mock provisioner
// ===========================================================================

use crate::descriptor::SourceRequirements;
use crate::provisioner::{ProvisionError, Provisioner};

/// Set-wide block context for tests.
///
/// The provisioner produces one of these per block. Individual indexes
/// declare narrower [`BlockContext`](crate::traits::IndexDef::BlockContext)
/// types and receive projections via [`ProvideContext`](crate::traits::ProvideContext).
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
