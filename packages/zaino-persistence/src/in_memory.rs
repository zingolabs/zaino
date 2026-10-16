//! In-memory backend: correct, no IO, data lost on drop.
//!
//! Useful for tests, benchmarks, and ephemeral demo runs.

use std::collections::{BTreeMap, HashMap};
use std::marker::PhantomData;
use std::ops::Bound;
use std::sync::{Arc, Mutex};

use crate::backend::{
    Backend, BackendReader, BackendWriter, Namespace, RawKey, RawValue, ScanDirection, ScanRange,
    WriteOp,
};
use crate::error::{CommitError, FlushError, OpenError, ReadError};

type Entries = BTreeMap<RawKey, RawValue>;
type Namespaces = HashMap<Namespace, Entries>;
type Store = Arc<Mutex<Namespaces>>;

/// In-memory backend. Stores key-value pairs per declared namespace.
///
/// Thread-safe via `Arc<Mutex<...>>` — readers and writers share the
/// same underlying map.
#[derive(Clone)]
pub struct InMemoryBackend {
    data: Store,
}

impl InMemoryBackend {
    /// Create an empty backend with `namespaces` declared.
    pub fn new(namespaces: &[Namespace]) -> Self {
        let declared = namespaces.iter().map(|&ns| (ns, Entries::new())).collect();
        Self {
            data: Arc::new(Mutex::new(declared)),
        }
    }

    /// Read all entries for a given namespace. For assertions.
    pub fn entries(&self, namespace: Namespace) -> HashMap<RawKey, RawValue> {
        let guard = self.data.lock().expect("mutex poisoned");
        guard
            .get(&namespace)
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default()
    }

    /// Read a single value. For assertions.
    pub fn get_value(&self, namespace: Namespace, key: &[u8]) -> Option<RawValue> {
        let guard = self.data.lock().expect("mutex poisoned");
        guard.get(&namespace).and_then(|m| m.get(key).cloned())
    }
}

/// The entries of `namespace`, or `NamespaceNotFound` if it was not declared.
fn declared(store: &Namespaces, namespace: Namespace) -> Result<&Entries, ReadError> {
    store
        .get(&namespace)
        .ok_or_else(|| ReadError::NamespaceNotFound(namespace.to_string()))
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
    data: Store,
}

impl BackendReader for InMemoryReader {
    fn get(&self, namespace: Namespace, key: &[u8]) -> Result<Option<RawValue>, ReadError> {
        let guard = self.data.lock().expect("mutex poisoned");
        Ok(declared(&guard, namespace)?.get(key).cloned())
    }

    fn scan(&self, namespace: Namespace) -> Result<Vec<(RawKey, RawValue)>, ReadError> {
        let guard = self.data.lock().expect("mutex poisoned");
        Ok(declared(&guard, namespace)?
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect())
    }

    fn scan_range<T, F>(
        &self,
        namespace: Namespace,
        range: ScanRange,
        filter: F,
    ) -> impl Iterator<Item = Result<Vec<T>, ReadError>> + Send + 'static + use<T, F>
    where
        T: Send + 'static,
        F: FnMut(&[u8], &[u8]) -> Option<T> + Send + 'static,
    {
        InMemoryChunks {
            data: Arc::clone(&self.data),
            namespace,
            range,
            filter,
            done: false,
            _item: PhantomData,
        }
    }
}

/// Lazy chunk iterator for [`InMemoryReader::scan_range`].
///
/// Locks the store once per chunk and resumes past the last visited key.
struct InMemoryChunks<T, F> {
    data: Store,
    namespace: Namespace,
    range: ScanRange,
    filter: F,
    done: bool,
    _item: PhantomData<fn() -> T>,
}

impl<T, F> Iterator for InMemoryChunks<T, F>
where
    F: FnMut(&[u8], &[u8]) -> Option<T>,
{
    type Item = Result<Vec<T>, ReadError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done || is_empty_range(&self.range.lower, &self.range.upper) {
            self.done = true;
            return None;
        }
        let guard = self.data.lock().expect("mutex poisoned");
        let map = match declared(&guard, self.namespace) {
            Ok(map) => map,
            Err(err) => {
                self.done = true;
                return Some(Err(err));
            }
        };
        let bounds = (
            self.range.lower.as_ref().map(Vec::as_slice),
            self.range.upper.as_ref().map(Vec::as_slice),
        );
        let entries = map
            .range::<[u8], _>(bounds)
            .map(|(k, v)| (k.as_slice(), v.as_slice()));
        let budget = self.range.chunk_bytes.get();
        let fill = match self.range.direction {
            ScanDirection::Forward => fill_chunk(entries, budget, &mut self.filter),
            ScanDirection::Reverse => fill_chunk(entries.rev(), budget, &mut self.filter),
        };
        let resume = fill.last_visited.map(<[u8]>::to_vec);

        match (fill.exhausted, resume) {
            (false, Some(key)) => match self.range.direction {
                ScanDirection::Forward => self.range.lower = Bound::Excluded(key),
                ScanDirection::Reverse => self.range.upper = Bound::Excluded(key),
            },
            _ => self.done = true,
        }
        (!fill.chunk.is_empty()).then_some(Ok(fill.chunk))
    }
}

