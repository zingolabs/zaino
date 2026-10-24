//! [`LsmTreeBackend`]: the `zaino-persistence` port over the engine
//!
//! - one writer at a time (`Mutex<Writing>`): checks a commit, buffers it, writes the buffer
//!   once it holds `write_buffer` bytes (or on `flush`)
//! - readers: the last published [`ReadState`] (lock-free `ArcSwap` load), never the engine
//! - every entry point guarded: a panic (std, a dependency, a caller's filter) = an error, and a
//!   write's poisons the backend until reopened

use std::{
    collections::HashMap,
    iter::Peekable,
    marker::PhantomData,
    num::NonZeroUsize,
    ops::{Bound, Range},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};

use arc_swap::ArcSwap;
use bytes::Bytes;
use zaino_persistence::{
    check_key, check_keys, check_shape, check_specs, is_empty_range, raw_entry, sequence_position,
    Backend, BackendReader, BackendWriter, CommitError, CommitState, FlushError, FootprintError,
    KeyState, MaintenanceReader, Namespace, NamespaceFootprint, NamespaceKind, NamespaceSpec,
    OpenError, RawKey, RawValue, ReadError, ScanDirection, ScanRange, Verification, VerifyError,
    WriteOp,
};
use zaino_primitives::types::BlockRef;

use crate::{
    buffer::{Buffer, Entry},
    emit,
    engine::{self, position, DiskView, Durable, Engine},
    error::{guarded, StoreError},
    fs::{Fs, RealFs},
    lsm::{Cursor, Lookup, LsmConfig, Tuning},
    manifest::Identity,
};

/// How [`LsmTreeBackend::open`] lays out and runs an index directory
#[derive(Debug, Clone, Copy)]
pub struct LsmTreeConfig<'a> {
    /// Index name, checked against the directory's (at most 255 bytes)
    pub name: &'a str,
    /// Layout version, checked against the directory's
    pub format: u16,
    /// Namespaces held, checked against those the directory was committed with
    pub namespaces: &'a [NamespaceSpec],
    /// Per-namespace [`Tuning`] (unlisted = default; a sequence takes no `scope`)
    pub tuning: &'a [(Namespace, Tuning)],
    /// Map namespaces' merge policy
    pub lsm: LsmConfig,
    /// Buffered heap at which a commit writes the buffer
    pub write_buffer: NonZeroUsize,
    /// Threads of the backend's pool for parallel writes and commit checks (`None` = the
    /// machine's available parallelism)
    pub threads: Option<NonZeroUsize>,
}

/// On-disk backend: positional files for sequences, an LSM tree for maps, one manifest commit
///
/// - commits visible to readers at once, durable after [`flush`](Backend::flush) or once the
///   buffer holds `write_buffer` bytes
/// - a failed write poisons writes ([`CommitError::Poisoned`]) until reopened; readers keep the
///   last published state
#[derive(Clone)]
pub struct LsmTreeBackend {
    shared: Arc<Shared>,
}

struct Shared {
    fs: Arc<dyn Fs>,
    path: PathBuf,
    identity: Identity,
    specs: Arc<[NamespaceSpec]>,
    versions: Arc<Versions>,
    write_buffer: NonZeroUsize,
    writing: Mutex<Writing>,
    published: ArcSwap<ReadState>,
}

/// The writer's side: engine + buffered commits
struct Writing {
    engine: Engine,
    buffer: Buffer,
    poisoned: bool,
}

/// What a reader reads: committed view + buffer above it, fixed once published
///
/// - alive while any reader or open scan holds it: its buffer's nodes a later commit replaced
///   stay allocated, outside `write_buffer` (counted in [`Versions`])
struct ReadState {
    specs: Arc<[NamespaceSpec]>,
    view: DiskView,
    buffer: Buffer,
    versions: Arc<Versions>,
}

