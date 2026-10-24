//! In-memory backend: correct, no IO, data lost on drop.
//!
//! Useful for tests, benchmarks, and ephemeral demo runs.

use std::collections::HashMap;
use std::marker::PhantomData;
use std::ops::Bound;
use std::sync::{Arc, Mutex, MutexGuard};

use imbl::{OrdMap, OrdSet};
use zaino_primitives::types::BlockRef;

use crate::backend::{
    Backend, BackendReader, BackendWriter, MaintenanceReader, Namespace, NamespaceSpec, RawKey,
    RawValue, ScanDirection, ScanRange, WriteOp,
};
use crate::error::{CommitError, FlushError, FootprintError, OpenError, ReadError, VerifyError};
use crate::rules::{check_commit, check_key, check_specs, is_empty_range, CommitState, KeyState};
use crate::verify::{NamespaceFootprint, Verification};

/// The current state; a commit swaps in a new one, readers keep the one they took.
type Store = Arc<Mutex<Arc<State>>>;

/// Everything committed: each namespace's entries and removed keys, and the tip.
///
/// Persistent maps: a clone is O(namespaces), sharing every entry.
#[derive(Clone)]
struct State {
    specs: Arc<[NamespaceSpec]>,
    tip: Option<BlockRef>,
    namespaces: HashMap<Namespace, Held>,
}

#[derive(Clone, Default)]
struct Held {
    entries: OrdMap<RawKey, RawValue>,
    removed: OrdSet<RawKey>,
}

/// In-memory backend. Stores key-value pairs per declared namespace.
///
/// A commit is visible and kept at once; each reader handle is a snapshot of
/// the state when it was opened.
#[derive(Clone)]
pub struct InMemoryBackend {
    data: Store,
}

impl InMemoryBackend {
    /// Create an empty backend with `namespaces` declared.
    pub fn new(namespaces: &[NamespaceSpec]) -> Result<Self, OpenError> {
        check_specs(namespaces)?;
        let held = namespaces
            .iter()
            .map(|spec| (spec.namespace, Held::default()))
            .collect();
        let state = State {
            specs: namespaces.into(),
            tip: None,
            namespaces: held,
        };
        Ok(Self {
            data: Arc::new(Mutex::new(Arc::new(state))),
        })
    }
}

/// The store, locked; a poisoned lock = a writer panicked mid-commit.
fn lock(store: &Store) -> Result<MutexGuard<'_, Arc<State>>, ReadError> {
    store.lock().map_err(|_| ReadError::ReadFailed {
        operation: "lock",
        source: "in-memory store lock poisoned".into(),
    })
}

/// The current state, shared.
fn current(store: &Store) -> Result<Arc<State>, ReadError> {
    Ok(Arc::clone(&*lock(store)?))
}

/// The held data of `namespace`, or `NamespaceNotFound` if it was not declared.
fn declared(state: &State, namespace: Namespace) -> Result<&Held, ReadError> {
    state
        .namespaces
        .get(&namespace)
        .ok_or_else(|| ReadError::NamespaceNotFound(namespace.to_string()))
}

impl CommitState for State {
    fn tip(&self) -> Option<BlockRef> {
        self.tip
    }

    fn sequence_len(&self, namespace: Namespace) -> Result<u64, ReadError> {
        Ok(declared(self, namespace)?.entries.len() as u64)
    }

    fn key_state(&self, namespace: Namespace, key: &[u8]) -> Result<KeyState, ReadError> {
        let held = declared(self, namespace)?;
        Ok(if held.entries.contains_key(key) {
            KeyState::Present
        } else if held.removed.contains(key) {
            KeyState::Removed
        } else {
            KeyState::Absent
        })
    }
}

impl Backend for InMemoryBackend {
    type Reader = InMemoryReader;
    type Writer = InMemoryWriter;

    fn reader(&self) -> Result<Self::Reader, OpenError> {
        let state = current(&self.data).map_err(|source| OpenError::Unavailable {
            operation: "snapshot",
            source: source.into(),
        })?;
        Ok(InMemoryReader { state })
    }

    fn writer(&self) -> Result<Self::Writer, OpenError> {
        Ok(InMemoryWriter {
            data: Arc::clone(&self.data),
        })
    }

    fn flush(&self) -> Result<(), FlushError> {
        Ok(())
    }

    fn verify(&self) -> Result<Verification, VerifyError> {
        Ok(Verification::default())
    }