/// One chunk's worth of a range walk.
struct Fill<'a, T> {
    chunk: Vec<T>,
    last_visited: Option<&'a [u8]>,
    exhausted: bool,
}

/// Map `entries` through `filter` until the next entry would take the chunk
/// past `budget` raw bytes, or the entries run out.
fn fill_chunk<'a, T>(
    entries: impl Iterator<Item = (&'a [u8], &'a [u8])>,
    budget: usize,
    filter: &mut impl FnMut(&[u8], &[u8]) -> Option<T>,
) -> Fill<'a, T> {
    let mut fill = Fill {
        chunk: Vec::new(),
        last_visited: None,
        exhausted: true,
    };
    let mut bytes = 0usize;
    for (key, value) in entries {
        let size = key.len().saturating_add(value.len());
        if !fill.chunk.is_empty() && bytes.saturating_add(size) > budget {
            fill.exhausted = false;
            break;
        }
        fill.last_visited = Some(key);
        if let Some(item) = filter(key, value) {
            fill.chunk.push(item);
            bytes = bytes.saturating_add(size);
        }
    }
    fill
}

/// Whether no key can satisfy both bounds.
fn is_empty_range(lower: &Bound<RawKey>, upper: &Bound<RawKey>) -> bool {
    match (lower, upper) {
        (Bound::Unbounded, _) | (_, Bound::Unbounded) => false,
        (Bound::Included(lo), Bound::Included(hi)) => lo > hi,
        (Bound::Included(lo) | Bound::Excluded(lo), Bound::Included(hi) | Bound::Excluded(hi)) => {
            lo >= hi
        }
    }
}

/// Write handle for the in-memory backend.
pub struct InMemoryWriter {
    data: Store,
}