/// Published states alive, and their buffers' estimated heap (an upper bound: states share most
/// nodes), as gauges moved by each state's creation and drop (atomic in the recorder: right
/// across reopens and concurrent drops)
#[derive(Debug)]
struct Versions {
    live: metrics::Gauge,
    heap: metrics::Gauge,
}

impl ReadState {
    fn new(
        specs: Arc<[NamespaceSpec]>,
        view: DiskView,
        buffer: Buffer,
        versions: &Arc<Versions>,
    ) -> Self {
        versions.live.increment(1.0);
        versions.heap.increment(buffer.heap() as f64);
        Self {
            specs,
            view,
            buffer,
            versions: Arc::clone(versions),
        }
    }
}

impl Drop for ReadState {
    fn drop(&mut self) {
        let (versions, heap) = (&self.versions, self.buffer.heap());
        emit::in_drop(|| {
            versions.live.decrement(1.0);
            versions.heap.decrement(heap as f64);
        });
    }
}

impl LsmTreeBackend {
    /// `path` (created if absent) as index `config.name`, through `fs`
    pub fn open(
        fs: Arc<dyn Fs>,
        path: &Path,
        config: &LsmTreeConfig<'_>,
    ) -> Result<Self, OpenError> {
        check_specs(config.namespaces)?;
        guarded(|| Self::open_checked(fs, path, config))
            .and_then(|opened| opened)
            .map_err(|error| error.into_open("open"))
    }

    /// [`open`](Self::open) on the real filesystem
    pub fn open_real(path: &Path, config: &LsmTreeConfig<'_>) -> Result<Self, OpenError> {
        Self::open(RealFs::shared(), path, config)
    }

    fn open_checked(
        fs: Arc<dyn Fs>,
        path: &Path,
        config: &LsmTreeConfig<'_>,
    ) -> Result<Self, StoreError> {
        let specs: Arc<[NamespaceSpec]> = config.namespaces.into();
        let tunings = tunings(&specs, config.tuning)?;
        let identity = Identity::new(config.name, config.format)?;
        let engine = Engine::open(
            &fs,
            path,
            identity.clone(),
            Arc::clone(&specs),
            &tunings,
            config.lsm,
            config.threads,
        )?;
        let buffer = Buffer::empty(&specs);
        let (live, heap) = emit::versions(config.name);
        let versions = Arc::new(Versions { live, heap });
        let published = ArcSwap::from_pointee(ReadState::new(
            Arc::clone(&specs),
            engine.view().clone(),
            buffer.clone(),
            &versions,
        ));
        Ok(Self {
            shared: Arc::new(Shared {
                fs,
                path: path.to_path_buf(),
                identity,
                specs,
                versions,
                write_buffer: config.write_buffer,
                writing: Mutex::new(Writing {
                    engine,
                    buffer,
                    poisoned: false,
                }),
                published,
            }),
        })
    }
}

/// Each spec's tuning, declaration order: unlisted = default; undeclared, listed twice or a
/// sequence `scope` refused
fn tunings(
    specs: &[NamespaceSpec],
    tuning: &[(Namespace, Tuning)],
) -> Result<Vec<Tuning>, StoreError> {
    let refused = |namespace: Namespace, reason| StoreError::Declared {
        name: namespace.to_string(),
        reason,
    };
    let mut tunings = vec![Tuning::default(); specs.len()];
    let mut listed = vec![false; specs.len()];
    for &(namespace, tuned) in tuning {
        let index =
            position(specs, namespace).ok_or_else(|| refused(namespace, "tuned, not declared"))?;
        let (Some(slot), Some(seen), Some(spec)) = (
            tunings.get_mut(index),
            listed.get_mut(index),
            specs.get(index),
        ) else {
            return Err(StoreError::Internal("a spec position past its specs"));
        };
        if std::mem::replace(seen, true) {
            return Err(refused(namespace, "tuned twice"));
        }
        if matches!(spec.kind, NamespaceKind::Sequence { .. }) && tuned.scope != 0 {
            return Err(refused(namespace, "a scope on a sequence (maps only)"));
        }
        *slot = tuned;
    }
    Ok(tunings)
}

