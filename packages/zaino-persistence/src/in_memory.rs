//! In-memory backend: correct, no IO, data lost on drop.
//!
//! Useful for tests, benchmarks, and ephemeral demo runs.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::backend::{
    Backend, BackendReader, BackendWriter, BulkPolicy, Namespace, RangeVisitor, RawKey, RawValue,
    WriteOp,
};
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
        // The map is unordered; sort by key to return the ascending bytewise
        // order the contract guarantees (and that the LMDB cursor returns).
        let mut entries: Vec<(RawKey, RawValue)> = guard
            .get(&namespace)
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(entries)
    }

    fn scan_range(
        &self,
        namespace: Namespace,
        start: &[u8],
        end_exclusive: &[u8],
        visit: &mut RangeVisitor<'_>,
    ) -> Result<(), ReadError> {
        let guard = self.data.lock().expect("mutex poisoned");
        let Some(map) = guard.get(&namespace) else {
            return Ok(());
        };
        // The map is unordered, so select the in-range keys and sort them to
        // present the ascending key order the contract (and the LMDB backend)
        // guarantee. Bounds mirror LMDB's cursor: `start` inclusive,
        // `end_exclusive` exclusive.
        let mut selected: Vec<(&RawKey, &RawValue)> = map
            .iter()
            .filter(|(key, _)| key.as_slice() >= start && key.as_slice() < end_exclusive)
            .collect();
        selected.sort_by(|a, b| a.0.cmp(b.0));
        for (key, value) in selected {
            if visit(key, value).is_break() {
                break;
            }
        }
        Ok(())
    }

    fn first_key(&self, namespace: Namespace) -> Result<Option<RawKey>, ReadError> {
        let guard = self.data.lock().expect("mutex poisoned");
        Ok(guard
            .get(&namespace)
            .and_then(|map| map.keys().min().cloned()))
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

    fn begin_bulk(&self, policy: BulkPolicy) -> Result<(), CommitError> {
        self.inner.begin_bulk(policy)
    }

    fn finish_bulk(&self) -> Result<(), CommitError> {
        self.inner.finish_bulk()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Backend;
    use core::ops::ControlFlow;

    fn seeded() -> (InMemoryBackend, Namespace) {
        let ns = Namespace::new("range_ns");
        let backend = InMemoryBackend::new();
        let mut writer = backend.writer().expect("writer");
        let put = |key: &[u8]| WriteOp::Put {
            namespace: ns,
            key: key.to_vec(),
            value: key.to_vec(),
        };
        writer
            .commit(vec![
                put(b"a1"),
                put(b"a2"),
                put(b"a3"),
                put(b"b1"),
                put(b"b2"),
            ])
            .expect("commit");
        (backend, ns)
    }

    fn collect_range(
        reader: &InMemoryReader,
        ns: Namespace,
        start: &[u8],
        end_exclusive: &[u8],
    ) -> Vec<RawKey> {
        let mut out = Vec::new();
        reader
            .scan_range(ns, start, end_exclusive, &mut |k, _| {
                out.push(k.to_vec());
                ControlFlow::Continue(())
            })
            .expect("scan_range");
        out
    }

    #[test]
    fn scan_range_is_ascending_end_exclusive_and_isolated() {
        let (backend, ns) = seeded();
        let reader = backend.reader().expect("reader");
        // Ascending order despite the unordered map; end exclusive; no b-keys.
        assert_eq!(
            collect_range(&reader, ns, b"a1", b"a3"),
            vec![b"a1".to_vec(), b"a2".to_vec()]
        );
        assert_eq!(
            collect_range(&reader, ns, b"a", b"b"),
            vec![b"a1".to_vec(), b"a2".to_vec(), b"a3".to_vec()],
            "only the a-prefixed keys, never b1/b2"
        );
    }

    #[test]
    fn scan_range_empty_and_unmatched_prefixes_visit_nothing() {
        let (backend, ns) = seeded();
        let reader = backend.reader().expect("reader");
        assert!(collect_range(&reader, ns, b"a2", b"a2").is_empty());
        assert!(collect_range(&reader, ns, b"a9", b"b0").is_empty());
        assert!(collect_range(&reader, ns, b"zzz", b"zzz\xff").is_empty());
        // An absent namespace is not an error.
        assert!(collect_range(&reader, Namespace::new("nope"), b"", b"\xff").is_empty());
    }

    #[test]
    fn scan_range_stops_on_break() {
        let (backend, ns) = seeded();
        let reader = backend.reader().expect("reader");
        let mut seen = Vec::new();
        reader
            .scan_range(ns, b"a", b"b", &mut |k, _| {
                seen.push(k.to_vec());
                ControlFlow::Break(())
            })
            .expect("scan_range");
        assert_eq!(seen, vec![b"a1".to_vec()]);
    }

    #[test]
    fn first_key_is_the_smallest_or_none() {
        let (backend, ns) = seeded();
        let reader = backend.reader().expect("reader");
        assert_eq!(
            reader.first_key(ns).expect("first_key"),
            Some(b"a1".to_vec())
        );
        assert_eq!(
            reader
                .first_key(Namespace::new("empty"))
                .expect("first_key"),
            None
        );
    }
}

/// The generic backend conformance suite, run against the in-memory backend.
///
/// In-memory ignores bulk mode (default no-op methods) and is not persistent, so
/// `reopen` returns `None` and the persistence and restart properties skip
/// themselves. One `#[test]` per property gives a precise failure site.
#[cfg(test)]
mod conformance_tests {
    use super::InMemoryBackend;
    use crate::backend::NamespaceSpec;
    use crate::conformance::{self, BackendFactory};

    /// A fresh in-memory backend per `fresh`; never persistent. The namespace
    /// list is ignored: the in-memory backend creates maps lazily on first put.
    struct InMemoryFactory;

    impl BackendFactory for InMemoryFactory {
        type B = InMemoryBackend;

        fn fresh(&self, _namespaces: &[NamespaceSpec]) -> Self::B {
            InMemoryBackend::new()
        }

        fn reopen(&self, _namespaces: &[NamespaceSpec]) -> Option<Self::B> {
            None
        }
    }

    #[test]
    fn get_put_delete_round_trip() {
        conformance::get_put_delete_round_trip(&InMemoryFactory);
    }

    #[test]
    fn commit_is_atomic_across_namespaces() {
        conformance::commit_is_atomic_across_namespaces(&InMemoryFactory);
    }

    #[test]
    fn scan_returns_bytewise_key_order() {
        conformance::scan_returns_bytewise_key_order(&InMemoryFactory);
    }

    #[test]
    fn scan_range_is_ascending_and_half_open() {
        conformance::scan_range_is_ascending_and_half_open(&InMemoryFactory);
    }

    #[test]
    fn first_key_is_smallest_or_none() {
        conformance::first_key_is_smallest_or_none(&InMemoryFactory);
    }

    #[test]
    fn namespaces_are_isolated() {
        conformance::namespaces_are_isolated(&InMemoryFactory);
    }

    #[test]
    fn reopen_persists_committed_data() {
        conformance::reopen_persists_committed_data(&InMemoryFactory);
    }

    #[test]
    fn bulk_disabled_matches_direct() {
        conformance::bulk_disabled_matches_direct(&InMemoryFactory);
    }

    #[test]
    fn bulk_enabled_after_finish_matches_direct() {
        conformance::bulk_enabled_after_finish_matches_direct(&InMemoryFactory);
    }

    #[test]
    fn is_complete_true_outside_bulk_mode() {
        conformance::is_complete_true_outside_bulk_mode(&InMemoryFactory);
    }

    #[test]
    fn is_complete_inside_bulk_mode() {
        conformance::is_complete_inside_bulk_mode(&InMemoryFactory);
    }

    #[test]
    fn finish_bulk_is_idempotent() {
        conformance::finish_bulk_is_idempotent(&InMemoryFactory);
    }

    #[test]
    fn restart_in_bulk_mode_matches_direct() {
        conformance::restart_in_bulk_mode_matches_direct(&InMemoryFactory);
    }

    #[test]
    fn run_all_aggregate() {
        conformance::run_all(&InMemoryFactory);
    }
}