impl BackendWriter for InMemoryWriter {
    fn commit(&mut self, ops: Vec<WriteOp>) -> Result<(), CommitError> {
        let mut guard = self.data.lock().expect("mutex poisoned");
        // Check the whole batch first so a rejected batch applies nothing.
        if let Some(undeclared) = ops
            .iter()
            .map(|op| match op {
                WriteOp::Put { namespace, .. } | WriteOp::Delete { namespace, .. } => *namespace,
            })
            .find(|ns| !guard.contains_key(ns))
        {
            return Err(CommitError::NamespaceNotFound(undeclared.to_string()));
        }
        for op in ops {
            match op {
                WriteOp::Put {
                    namespace,
                    key,
                    value,
                } => {
                    guard
                        .get_mut(&namespace)
                        .expect("batch namespaces checked above")
                        .insert(key, value);
                }
                WriteOp::Delete { namespace, key } => {
                    guard
                        .get_mut(&namespace)
                        .expect("batch namespaces checked above")
                        .remove(&key);
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

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;
    use std::ops::Bound::{self, Excluded, Included, Unbounded};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use super::InMemoryBackend;
    use crate::backend::{
        raw_entry, Backend, BackendReader, BackendWriter, Namespace, RawKey, RawValue,
        ScanDirection::{self, Forward, Reverse},
        ScanRange, WriteOp,
    };
    use crate::error::{CommitError, ReadError};

    const NS: Namespace = Namespace::new("ns");
    const OTHER: Namespace = Namespace::new("other");

    fn put(key: u8, value_len: usize) -> WriteOp {
        WriteOp::Put {
            namespace: NS,
            key: vec![key],
            value: vec![key; value_len],
        }
    }

    /// One-byte keys, written in the given order, each with a value of the given length.
    fn backend_with(entries: &[(u8, usize)]) -> InMemoryBackend {
        let backend = InMemoryBackend::new(&[NS]);
        backend
            .writer()
            .expect("in-memory writer is infallible")
            .commit(entries.iter().map(|&(k, len)| put(k, len)).collect())
            .expect("in-memory commit is infallible");
        backend
    }

    fn range(
        lower: Bound<u8>,
        upper: Bound<u8>,
        direction: ScanDirection,
        chunk_bytes: usize,
    ) -> ScanRange {
        ScanRange {
            lower: lower.map(|k| vec![k]),
            upper: upper.map(|k| vec![k]),
            direction,
            chunk_bytes: NonZeroUsize::new(chunk_bytes).expect("test budgets are non-zero"),
        }
    }

    fn raw_chunks(backend: &InMemoryBackend, range: ScanRange) -> Vec<Vec<(RawKey, RawValue)>> {
        backend
            .reader()
            .expect("in-memory reader is infallible")
            .scan_range(NS, range, raw_entry)
            .collect::<Result<_, _>>()
            .expect("in-memory scan is infallible")
    }

    fn chunk_keys(chunks: &[Vec<(RawKey, RawValue)>]) -> Vec<Vec<u8>> {
        chunks
            .iter()
            .map(|chunk| chunk.iter().map(|(k, _)| k[0]).collect())
            .collect()
    }

    fn keys(chunks: &[Vec<(RawKey, RawValue)>]) -> Vec<u8> {
        chunk_keys(chunks).concat()
    }

    #[test]
    fn visits_keys_in_direction_order() {
        let backend = backend_with(&[(3, 1), (1, 1), (4, 1), (0, 1), (2, 1)]);
        let all = |dir| {
            keys(&raw_chunks(
                &backend,
                range(Unbounded, Unbounded, dir, 1024),
            ))
        };
        assert_eq!(all(Forward), [0, 1, 2, 3, 4]);
        assert_eq!(all(Reverse), [4, 3, 2, 1, 0]);
    }

    #[test]
    fn bounds_select_keys() {
        let backend = backend_with(&(0..=5).map(|k| (k, 1)).collect::<Vec<_>>());
        let cases: [(Bound<u8>, Bound<u8>, &[u8]); 5] = [
            (Included(1), Included(4), &[1, 2, 3, 4]),
            (Excluded(1), Excluded(4), &[2, 3]),
            (Unbounded, Excluded(2), &[0, 1]),
            (Included(4), Unbounded, &[4, 5]),
            (Excluded(2), Included(3), &[3]),
        ];
        for (lower, upper, expected) in cases {
            let forward = keys(&raw_chunks(&backend, range(lower, upper, Forward, 1024)));
            let mut reverse = keys(&raw_chunks(&backend, range(lower, upper, Reverse, 1024)));
            reverse.reverse();
            assert_eq!(forward, expected, "forward {lower:?}..{upper:?}");
            assert_eq!(reverse, expected, "reverse {lower:?}..{upper:?}");
        }
    }

    #[test]
    fn chunks_stay_within_byte_budget() {
        // Every entry is 4 raw bytes, so a 10-byte budget fits two.
        let backend = backend_with(&(0..10).map(|k| (k, 3)).collect::<Vec<_>>());
        let chunks = raw_chunks(&backend, range(Unbounded, Unbounded, Forward, 10));
        assert_eq!(
            chunk_keys(&chunks),
            [vec![0, 1], vec![2, 3], vec![4, 5], vec![6, 7], vec![8, 9]]
        );
    }

    #[test]
    fn oversized_entry_is_returned_alone() {
        let backend = backend_with(&[(0, 1), (1, 100), (2, 1)]);
        for (dir, expected) in [
            (Forward, [vec![0], vec![1], vec![2]]),
            (Reverse, [vec![2], vec![1], vec![0]]),
        ] {
            let chunks = raw_chunks(&backend, range(Unbounded, Unbounded, dir, 10));
            assert_eq!(chunk_keys(&chunks), expected, "{dir:?}");
        }
    }

    #[test]
    fn joined_chunks_equal_full_scan() {
        let entries: Vec<_> = (0..50).map(|k| (k, usize::from(k % 7) + 1)).collect();
        let backend = backend_with(&entries);
        let full: Vec<(RawKey, RawValue)> = entries
            .iter()
            .map(|&(k, len)| (vec![k], vec![k; len]))
            .collect();
        let mut full_reverse = full.clone();
        full_reverse.reverse();
        for budget in 1..=40 {
            for (dir, expected) in [(Forward, &full), (Reverse, &full_reverse)] {
                let chunks = raw_chunks(&backend, range(Unbounded, Unbounded, dir, budget));
                assert!(
                    chunks.iter().all(|c| !c.is_empty()),
                    "empty chunk at {budget} {dir:?}"
                );
                assert_eq!(&chunks.concat(), expected, "budget {budget} {dir:?}");
            }
        }
    }

    #[test]
    fn filter_maps_and_skips_entries() {
        // Every entry is 4 raw bytes; only kept entries count toward the budget.
        let backend = backend_with(&(0..10).map(|k| (k, 3)).collect::<Vec<_>>());
        let chunks: Vec<Vec<(u8, usize)>> = backend
            .reader()
            .expect("in-memory reader is infallible")
            .scan_range(
                NS,
                range(Unbounded, Unbounded, Forward, 8),
                |key: &[u8], value: &[u8]| key[0].is_multiple_of(2).then(|| (key[0], value.len())),
            )
            .collect::<Result<_, _>>()
            .expect("in-memory scan is infallible");
        assert_eq!(
            chunks,
            [vec![(0, 3), (2, 3)], vec![(4, 3), (6, 3)], vec![(8, 3)]]
        );
    }

    #[test]
    fn filter_runs_once_per_entry() {
        let backend = backend_with(&(0..20).map(|k| (k, 1)).collect::<Vec<_>>());
        for dir in [Forward, Reverse] {
            let calls = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&calls);
            let kept: usize = backend
                .reader()
                .expect("in-memory reader is infallible")
                .scan_range(
                    NS,
                    range(Unbounded, Unbounded, dir, 4),
                    move |key: &[u8], _: &[u8]| {
                        counter.fetch_add(1, Ordering::Relaxed);
                        key[0].is_multiple_of(3).then_some(())
                    },
                )
                .map(|chunk| chunk.expect("in-memory scan is infallible").len())
                .sum();
            assert_eq!(kept, 7, "{dir:?}");
            assert_eq!(calls.load(Ordering::Relaxed), 20, "{dir:?}");
        }
    }

    #[test]
    fn empty_and_inverted_ranges_yield_nothing() {
        let backend = backend_with(&(0..5).map(|k| (k, 1)).collect::<Vec<_>>());
        let cases = [
            (Included(3), Included(2)),
            (Excluded(2), Excluded(2)),
            (Excluded(2), Included(2)),
            (Included(2), Excluded(2)),
            (Included(10), Included(20)),
        ];
        for (lower, upper) in cases {
            for dir in [Forward, Reverse] {
                let chunks = raw_chunks(&backend, range(lower, upper, dir, 1024));
                assert!(chunks.is_empty(), "{lower:?}..{upper:?} {dir:?}");
            }
        }
    }

    #[test]
    fn undeclared_namespace_is_an_error_for_every_read() {
        let reader = InMemoryBackend::new(&[NS])
            .reader()
            .expect("in-memory reader is infallible");
        let not_found = |result: Result<_, ReadError>| matches!(result, Err(ReadError::NamespaceNotFound(name)) if name == OTHER.as_str());
        assert!(not_found(reader.get(OTHER, &[0]).map(drop)));
        assert!(not_found(reader.scan(OTHER).map(drop)));
        let mut chunks =
            reader.scan_range(OTHER, range(Unbounded, Unbounded, Forward, 1024), raw_entry);
        assert!(not_found(
            chunks.next().expect("error is yielded").map(drop)
        ));
        assert!(chunks.next().is_none(), "finished after the error");
    }

    #[test]
    fn declared_empty_namespace_reads_as_empty() -> Result<(), ReadError> {
        let reader = InMemoryBackend::new(&[NS])
            .reader()
            .expect("in-memory reader is infallible");
        assert_eq!(reader.get(NS, &[0])?, None);
        assert!(reader.scan(NS)?.is_empty());
        let mut chunks =
            reader.scan_range(NS, range(Unbounded, Unbounded, Forward, 1024), raw_entry);
        assert!(chunks.next().is_none());
        Ok(())
    }

    #[test]
    fn commit_naming_undeclared_namespace_applies_nothing() {
        let backend = InMemoryBackend::new(&[NS]);
        let mut writer = backend.writer().expect("in-memory writer is infallible");
        for undeclared in [
            WriteOp::Put {
                namespace: OTHER,
                key: vec![1],
                value: vec![1],
            },
            WriteOp::Delete {
                namespace: OTHER,
                key: vec![1],
            },
        ] {
            let result = writer.commit(vec![put(0, 1), undeclared]);
            assert!(
                matches!(&result, Err(CommitError::NamespaceNotFound(name)) if name == OTHER.as_str()),
                "{result:?}"
            );
            assert!(
                backend.entries(NS).is_empty(),
                "rejected batch applied nothing"
            );
        }
    }

    #[test]
    fn commits_between_chunks_neither_repeat_nor_skip() {
        let backend = backend_with(&[(0, 1), (2, 1), (4, 1), (6, 1)]);
        let reader = backend.reader().expect("in-memory reader is infallible");
        let mut chunks = reader.scan_range(NS, range(Unbounded, Unbounded, Forward, 2), raw_entry);
        let first = chunks
            .next()
            .expect("range is non-empty")
            .expect("in-memory scan is infallible");
        assert_eq!(keys(&[first]), [0]);

        backend
            .writer()
            .expect("in-memory writer is infallible")
            .commit(vec![
                put(0, 2),
                put(1, 1),
                put(5, 1),
                WriteOp::Delete {
                    namespace: NS,
                    key: vec![4],
                },
            ])
            .expect("in-memory commit is infallible");

        let rest: Vec<_> = chunks
            .collect::<Result<_, _>>()
            .expect("in-memory scan is infallible");
        assert_eq!(keys(&rest), [1, 2, 5, 6]);
    }
}