/// Every file `path`'s committed manifest names, scrubbed: offline, no lock, safe beside a live
/// writer (`name`, `format` and `namespaces` as it was opened with)
pub fn verify_dir(
    fs: &dyn Fs,
    path: &Path,
    name: &str,
    format: u16,
    namespaces: &[NamespaceSpec],
) -> Result<Verification, VerifyError> {
    guarded(|| {
        let identity = Identity::new(name, format)?;
        engine::verify(fs, path, &identity, namespaces)
    })
    .and_then(|verified| verified)
    .map_err(|error| error.into_verify("verify"))
}

impl Shared {
    fn lock(&self) -> Result<MutexGuard<'_, Writing>, StoreError> {
        // only guarded code runs under the lock: poisoned = a panic escaped the guard
        self.writing
            .lock()
            .map_err(|_| StoreError::Internal("writer lock poisoned"))
    }

    fn publish(&self, writing: &Writing) {
        self.published.store(Arc::new(ReadState::new(
            Arc::clone(&self.specs),
            writing.engine.view().clone(),
            writing.buffer.clone(),
            &self.versions,
        )));
    }

    /// Checked, buffered, published; written once the buffer reaches `write_buffer`
    fn commit(
        &self,
        writing: &mut Writing,
        tip: BlockRef,
        ops: &[WriteOp],
    ) -> Result<(), CommitError> {
        let next = {
            let mut state = Pending::new(&self.specs, writing.engine.view(), &writing.buffer);
            check_shape(&self.specs, &state, tip, ops)?;
            writing
                .engine
                .pool()
                .install(|| state.prefetch(ops))
                .map_err(|error| CommitError::from_check(error.into_read("commit check")))?;
            check_keys(&self.specs, &state, ops)?;
            writing
                .buffer
                .with(&self.specs, tip, ops)
                .map_err(|error| error.into_commit("buffer"))?
        };
        match next.heap() >= self.write_buffer.get() {
            true => self
                .write(writing, &next)
                .map_err(|error| error.into_commit("write")),
            false => {
                writing.buffer = next;
                self.publish(writing);
                Ok(())
            }
        }
    }

    /// `buffer` made durable, then published over an empty buffer; a failure poisons, except a
    /// background merge failed for a cause a retry can clear (found before anything was
    /// written: nothing to recover)
    fn write(&self, writing: &mut Writing, buffer: &Buffer) -> Result<(), StoreError> {
        if let Err(error) = writing.engine.write(buffer) {
            writing.poisoned |= !error.merge_failed();
            return Err(error);
        }
        writing.buffer = Buffer::empty(&self.specs);
        self.publish(writing);
        Ok(())
    }
}

impl Backend for LsmTreeBackend {
    type Reader = LsmTreeReader;
    type Writer = LsmTreeWriter;

    fn reader(&self) -> Result<LsmTreeReader, OpenError> {
        Ok(LsmTreeReader {
            state: self.shared.published.load_full(),
        })
    }

    fn writer(&self) -> Result<LsmTreeWriter, OpenError> {
        Ok(LsmTreeWriter {
            shared: Arc::clone(&self.shared),
        })
    }

    fn flush(&self) -> Result<(), FlushError> {
        let shared = &self.shared;
        let flush_error = |error: StoreError| FlushError::Write(error.into_commit("flush"));
        let mut writing = shared.lock().map_err(flush_error)?;
        if writing.poisoned {
            return Err(FlushError::Write(CommitError::Poisoned));
        }
        if writing.buffer.tip().is_none() {
            return Ok(());
        }
        let buffer = writing.buffer.clone();
        match poisoning_panics(&mut writing, |writing| shared.write(writing, &buffer)) {
            Ok(written) => written.map_err(flush_error),
            Err(panicked) => Err(flush_error(panicked)),
        }
    }

