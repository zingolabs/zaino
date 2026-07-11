//! In-memory backend: correct, no IO, data lost on drop.
//!
//! Useful for tests, benchmarks, and ephemeral demo runs.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use zaino_primitives::types::IndexId;

use crate::backend::{WriteOp, Backend, BackendReader, BackendWriter};
use crate::error::BackendError;

/// In-memory backend. Stores key-value pairs per index.
///
/// Thread-safe via `Arc<Mutex<...>>` — readers and writers share the
/// same underlying map.
#[derive(Clone)]
pub struct InMemoryBackend {
    data: Arc<Mutex<HashMap<IndexId, HashMap<Vec<u8>, Vec<u8>>>>>,
}

impl InMemoryBackend {
    /// Create an empty backend.
    pub fn new() -> Self {
        Self {
            data: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Read all entries for a given index. For assertions.
    pub fn entries(&self, index: IndexId) -> HashMap<Vec<u8>, Vec<u8>> {
        let guard = self.data.lock().expect("mutex poisoned");
        guard.get(&index).cloned().unwrap_or_default()
    }

    /// Read a single value. For assertions.
    pub fn get_value(&self, index: IndexId, key: &[u8]) -> Option<Vec<u8>> {
        let guard = self.data.lock().expect("mutex poisoned");
        guard.get(&index).and_then(|m| m.get(key).cloned())
    }
}

impl Default for InMemoryBackend {
    fn default() -> Self {
        Self::new()
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
}

/// Read handle for the in-memory backend.
pub struct InMemoryReader {
    data: Arc<Mutex<HashMap<IndexId, HashMap<Vec<u8>, Vec<u8>>>>>,
}

impl BackendReader for InMemoryReader {
    fn get(&self, index: IndexId, key: &[u8]) -> Result<Option<Vec<u8>>, BackendError> {
        let guard = self.data.lock().expect("mutex poisoned");
        Ok(guard.get(&index).and_then(|m| m.get(key).cloned()))
    }

    fn scan(&self, index: IndexId) -> Result<Vec<(Vec<u8>, Vec<u8>)>, BackendError> {
        let guard = self.data.lock().expect("mutex poisoned");
        Ok(guard
            .get(&index)
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default())
    }
}

/// Write handle for the in-memory backend.
pub struct InMemoryWriter {
    data: Arc<Mutex<HashMap<IndexId, HashMap<Vec<u8>, Vec<u8>>>>>,
}

impl BackendWriter for InMemoryWriter {
    fn commit(&mut self, ops: Vec<WriteOp>) -> Result<(), BackendError> {
        let mut guard = self.data.lock().expect("mutex poisoned");
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

/// Backend wrapper that adds configurable latency to commits.
///
/// Simulates durable write cost. Reads are unaffected.
#[derive(Clone)]
pub struct SlowBackend<B> {
    inner: B,
    commit_delay: std::time::Duration,
}

impl<B> SlowBackend<B> {
    /// Wrap `inner` with a fixed delay per `commit` call.
    pub fn new(inner: B, commit_delay: std::time::Duration) -> Self {
        Self {
            inner,
            commit_delay,
        }
    }
}

impl<B: Backend> Backend for SlowBackend<B> {
    type Reader = B::Reader;
    type Writer = SlowWriter<B::Writer>;

    fn reader(&self) -> Result<Self::Reader, BackendError> {
        self.inner.reader()
    }

    fn writer(&self) -> Result<Self::Writer, BackendError> {
        let inner = self.inner.writer()?;
        Ok(SlowWriter {
            inner,
            delay: self.commit_delay,
        })
    }

    fn flush(&self) -> Result<(), BackendError> {
        self.inner.flush()
    }
}

/// Writer that sleeps before delegating to the inner writer.
pub struct SlowWriter<W> {
    inner: W,
    delay: std::time::Duration,
}

impl<W: BackendWriter> BackendWriter for SlowWriter<W> {
    fn commit(&mut self, ops: Vec<WriteOp>) -> Result<(), BackendError> {
        std::thread::sleep(self.delay);
        self.inner.commit(ops)
    }
}