    fn footprint(&self) -> Result<Vec<NamespaceFootprint>, FootprintError> {
        let state = current(&self.data).map_err(|source| FootprintError::Failed {
            operation: "snapshot",
            source: source.into(),
        })?;
        Ok(state
            .specs
            .iter()
            .map(|spec| {
                let bytes = state.namespaces.get(&spec.namespace).map_or(0, |held| {
                    held.entries
                        .iter()
                        .map(|(k, v)| (k.len() + v.len()) as u64)
                        .sum()
                });
                NamespaceFootprint {
                    namespace: spec.namespace,
                    bytes,
                    cached: None,
                }
            })
            .collect())
    }
}

/// Read handle for the in-memory backend: the state when it was opened.
pub struct InMemoryReader {
    state: Arc<State>,
}

impl BackendReader for InMemoryReader {
    fn tip(&self) -> Result<Option<BlockRef>, ReadError> {
        Ok(self.state.tip)
    }

    fn get(&self, namespace: Namespace, key: &[u8]) -> Result<Option<RawValue>, ReadError> {
        check_key(&self.state.specs, namespace, key)?;
        Ok(declared(&self.state, namespace)?.entries.get(key).cloned())
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
            state: Arc::clone(&self.state),
            namespace,
            range,
            filter,
            done: false,
            _item: PhantomData,
        }
    }
}

impl MaintenanceReader for InMemoryReader {
    fn scan(&self, namespace: Namespace) -> Result<Vec<(RawKey, RawValue)>, ReadError> {
        Ok(declared(&self.state, namespace)?
            .entries
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect())
    }
}

/// Lazy chunk iterator for [`InMemoryReader::scan_range`].
///
/// Walks the reader's snapshot, resuming past the last visited key.
struct InMemoryChunks<T, F> {
    state: Arc<State>,
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
        if self.done {
            return None;
        }
        let map = match declared(&self.state, self.namespace) {
            Ok(held) => &held.entries,
            Err(err) => {
                self.done = true;
                return Some(Err(err));
            }
        };
        if is_empty_range(bytes(&self.range.lower), bytes(&self.range.upper)) {
            self.done = true;
            return None;
        }
        let bounds = (
            self.range.lower.as_ref().map(Vec::as_slice),
            self.range.upper.as_ref().map(Vec::as_slice),
        );
        let entries = map
            .range::<_, [u8]>(bounds)
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
        fill.last_visited.is_some().then_some(Ok(fill.chunk))
    }
}

/// One chunk's worth of a range walk.
struct Fill<'a, T> {
    chunk: Vec<T>,
    last_visited: Option<&'a [u8]>,
    exhausted: bool,
}

/// `bound` over borrowed key bytes
fn bytes(bound: &Bound<RawKey>) -> Bound<&[u8]> {
    bound.as_ref().map(Vec::as_slice)
}

/// Map `entries` through `filter` until the next entry would take the bytes
/// visited past `budget`, or the entries run out (at least one entry visited)
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
    let mut visited = 0usize;
    for (key, value) in entries {
        let size = key.len().saturating_add(value.len());
        if fill.last_visited.is_some() && visited.saturating_add(size) > budget {
            fill.exhausted = false;
            break;
        }
        fill.last_visited = Some(key);
        visited = visited.saturating_add(size);
        if let Some(item) = filter(key, value) {
            fill.chunk.push(item);
        }
    }
    fill
}

/// Write handle for the in-memory backend.
pub struct InMemoryWriter {
    data: Store,
}

impl BackendWriter for InMemoryWriter {
    fn commit(&mut self, tip: BlockRef, ops: Vec<WriteOp>) -> Result<(), CommitError> {
        // poisoned = a writer panicked mid-commit
        let mut guard = self.data.lock().map_err(|_| CommitError::Poisoned)?;
        // Check the whole batch first so a rejected batch applies nothing.
        check_commit(&guard.specs, &**guard, tip, &ops)?;
        // Built aside and swapped in whole: readers keep theirs, a failure applies nothing.
        let mut state = State::clone(&guard);
        for op in ops {
            let (WriteOp::Put { namespace, .. } | WriteOp::Delete { namespace, .. }) = &op;
            let held = state
                .namespaces
                .get_mut(namespace)
                .ok_or_else(|| CommitError::NamespaceNotFound(namespace.to_string()))?;
            match op {
                WriteOp::Put { key, value, .. } => {
                    held.entries.insert(key, value);
                }
                WriteOp::Delete { key, .. } => {
                    held.entries.remove(&key);
                    held.removed.insert(key);
                }
            }
        }
        state.tip = Some(tip);
        *guard = Arc::new(state);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