    fn verify(&self) -> Result<Verification, VerifyError> {
        let shared = &self.shared;
        guarded(|| {
            engine::verify(
                shared.fs.as_ref(),
                &shared.path,
                &shared.identity,
                &shared.specs,
            )
        })
        .and_then(|verified| verified)
        .map_err(|error| error.into_verify("verify"))
    }

    fn footprint(&self) -> Result<Vec<NamespaceFootprint>, FootprintError> {
        let state = self.shared.published.load_full();
        guarded(|| state.view.footprint(&state.specs))
            .map_err(|error| error.into_footprint("footprint"))
    }
}

/// Write handle: commits through the backend's single writer
///
/// - holds the engine (and the directory's `LOCK`) while it lives: drop it before reopening;
///   readers hold only their snapshot
pub struct LsmTreeWriter {
    shared: Arc<Shared>,
}

impl BackendWriter for LsmTreeWriter {
    fn commit(&mut self, tip: BlockRef, ops: Vec<WriteOp>) -> Result<(), CommitError> {
        let shared = &self.shared;
        let mut writing = shared.lock().map_err(|error| error.into_commit("lock"))?;
        if writing.poisoned {
            return Err(CommitError::Poisoned);
        }
        match poisoning_panics(&mut writing, |writing| shared.commit(writing, tip, &ops)) {
            Ok(committed) => {
                // corrupt stored bytes found by the checks: no write may build on them
                if matches!(committed, Err(CommitError::Corrupted(_))) {
                    writing.poisoned = true;
                }
                committed
            }
            Err(panicked) => Err(panicked.into_commit("commit")),
        }
    }
}

/// `run` on the writer state, a panic inside it caught as an error that poisons writes (the
/// state may be half changed)
fn poisoning_panics<R>(
    writing: &mut Writing,
    run: impl FnOnce(&mut Writing) -> R,
) -> Result<R, StoreError> {
    let ran = guarded(|| run(writing));
    if ran.is_err() {
        writing.poisoned = true;
    }
    ran
}

/// The state a commit is checked against: buffer first, then durable (its map keys looked up in
/// one batch per map: prefetched, parallel past a few dozen)
struct Pending<'a> {
    specs: &'a [NamespaceSpec],
    view: &'a DiskView,
    buffer: &'a Buffer,
    durable: HashMap<usize, HashMap<&'a [u8], Lookup>>,
}

impl<'a> Pending<'a> {
    fn new(specs: &'a [NamespaceSpec], view: &'a DiskView, buffer: &'a Buffer) -> Self {
        Self {
            specs,
            view,
            buffer,
            durable: HashMap::new(),
        }
    }

    /// Durable states of `ops`' unbuffered map keys, one batch per map (after `check_shape`:
    /// every key declared and of its width)
    fn prefetch(&mut self, ops: &'a [WriteOp]) -> Result<(), StoreError> {
        let mut asked: HashMap<usize, Vec<&'a [u8]>> = HashMap::new();
        for op in ops {
            let (namespace, key) = match op {
                WriteOp::Put { namespace, key, .. } | WriteOp::Delete { namespace, key } => {
                    (*namespace, key.as_slice())
                }
            };
            let Some(index) = position(self.specs, namespace) else {
                continue;
            };
            let unbuffered = self
                .buffer
                .entries(index)
                .is_some_and(|entries| !entries.contains_key(key));
            if unbuffered {
                asked.entry(index).or_default().push(key);
            }
        }
        for (index, keys) in asked {
            let Some(Durable::Map(snapshot)) = self.view.held(index) else {
                return Err(StoreError::Internal("a map's keys asked of another kind"));
            };
            let lookups = snapshot.get_many(&keys)?;
            self.durable
                .insert(index, keys.into_iter().zip(lookups).collect());
        }
        Ok(())
    }

    fn index(&self, namespace: Namespace) -> Result<usize, ReadError> {
        position(self.specs, namespace)
            .ok_or_else(|| ReadError::NamespaceNotFound(namespace.to_string()))
    }
}

