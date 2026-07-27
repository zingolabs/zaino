//! In-memory backend: correct, no IO, data lost on drop.
//!
//! Useful for tests, benchmarks, and ephemeral demo runs.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::backend::{Backend, BackendReader, BackendWriter, Namespace, RawKey, RawValue, WriteOp};
use crate::error::{CommitError, FlushError, OpenError, ReadError};

/// In-memory backend. Stores key-value pairs per namespace.
///
/// Thread-safe via `Arc<Mutex<...>>` — readers and writers share the
/// same underlying map.
#[derive(Clone)]
pub struct InMemoryBackend {
    data: Arc<Mutex<HashMap<Namespace, HashMap<RawKey, RawValue>>>>,
}

impl InMemoryBackend {
    /// Create an empty backend.
    pub fn new() -> Self {
        Self {
            data: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Read all entries for a given namespace. For assertions.
    pub fn entries(&self, namespace: Namespace) -> HashMap<RawKey, RawValue> {
        let guard = self.data.lock().expect("mutex poisoned");
        guard.get(&namespace).cloned().unwrap_or_default()
    }

    /// Read a single value. For assertions.
    pub fn get_value(&self, namespace: Namespace, key: &[u8]) -> Option<RawValue> {
        let guard = self.data.lock().expect("mutex poisoned");
        guard.get(&namespace).and_then(|m| m.get(key).cloned())
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

    fn reader(&self) -> Result<Self::Reader, OpenError> {
        Ok(InMemoryReader {
            data: Arc::clone(&self.data),
        })
    }

    fn writer(&self) -> Result<Self::Writer, OpenError> {
        Ok(InMemoryWriter {
            data: Arc::clone(&self.data),
        })
    }

    fn flush(&self) -> Result<(), FlushError> {
        Ok(())
    }
}

/// Read handle for the in-memory backend.
pub struct InMemoryReader {
    data: Arc<Mutex<HashMap<Namespace, HashMap<RawKey, RawValue>>>>,
}

impl BackendReader for InMemoryReader {
    fn get(&self, namespace: Namespace, key: &[u8]) -> Result<Option<RawValue>, ReadError> {
        let guard = self.data.lock().expect("mutex poisoned");
        Ok(guard.get(&namespace).and_then(|m| m.get(key).cloned()))
    }

    fn scan(&self, namespace: Namespace) -> Result<Vec<(RawKey, RawValue)>, ReadError> {
        let guard = self.data.lock().expect("mutex poisoned");
        Ok(guard
            .get(&namespace)
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default())
    }
}

/// Write handle for the in-memory backend.
pub struct InMemoryWriter {
    data: Arc<Mutex<HashMap<Namespace, HashMap<RawKey, RawValue>>>>,
}

impl BackendWriter for InMemoryWriter {
    fn commit(&mut self, ops: Vec<WriteOp>) -> Result<(), CommitError> {
        let mut guard = self.data.lock().expect("mutex poisoned");
        for op in ops {
            match op {
                WriteOp::Put {
                    namespace,
                    key,
                    value,
                } => {
                    guard.entry(namespace).or_default().insert(key, value);
                }
                WriteOp::Delete { namespace, key } => {
                    if let Some(map) = guard.get_mut(&namespace) {
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

    fn reader(&self) -> Result<Self::Reader, OpenError> {
        self.inner.reader()
    }

    fn writer(&self) -> Result<Self::Writer, OpenError> {
        let inner = self.inner.writer()?;
        Ok(SlowWriter {
            inner,
            delay: self.commit_delay,
        })
    }

    fn flush(&self) -> Result<(), FlushError> {
        self.inner.flush()
    }
}

/// Writer that sleeps before delegating to the inner writer.
pub struct SlowWriter<W> {
    inner: W,
    delay: std::time::Duration,
}

impl<W: BackendWriter> BackendWriter for SlowWriter<W> {
    fn commit(&mut self, ops: Vec<WriteOp>) -> Result<(), CommitError> {
        std::thread::sleep(self.delay);
        self.inner.commit(ops)
    }
}