impl CommitState for Pending<'_> {
    fn tip(&self) -> Option<BlockRef> {
        self.buffer.tip().or(self.view.tip())
    }

    fn sequence_len(&self, namespace: Namespace) -> Result<u64, ReadError> {
        let index = self.index(namespace)?;
        let durable = match self.view.held(index) {
            Some(Durable::Sequence(pages)) => pages.len(),
            _ => return Err(not_a(namespace, "sequence")),
        };
        let buffered = self
            .buffer
            .records(index)
            .map_or(0, |records| records.len());
        Ok(durable.saturating_add(buffered as u64))
    }

    fn key_state(&self, namespace: Namespace, key: &[u8]) -> Result<KeyState, ReadError> {
        let index = self.index(namespace)?;
        match self
            .buffer
            .entries(index)
            .and_then(|entries| entries.get(key))
        {
            Some(Entry::Value(_)) => return Ok(KeyState::Present),
            Some(Entry::Removed | Entry::Cancelled(_)) => return Ok(KeyState::Removed),
            None => {}
        }
        let prefetched = self.durable.get(&index).and_then(|keys| keys.get(key));
        let lookup = match prefetched {
            Some(lookup) => lookup.clone(),
            None => match self.view.held(index) {
                Some(Durable::Map(snapshot)) => snapshot
                    .lookup_with(key, |value, removed| match (value, removed) {
                        (Some(_), _) => Lookup::Value(Bytes::new()),
                        (None, true) => Lookup::Removed,
                        (None, false) => Lookup::Absent,
                    })
                    .map_err(|error| StoreError::from(error).into_read("key state"))?,
                _ => return Err(not_a(namespace, "map")),
            },
        };
        Ok(match lookup {
            Lookup::Value(_) => KeyState::Present,
            Lookup::Removed => KeyState::Removed,
            Lookup::Absent => KeyState::Absent,
        })
    }
}

fn not_a(namespace: Namespace, kind: &'static str) -> ReadError {
    ReadError::ReadFailed {
        operation: kind,
        source: format!("{namespace} is not a {kind}").into(),
    }
}

/// Read handle: one published state, fixed (later commits need a new handle)
pub struct LsmTreeReader {
    state: Arc<ReadState>,
}

impl BackendReader for LsmTreeReader {
    fn tip(&self) -> Result<Option<BlockRef>, ReadError> {
        Ok(self.state.buffer.tip().or(self.state.view.tip()))
    }

    fn get(&self, namespace: Namespace, key: &[u8]) -> Result<Option<RawValue>, ReadError> {
        check_key(&self.state.specs, namespace, key)?;
        guarded(|| self.state.get(namespace, key))
            .and_then(|value| value)
            .map_err(|error| error.into_read("get"))
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
        Chunks {
            state: Arc::clone(&self.state),
            namespace,
            range,
            filter,
            walk: None,
            done: false,
            _item: PhantomData,
        }
    }
}

impl MaintenanceReader for LsmTreeReader {
    fn scan(&self, namespace: Namespace) -> Result<Vec<(RawKey, RawValue)>, ReadError> {
        let whole = ScanRange {
            lower: Bound::Unbounded,
            upper: Bound::Unbounded,
            direction: ScanDirection::Forward,
            chunk_bytes: NonZeroUsize::MAX,
        };
        let chunks = self.scan_range(namespace, whole, raw_entry);
        chunks
            .collect::<Result<Vec<_>, _>>()
            .map(|chunks| chunks.concat())
    }
}

impl ReadState {
    fn get(&self, namespace: Namespace, key: &[u8]) -> Result<Option<RawValue>, StoreError> {
        let index = position(&self.specs, namespace)
            .ok_or(StoreError::Internal("a checked key of no namespace"))?;
        match self.view.held(index) {
            Some(Durable::Sequence(pages)) => {
                let at = sequence_position(key).ok_or(StoreError::Internal(
                    "a checked sequence key of no position",
                ))?;
                match at.checked_sub(pages.len()) {
                    None => Ok(pages.record(at)?.map(<[u8]>::to_vec)),
                    Some(above) => Ok(usize::try_from(above)
                        .ok()
                        .and_then(|above| self.buffer.records(index)?.get(above))
                        .map(|record| record.to_vec())),
                }
            }
            Some(Durable::Map(snapshot)) => {
                match self
                    .buffer
                    .entries(index)
                    .and_then(|entries| entries.get(key))
                {
                    Some(Entry::Value(value)) => Ok(Some(value.to_vec())),
                    Some(Entry::Removed | Entry::Cancelled(_)) => Ok(None),
                    None => Ok(snapshot.lookup_with(key, |value, _| value.map(<[u8]>::to_vec))?),
                }
            }
            None => Err(StoreError::Internal("a namespace without its view")),
        }
    }
}

/// Lazy chunks of one range scan over one published state
struct Chunks<T, F> {
    state: Arc<ReadState>,
    namespace: Namespace,
    range: ScanRange,
    filter: F,
    walk: Option<Walk>,
    done: bool,
    _item: PhantomData<fn() -> T>,
}

/// Where a scan stands between chunks
enum Walk {
    /// Positions left, `start..end` (forward takes from the front, reverse from the back)
    Sequence { index: usize, left: Range<u64> },
    /// The durable cursor + where the buffer's entries resume
    Map {
        index: usize,
        cursor: Cursor,
        above: Above,
    },
}

/// The buffer's side of a map walk
#[derive(Debug, Clone)]
enum Above {
    /// Nothing taken yet: from the scan's own bound
    Start,
    /// Resume at this key, inclusive
    From(Bytes),
    /// Walked out
    Done,
}

impl<T, F> Iterator for Chunks<T, F>
where
    F: FnMut(&[u8], &[u8]) -> Option<T>,
{
    type Item = Result<Vec<T>, ReadError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let filled = guarded(|| self.fill())
            .unwrap_or_else(|panicked| Err(panicked.into_read("scan_range")));
        match filled {
            Ok(Some(chunk)) => Some(Ok(chunk)),
            Ok(None) => {
                self.done = true;
                None
            }
            Err(error) => {
                self.done = true;
                Some(Err(error))
            }
        }
    }
}

impl<T, F> Chunks<T, F>
where
    F: FnMut(&[u8], &[u8]) -> Option<T>,
{
    /// The next chunk (`None` = walked out)
    fn fill(&mut self) -> Result<Option<Vec<T>>, ReadError> {
        let lower = self.range.lower.as_ref().map(Vec::as_slice);
        let upper = self.range.upper.as_ref().map(Vec::as_slice);
        if self.walk.is_none() {
            let index = position(&self.state.specs, self.namespace)
                .ok_or_else(|| ReadError::NamespaceNotFound(self.namespace.to_string()))?;
            if is_empty_range(lower, upper) {
                return Ok(None);
            }
            let walk = self
                .walk_of(index)
                .map_err(|error| error.into_read("scan_range"))?;
            self.walk = Some(walk);
        }
        let reverse = self.range.direction == ScanDirection::Reverse;
        let budget = self.range.chunk_bytes.get();
        let state = &*self.state;
        let filled = match &mut self.walk {
            Some(Walk::Sequence { index, left }) => {
                fill_sequence(state, *index, left, reverse, budget, &mut self.filter)
            }
            Some(Walk::Map {
                index,
                cursor,
                above,
            }) => {
                let entries = state.buffer.entries(*index);
                let resumed = match &*above {
                    Above::Start => Some(None),
                    Above::From(key) => Some(Some(Bound::Included(key.as_ref()))),
                    Above::Done => None,
                };
                let bounds = resumed.map(|resumed| match reverse {
                    false => (resumed.unwrap_or(lower), upper),
                    true => (lower, resumed.unwrap_or(upper)),
                });
                let taken = match (entries, bounds) {
                    (Some(entries), Some(bounds)) if !is_empty_range(bounds.0, bounds.1) => {
                        let range = entries.range::<_, [u8]>(bounds);
                        match reverse {
                            false => fill_map(
                                cursor,
                                range.peekable(),
                                reverse,
                                budget,
                                &mut self.filter,
                            ),
                            true => {
                                let range = range.rev().peekable();
                                fill_map(cursor, range, reverse, budget, &mut self.filter)
                            }
                        }
                    }
                    _ => fill_map(
                        cursor,
                        std::iter::empty().peekable(),
                        reverse,
                        budget,
                        &mut self.filter,
                    ),
                };
                taken.map(|(chunk, next)| {
                    *above = next;
                    chunk
                })
            }
            None => Err(StoreError::Internal("a scan without its walk")),
        };
        filled.map_err(|error| error.into_read("scan_range"))
    }

    /// The walk over namespace `index`
    fn walk_of(&self, index: usize) -> Result<Walk, StoreError> {
        let state = &*self.state;
        let lower = self.range.lower.as_ref().map(Vec::as_slice);
        let upper = self.range.upper.as_ref().map(Vec::as_slice);
        match state.view.held(index) {
            Some(Durable::Sequence(pages)) => {
                let buffered = state
                    .buffer
                    .records(index)
                    .map_or(0, |records| records.len());
                let len = pages.len().saturating_add(buffered as u64);
                Ok(Walk::Sequence {
                    index,
                    left: positions(lower, upper, len),
                })
            }
            Some(Durable::Map(snapshot)) => Ok(Walk::Map {
                index,
                cursor: Cursor::new(
                    snapshot,
                    lower,
                    upper,
                    self.range.direction == ScanDirection::Reverse,
                )?,
                above: Above::Start,
            }),
            None => Err(StoreError::Internal("a namespace without its view")),
        }
    }
}

/// The kept records among up to `budget` visited bytes of records `left`, in walk order, durable
/// then buffered (`None` = nothing left to visit)
fn fill_sequence<T>(
    state: &ReadState,
    index: usize,
    left: &mut Range<u64>,
    reverse: bool,
    budget: usize,
    filter: &mut impl FnMut(&[u8], &[u8]) -> Option<T>,
) -> Result<Option<Vec<T>>, StoreError> {
    let Some(Durable::Sequence(pages)) = state.view.held(index) else {
        return Err(StoreError::Internal("a sequence walk over another kind"));
    };
    if left.is_empty() {
        return Ok(None);
    }
    pages.will_need(left.clone(), budget, reverse)?;
    let durable = pages.len();
    let mut chunk = Vec::new();
    let mut visited = Visited::default();
    while !left.is_empty() {
        let at = match reverse {
            false => left.start,
            true => left.end.saturating_sub(1),
        };
        let record = match at.checked_sub(durable) {
            None => pages.record(at)?,
            Some(above) => usize::try_from(above)
                .ok()
                .and_then(|above| state.buffer.records(index)?.get(above))
                .map(Bytes::as_ref),
        }
        .ok_or(StoreError::Internal("a sequence position past its records"))?;
        let key = at.to_be_bytes();
        if !visited.admits(&key, record, budget) {
            break;
        }
        chunk.extend(filter(&key, record));
        match reverse {
            false => left.start = left.start.saturating_add(1),
            true => left.end = at,
        }
    }
    Ok(Some(chunk))
}

/// Raw bytes a chunk has visited, kept or skipped
#[derive(Default)]
struct Visited {
    bytes: Option<usize>,
}

impl Visited {
    /// `key ‖ value` visited, unless it would take a chunk that visited something past `budget`
    fn admits(&mut self, key: &[u8], value: &[u8], budget: usize) -> bool {
        let size = key.len().saturating_add(value.len());
        match self.bytes {
            Some(bytes) if bytes.saturating_add(size) > budget => false,
            bytes => {
                self.bytes = Some(bytes.unwrap_or(0).saturating_add(size));
                true
            }
        }
    }

    fn any(&self) -> bool {
        self.bytes.is_some()
    }
}

/// Which side holds the next key of a map walk
#[derive(Clone, Copy)]
enum Next {
    Durable,
    Above,
    Both,
}

/// The kept rows among up to `budget` visited bytes of the durable cursor merged with the
/// buffer's `above` entries, in walk order (`None` = nothing left to visit); and where the
/// buffer resumes
///
/// - a buffered value or removal of a key masks its durable row
fn fill_map<'a, T>(
    cursor: &mut Cursor,
    mut above: Peekable<impl Iterator<Item = (&'a Bytes, &'a Entry)>>,
    reverse: bool,
    budget: usize,
    filter: &mut impl FnMut(&[u8], &[u8]) -> Option<T>,
) -> Result<(Option<Vec<T>>, Above), StoreError> {
    let mut chunk = Vec::new();
    let mut visited = Visited::default();
    loop {
        let durable = cursor.current()?;
        let buffered = above.peek().copied();
        let next = match (durable, buffered) {
            (None, None) => return Ok((visited.any().then_some(chunk), Above::Done)),
            (Some(_), None) => Next::Durable,
            (None, Some(_)) => Next::Above,
            (Some((key, _)), Some((other, _))) => match (key.cmp(other.as_ref()), reverse) {
                (std::cmp::Ordering::Equal, _) => Next::Both,
                (std::cmp::Ordering::Less, false) | (std::cmp::Ordering::Greater, true) => {
                    Next::Durable
                }
                _ => Next::Above,
            },
        };
        let row = match (next, durable, buffered) {
            (Next::Durable, Some(row), _) => Some(row),
            (Next::Above | Next::Both, _, Some((key, Entry::Value(value)))) => {
                Some((key.as_ref(), value.as_ref()))
            }
            _ => None,
        };
        if let Some((key, value)) = row {
            if !visited.admits(key, value, budget) {
                let resume = match buffered {
                    Some((key, _)) => Above::From(key.clone()),
                    None => Above::Done,
                };
                return Ok((Some(chunk), resume));
            }
            chunk.extend(filter(key, value));
        }
        if matches!(next, Next::Above | Next::Both) {
            above.next();
        }
        if matches!(next, Next::Durable | Next::Both) {
            cursor.advance()?;
        }
    }
}

/// Sequence positions between key bounds, within `len` (keys = 8-byte big-endian positions;
/// a bound of another length compares as bytes, like any key)
fn positions(lower: Bound<&[u8]>, upper: Bound<&[u8]>, len: u64) -> Range<u64> {
    let start = match lower {
        Bound::Unbounded => 0,
        Bound::Included(key) => ceil(key).0,
        Bound::Excluded(key) => {
            let (at, exact) = ceil(key);
            at.saturating_add(u128::from(exact))
        }
    };
    let end = match upper {
        Bound::Unbounded => u128::from(len),
        Bound::Included(key) => {
            let (at, exact) = ceil(key);
            at.saturating_add(u128::from(exact))
        }
        Bound::Excluded(key) => ceil(key).0,
    };
    let clamp = |at: u128| u64::try_from(at).unwrap_or(u64::MAX).min(len);
    let (start, end) = (clamp(start), clamp(end));
    start..end.max(start)
}

/// First position whose key is at or past `key`, and whether `key` is exactly that key
fn ceil(key: &[u8]) -> (u128, bool) {
    let mut padded = [0u8; 8];
    for (to, from) in padded.iter_mut().zip(key) {
        *to = *from;
    }
    let at = u128::from(u64::from_be_bytes(padded));
    match key.len().cmp(&padded.len()) {
        std::cmp::Ordering::Less => (at, false),
        std::cmp::Ordering::Equal => (at, true),
        std::cmp::Ordering::Greater => (at.saturating_add(1), false),
    }
}

#[cfg(test)]
mod tests;
