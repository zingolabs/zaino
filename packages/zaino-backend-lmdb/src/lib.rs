//! LMDB backend adapter for zaino-persistence.
//!
//! One LMDB named database per [`Namespace`]. Atomic commits via
//! write transactions. Zero-copy reads via memory-mapped files.
//!
//! ```ignore
//! let backend = LmdbBackend::open(LmdbConfig {
//!     path: "/tmp/zaino-db".into(),
//!     map_size_bytes: 1 << 30, // 1 GB
//!     namespaces: vec![
//!         NamespaceSpec { namespace: Namespace::new("headers"), key_order: KeyOrder::WalkOrdered },
//!         NamespaceSpec::meta(Namespace::new("_watermark")),
//!     ],
//! })?;
//! ```

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// Emit env-wide LMDB stats once every this many commits (under
/// `sync-profile`). A coarse cadence keeps the extra read off the per-batch
/// hot path while still tracking B-tree growth over a long sync.
#[cfg(feature = "sync-profile")]
const STATS_EVERY_N_COMMITS: u64 = 50;

use lmdb::{
    Cursor, Database, DatabaseFlags, Environment, EnvironmentFlags, Transaction, WriteFlags,
};
use lmdb_sys::{MDB_FIRST, MDB_NEXT, MDB_SET_RANGE};
use zaino_persistence::{
    Backend, BackendReader, BackendWriter, CommitError, FlushError, KeyOrder, Namespace,
    NamespaceSpec, OpenError, RangeVisitor, RawKey, RawValue, ReadError, WriteOp,
};

/// Configuration for [`LmdbBackend`].
pub struct LmdbConfig {
    /// Path to the LMDB environment directory.
    pub path: PathBuf,
    /// Maximum database size in bytes. LMDB requires this upfront.
    /// Defaults to 1 GB if not set.
    pub map_size_bytes: usize,
    /// Namespaces to create (one LMDB named database each), each paired with its
    /// [`KeyOrder`]. The order is carried for the
    /// deferral machinery; opening a database does not yet depend on it.
    pub namespaces: Vec<NamespaceSpec>,
}

impl Default for LmdbConfig {
    fn default() -> Self {
        Self {
            path: PathBuf::from("./zaino-db"),
            map_size_bytes: 1 << 30, // 1 GB
            namespaces: Vec::new(),
        }
    }
}

/// LMDB-backed persistence backend.
///
/// Holds the environment and a map of namespace → LMDB database handle.
/// Thread-safe: LMDB allows concurrent read transactions and serializes
/// writes internally.
///
/// `Clone` is cheap and shares one environment: the `env` is `Arc`-shared and
/// the `dbs` map holds `Copy` LMDB handles into it. Cloning yields another
/// handle onto the *same* database — as the engine and the store reader both
/// need when they operate over one backend.
#[derive(Clone)]
pub struct LmdbBackend {
    env: Arc<Environment>,
    dbs: HashMap<Namespace, Database>,
    /// Each namespace's [`KeyOrder`], so a writer selects
    /// [`WriteFlags::APPEND`] for the [`WalkOrdered`](KeyOrder::WalkOrdered)
    /// ones. Holds `Copy` entries; cloning per handle is cheap.
    key_orders: HashMap<Namespace, KeyOrder>,
    /// Commits so far, shared across the writers this backend hands out, so
    /// the periodic env-stats cadence holds across the fresh writer each batch
    /// opens. Present only under `sync-profile`.
    #[cfg(feature = "sync-profile")]
    commit_counter: Arc<std::sync::atomic::AtomicU64>,
}

impl LmdbBackend {
    /// Open or create an LMDB environment with the given namespaces.
    pub fn open(config: LmdbConfig) -> Result<Self, OpenError> {
        std::fs::create_dir_all(&config.path).map_err(|e| open_error("create directory", e))?;

        let env = Environment::new()
            .set_max_dbs(config.namespaces.len() as u32 + 1)
            .set_map_size(config.map_size_bytes)
            .set_flags(
                // NO_TLS: allows sharing read transactions across threads.
                // NO_READAHEAD: better for random-access patterns.
                // NO_SYNC: skip fsync per commit — we flush explicitly at
                // batch boundaries via Backend::flush(). Much faster for
                // batch writes; crash between flushes loses at most one batch
                // (the watermark ensures clean resume).
                EnvironmentFlags::NO_TLS
                    | EnvironmentFlags::NO_READAHEAD
                    | EnvironmentFlags::NO_SYNC,
            )
            .open(&config.path)
            .map_err(|e| open_error("open environment", e))?;

        let mut dbs = HashMap::new();
        let mut key_orders = HashMap::new();
        for spec in &config.namespaces {
            let db = open_or_create_db(&env, spec.namespace.as_str())
                .map_err(|e| open_error("create database", e))?;
            dbs.insert(spec.namespace, db);
            key_orders.insert(spec.namespace, spec.key_order);
        }

        Ok(Self {
            env: Arc::new(env),
            dbs,
            key_orders,
            #[cfg(feature = "sync-profile")]
            commit_counter: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        })
    }
}

/// Translate an LMDB write error into a [`CommitError`].
///
/// The exhausted-map case (`MDB_MAP_FULL`) maps to the precise
/// [`CommitError::OutOfSpace`] — a capacity condition the caller acts on (grow
/// the map), not corruption. Every other error is preserved as the *typed
/// source* of [`CommitError::WriteFailed`] (boxed, since the port must not name
/// `lmdb::Error`), never stringified — so the cause chain stays inspectable.
/// `matches!` keeps this to the one variant we distinguish without a catch-all
/// match over LMDB's error enum.
fn commit_error(operation: &'static str, error: lmdb::Error) -> CommitError {
    if matches!(error, lmdb::Error::MapFull) {
        CommitError::OutOfSpace
    } else {
        CommitError::WriteFailed {
            operation,
            source: Box::new(error),
        }
    }
}

/// Build an [`OpenError`] that keeps the underlying error as a typed source
/// (boxed at the port boundary), rather than stringifying it. Generic over the
/// cause so it serves both the filesystem (`io::Error`) and LMDB open steps.
fn open_error(
    operation: &'static str,
    source: impl std::error::Error + Send + Sync + 'static,
) -> OpenError {
    OpenError::Unavailable {
        operation,
        source: Box::new(source),
    }
}

/// Build a [`ReadError`] that keeps the LMDB error as a typed source (boxed at
/// the port boundary), rather than stringifying it.
fn read_error(operation: &'static str, source: lmdb::Error) -> ReadError {
    ReadError::ReadFailed {
        operation,
        source: Box::new(source),
    }
}

fn open_or_create_db(env: &Environment, name: &str) -> Result<Database, lmdb::Error> {
    match env.open_db(Some(name)) {
        Ok(db) => Ok(db),
        Err(lmdb::Error::NotFound) => env.create_db(Some(name), DatabaseFlags::empty()),
        Err(e) => Err(e),
    }
}

impl Backend for LmdbBackend {
    type Reader = LmdbReader;
    type Writer = LmdbWriter;

    fn reader(&self) -> Result<Self::Reader, OpenError> {
        Ok(LmdbReader {
            env: Arc::clone(&self.env),
            dbs: self.dbs.clone(),
        })
    }

    fn writer(&self) -> Result<Self::Writer, OpenError> {
        Ok(LmdbWriter {
            env: Arc::clone(&self.env),
            dbs: self.dbs.clone(),
            key_orders: self.key_orders.clone(),
            #[cfg(feature = "sync-profile")]
            commit_counter: Arc::clone(&self.commit_counter),
        })
    }

    fn flush(&self) -> Result<(), FlushError> {
        self.env
            .sync(true)
            .map_err(|e| FlushError::IoError(Box::new(e)))
    }
}

/// LMDB read handle.
pub struct LmdbReader {
    env: Arc<Environment>,
    dbs: HashMap<Namespace, Database>,
}

impl LmdbReader {
    fn resolve_db(&self, namespace: Namespace) -> Result<Database, ReadError> {
        self.dbs
            .get(&namespace)
            .copied()
            .ok_or_else(|| ReadError::NamespaceNotFound(namespace.to_string()))
    }
}

impl BackendReader for LmdbReader {
    fn get(&self, namespace: Namespace, key: &[u8]) -> Result<Option<RawValue>, ReadError> {
        let db = self.resolve_db(namespace)?;
        let txn = self
            .env
            .begin_ro_txn()
            .map_err(|e| read_error("begin read transaction", e))?;

        match txn.get(db, &key) {
            Ok(bytes) => Ok(Some(bytes.to_vec())),
            Err(lmdb::Error::NotFound) => Ok(None),
            Err(e) => Err(read_error("get", e)),
        }
    }

    fn scan(&self, namespace: Namespace) -> Result<Vec<(RawKey, RawValue)>, ReadError> {
        let db = self.resolve_db(namespace)?;
        let txn = self
            .env
            .begin_ro_txn()
            .map_err(|e| read_error("begin read transaction", e))?;

        let mut cursor = txn
            .open_ro_cursor(db)
            .map_err(|e| read_error("open cursor", e))?;

        let entries: Vec<(RawKey, RawValue)> = cursor
            .iter()
            .map(|(k, v)| (k.to_vec(), v.to_vec()))
            .collect();

        Ok(entries)
    }

    fn scan_range(
        &self,
        namespace: Namespace,
        start: &[u8],
        end_exclusive: &[u8],
        visit: &mut RangeVisitor<'_>,
    ) -> Result<(), ReadError> {
        let db = self.resolve_db(namespace)?;
        let txn = self
            .env
            .begin_ro_txn()
            .map_err(|e| read_error("begin read transaction", e))?;
        let cursor = txn
            .open_ro_cursor(db)
            .map_err(|e| read_error("open cursor", e))?;

        // Seek the first key >= `start` with `MDB_SET_RANGE`, then step forward
        // with `MDB_NEXT`. The raw cursor ops are used rather than the crate's
        // `iter_from`, which unwraps the seek and so panics when `start` is past
        // every key — a legitimate empty-range outcome, not a fault. A seek or
        // step that finds nothing returns `MDB_NOTFOUND`, surfaced as `None`.
        let mut entry = match cursor.get(Some(start), None, MDB_SET_RANGE) {
            Ok((Some(key), value)) => Some((key, value)),
            Ok((None, _)) => None,
            Err(lmdb::Error::NotFound) => None,
            Err(e) => return Err(read_error("seek range", e)),
        };
        while let Some((key, value)) = entry {
            if key >= end_exclusive {
                break;
            }
            if visit(key, value).is_break() {
                break;
            }
            entry = match cursor.get(None, None, MDB_NEXT) {
                Ok((Some(key), value)) => Some((key, value)),
                Ok((None, _)) => None,
                Err(lmdb::Error::NotFound) => None,
                Err(e) => return Err(read_error("step range", e)),
            };
        }
        Ok(())
    }

    fn first_key(&self, namespace: Namespace) -> Result<Option<RawKey>, ReadError> {
        let db = self.resolve_db(namespace)?;
        let txn = self
            .env
            .begin_ro_txn()
            .map_err(|e| read_error("begin read transaction", e))?;
        let cursor = txn
            .open_ro_cursor(db)
            .map_err(|e| read_error("open cursor", e))?;

        match cursor.get(None, None, MDB_FIRST) {
            Ok((Some(key), _)) => Ok(Some(key.to_vec())),
            Ok((None, _)) => Ok(None),
            Err(lmdb::Error::NotFound) => Ok(None),
            Err(e) => Err(read_error("first key", e)),
        }
    }
}

/// LMDB write handle.
pub struct LmdbWriter {
    env: Arc<Environment>,
    dbs: HashMap<Namespace, Database>,
    key_orders: HashMap<Namespace, KeyOrder>,
    #[cfg(feature = "sync-profile")]
    commit_counter: Arc<std::sync::atomic::AtomicU64>,
}

impl LmdbWriter {
    fn resolve_db(&self, namespace: Namespace) -> Result<Database, CommitError> {
        self.dbs
            .get(&namespace)
            .copied()
            .ok_or_else(|| CommitError::NamespaceNotFound(namespace.to_string()))
    }

    /// The write flags for a put into `namespace`.
    ///
    /// [`WalkOrdered`](KeyOrder::WalkOrdered) namespaces take
    /// [`WriteFlags::APPEND`]: their keys lead with the big-endian height and so
    /// arrive strictly ascending, which lets LMDB skip the B-tree search and fill
    /// pages sequentially. The append also *enforces* the order — a key that is
    /// not strictly greater than the current last key is rejected
    /// ([`CommitError::OutOfOrderAppend`]), so a mis-stated key order fails loudly
    /// rather than mis-ordering silently. Every other namespace
    /// ([`Scattered`](KeyOrder::Scattered), [`Meta`](KeyOrder::Meta), and an
    /// unknown namespace that `resolve_db` will reject anyway) takes a plain
    /// overwriting put.
    fn write_flags(&self, namespace: Namespace) -> WriteFlags {
        match self.key_orders.get(&namespace) {
            Some(KeyOrder::WalkOrdered) => WriteFlags::APPEND,
            _ => WriteFlags::empty(),
        }
    }

    /// Emit env-wide LMDB B-tree stats once every [`STATS_EVERY_N_COMMITS`]
    /// commits, read after the write txn has committed (never inside it).
    ///
    /// The `lmdb` crate exposes only `Environment::stat` (the environment's
    /// main database), not per-named-database stats — those would need the raw
    /// `mdb_stat` ffi on each db handle, which this crate cannot reach without
    /// `unsafe`. The figures are therefore env-wide, not per-index.
    #[cfg(feature = "sync-profile")]
    fn maybe_emit_env_stats(&self) {
        use std::sync::atomic::Ordering;
        let commits = self.commit_counter.fetch_add(1, Ordering::Relaxed) + 1;
        if !commits.is_multiple_of(STATS_EVERY_N_COMMITS) {
            return;
        }
        match self.env.stat() {
            Ok(stat) => tracing::info!(
                commits,
                page_size = stat.page_size(),
                depth = stat.depth(),
                branch_pages = stat.branch_pages(),
                leaf_pages = stat.leaf_pages(),
                overflow_pages = stat.overflow_pages(),
                entries = stat.entries(),
                "lmdb env stats"
            ),
            Err(error) => tracing::debug!(%error, "lmdb env stats unavailable"),
        }
    }
}

impl BackendWriter for LmdbWriter {
    fn commit(&mut self, ops: Vec<WriteOp>) -> Result<(), CommitError> {
        #[cfg(feature = "sync-profile")]
        let op_count = ops.len();
        #[cfg(feature = "sync-profile")]
        let put_start = std::time::Instant::now();

        let mut txn = self
            .env
            .begin_rw_txn()
            .map_err(|e| commit_error("begin rw txn", e))?;

        for op in ops {
            match op {
                WriteOp::Put {
                    namespace,
                    key,
                    value,
                } => {
                    let db = self.resolve_db(namespace)?;
                    let flags = self.write_flags(namespace);
                    match txn.put(db, &key, &value, flags) {
                        Ok(()) => {}
                        // An append rejects a key that is not strictly greater
                        // than the current last one with `MDB_KEYEXIST`. Only an
                        // append can produce it here (a plain put overwrites), so
                        // guarding on the flag keeps a plain-put `KeyExist` — were
                        // one ever to arise — on the generic write-failure path.
                        Err(e @ lmdb::Error::KeyExist) if flags.contains(WriteFlags::APPEND) => {
                            return Err(CommitError::OutOfOrderAppend {
                                namespace: namespace.to_string(),
                                source: Box::new(e),
                            });
                        }
                        Err(e) => return Err(commit_error("put", e)),
                    }
                }
                WriteOp::Delete { namespace, key } => {
                    let db = self.resolve_db(namespace)?;
                    match txn.del(db, &key, None) {
                        Ok(()) | Err(lmdb::Error::NotFound) => {}
                        Err(e) => {
                            return Err(commit_error("delete", e));
                        }
                    }
                }
            }
        }

        // The put loop (building the write txn in memory) is measured
        // separately from `txn.commit()` (the flush), because the I/O cost the
        // profiling exists to attribute lives in the flush, not the puts.
        #[cfg(feature = "sync-profile")]
        let put_ms = put_start.elapsed().as_secs_f64() * 1000.0;
        #[cfg(feature = "sync-profile")]
        let flush_start = std::time::Instant::now();

        txn.commit().map_err(|e| commit_error("commit", e))?;

        #[cfg(feature = "sync-profile")]
        {
            let flush_ms = flush_start.elapsed().as_secs_f64() * 1000.0;
            // Emitted inside the engine's `sync_commit` span, so Loki carries
            // this split alongside that span's batch and committed_height.
            tracing::info!(put_ms, flush_ms, op_count, "lmdb commit split");
            self.maybe_emit_env_stats();
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::ops::ControlFlow;

    fn test_config(dir: &std::path::Path, namespaces: Vec<Namespace>) -> LmdbConfig {
        LmdbConfig {
            path: dir.to_path_buf(),
            map_size_bytes: 1 << 20, // 1 MB for tests
            namespaces: namespaces.into_iter().map(spec).collect(),
        }
    }

    /// A namespace spec for these raw-KV tests. They exercise the generic
    /// put/scan/delete/range path, not key ordering, so `Scattered` — the plain
    /// overwriting put, with no append-order constraint — is the neutral choice.
    /// Append enforcement for `WalkOrdered` namespaces is covered by
    /// [`walk_ordered_rejects_descending_key`] and the conformance suite.
    fn spec(namespace: Namespace) -> NamespaceSpec {
        NamespaceSpec {
            namespace,
            key_order: KeyOrder::Scattered,
        }
    }

    #[test]
    fn open_and_write_read() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ns = Namespace::new("test_ns");
        let backend = LmdbBackend::open(test_config(tmp.path(), vec![ns])).expect("open");

        // Write
        let mut writer = backend.writer().expect("writer");
        writer
            .commit(vec![WriteOp::Put {
                namespace: ns,
                key: b"hello".to_vec(),
                value: b"world".to_vec(),
            }])
            .expect("commit");

        // Read
        let reader = backend.reader().expect("reader");
        let val = reader.get(ns, b"hello").expect("get").expect("exists");
        assert_eq!(val, b"world");
    }

    #[test]
    fn scan_returns_all_entries() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ns = Namespace::new("scan_ns");
        let backend = LmdbBackend::open(test_config(tmp.path(), vec![ns])).expect("open");

        let mut writer = backend.writer().expect("writer");
        writer
            .commit(vec![
                WriteOp::Put {
                    namespace: ns,
                    key: b"a".to_vec(),
                    value: b"1".to_vec(),
                },
                WriteOp::Put {
                    namespace: ns,
                    key: b"b".to_vec(),
                    value: b"2".to_vec(),
                },
                WriteOp::Put {
                    namespace: ns,
                    key: b"c".to_vec(),
                    value: b"3".to_vec(),
                },
            ])
            .expect("commit");

        let reader = backend.reader().expect("reader");
        let entries = reader.scan(ns).expect("scan");
        assert_eq!(entries.len(), 3);
        // LMDB returns in key order
        assert_eq!(entries[0], (b"a".to_vec(), b"1".to_vec()));
        assert_eq!(entries[2], (b"c".to_vec(), b"3".to_vec()));
    }

    /// Collect the entries `scan_range` visits over `[start, end_exclusive)`.
    fn collect_range(
        reader: &LmdbReader,
        ns: Namespace,
        start: &[u8],
        end_exclusive: &[u8],
    ) -> Vec<(RawKey, RawValue)> {
        let mut out = Vec::new();
        reader
            .scan_range(ns, start, end_exclusive, &mut |k, v| {
                out.push((k.to_vec(), v.to_vec()));
                ControlFlow::Continue(())
            })
            .expect("scan_range");
        out
    }

    fn seeded_range_backend() -> (tempfile::TempDir, LmdbBackend, Namespace) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ns = Namespace::new("range_ns");
        let backend = LmdbBackend::open(test_config(tmp.path(), vec![ns])).expect("open");
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
        (tmp, backend, ns)
    }

    #[test]
    fn scan_range_end_is_exclusive() {
        let (_tmp, backend, ns) = seeded_range_backend();
        let reader = backend.reader().expect("reader");
        // [a1, a3) excludes a3.
        let got = collect_range(&reader, ns, b"a1", b"a3");
        let keys: Vec<RawKey> = got.into_iter().map(|(k, _)| k).collect();
        assert_eq!(keys, vec![b"a1".to_vec(), b"a2".to_vec()]);
    }

    #[test]
    fn scan_range_empty_when_start_equals_end() {
        let (_tmp, backend, ns) = seeded_range_backend();
        let reader = backend.reader().expect("reader");
        assert!(collect_range(&reader, ns, b"a2", b"a2").is_empty());
    }

    #[test]
    fn scan_range_matching_no_prefix_is_empty_not_an_error() {
        let (_tmp, backend, ns) = seeded_range_backend();
        let reader = backend.reader().expect("reader");
        // A prefix between the stored keys, and one past every key: both empty,
        // the latter exercising the `MDB_SET_RANGE` seek-past-the-end path that
        // must not panic.
        assert!(collect_range(&reader, ns, b"a9", b"b0").is_empty());
        assert!(collect_range(&reader, ns, b"zzz", b"zzz\xff").is_empty());
    }

    #[test]
    fn scan_range_does_not_leak_neighbouring_prefixes() {
        let (_tmp, backend, ns) = seeded_range_backend();
        let reader = backend.reader().expect("reader");
        // The `a` prefix is [a, b); not one `b` key leaks in.
        let keys: Vec<RawKey> = collect_range(&reader, ns, b"a", b"b")
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(
            keys,
            vec![b"a1".to_vec(), b"a2".to_vec(), b"a3".to_vec()],
            "only the a-prefixed keys, never b1/b2"
        );
    }

    #[test]
    fn scan_range_stops_on_break() {
        let (_tmp, backend, ns) = seeded_range_backend();
        let reader = backend.reader().expect("reader");
        let mut seen = Vec::new();
        reader
            .scan_range(ns, b"a", b"b", &mut |k, _| {
                seen.push(k.to_vec());
                // Stop after the first entry.
                ControlFlow::Break(())
            })
            .expect("scan_range");
        assert_eq!(seen, vec![b"a1".to_vec()], "early stop reads no further");
    }

    #[test]
    fn first_key_is_the_smallest_or_none_when_empty() {
        let (_tmp, backend, ns) = seeded_range_backend();
        let reader = backend.reader().expect("reader");
        assert_eq!(
            reader.first_key(ns).expect("first_key"),
            Some(b"a1".to_vec())
        );

        let tmp = tempfile::tempdir().expect("tempdir");
        let empty_ns = Namespace::new("empty_first");
        let empty = LmdbBackend::open(test_config(tmp.path(), vec![empty_ns])).expect("open");
        let reader = empty.reader().expect("reader");
        assert_eq!(reader.first_key(empty_ns).expect("first_key"), None);
    }

    #[test]
    fn get_missing_key_returns_none() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ns = Namespace::new("empty_ns");
        let backend = LmdbBackend::open(test_config(tmp.path(), vec![ns])).expect("open");

        let reader = backend.reader().expect("reader");
        assert!(reader.get(ns, b"nope").expect("get").is_none());
    }

    #[test]
    fn unknown_namespace_is_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = LmdbBackend::open(test_config(tmp.path(), vec![Namespace::new("known")]))
            .expect("open");

        let reader = backend.reader().expect("reader");
        let err = reader.get(Namespace::new("unknown"), b"key").unwrap_err();
        assert!(matches!(err, ReadError::NamespaceNotFound(_)));
    }

    #[test]
    fn delete_removes_entry() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ns = Namespace::new("del_ns");
        let backend = LmdbBackend::open(test_config(tmp.path(), vec![ns])).expect("open");

        let mut writer = backend.writer().expect("writer");
        writer
            .commit(vec![WriteOp::Put {
                namespace: ns,
                key: b"gone".to_vec(),
                value: b"soon".to_vec(),
            }])
            .expect("put");

        writer
            .commit(vec![WriteOp::Delete {
                namespace: ns,
                key: b"gone".to_vec(),
            }])
            .expect("delete");

        let reader = backend.reader().expect("reader");
        assert!(reader.get(ns, b"gone").expect("get").is_none());
    }

    #[test]
    fn atomic_commit_all_or_nothing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ns = Namespace::new("atomic_ns");
        let backend = LmdbBackend::open(test_config(tmp.path(), vec![ns])).expect("open");

        // Put two entries in one commit
        let mut writer = backend.writer().expect("writer");
        writer
            .commit(vec![
                WriteOp::Put {
                    namespace: ns,
                    key: b"k1".to_vec(),
                    value: b"v1".to_vec(),
                },
                WriteOp::Put {
                    namespace: ns,
                    key: b"k2".to_vec(),
                    value: b"v2".to_vec(),
                },
            ])
            .expect("commit");

        let reader = backend.reader().expect("reader");
        assert!(reader.get(ns, b"k1").expect("get").is_some());
        assert!(reader.get(ns, b"k2").expect("get").is_some());
    }

    #[test]
    fn exhausting_the_map_reports_out_of_space() {
        // A tiny map so a modest write overflows it. LMDB signals this with
        // MDB_MAP_FULL, which must surface as the precise OutOfSpace variant —
        // not a stringified WriteFailed — so a caller can act on it (grow the
        // map) rather than mistake it for corruption or a transient fault.
        let tmp = tempfile::tempdir().expect("tempdir");
        let ns = Namespace::new("full_ns");
        let config = LmdbConfig {
            path: tmp.path().to_path_buf(),
            map_size_bytes: 64 << 10, // 64 KiB
            namespaces: vec![spec(ns)],
        };
        let backend = LmdbBackend::open(config).expect("open");
        let mut writer = backend.writer().expect("writer");

        let big = vec![0u8; 256 << 10]; // 256 KiB > the whole map
        let err = writer
            .commit(vec![WriteOp::Put {
                namespace: ns,
                key: b"big".to_vec(),
                value: big,
            }])
            .expect_err("a value larger than the map cannot be committed");
        assert!(
            matches!(err, CommitError::OutOfSpace),
            "map-full must map to OutOfSpace, got: {err:?}"
        );
    }

    #[test]
    fn reopen_persists_data() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ns = Namespace::new("persist_ns");

        // Session 1: write
        {
            let backend = LmdbBackend::open(test_config(tmp.path(), vec![ns])).expect("open");
            let mut writer = backend.writer().expect("writer");
            writer
                .commit(vec![WriteOp::Put {
                    namespace: ns,
                    key: b"durable".to_vec(),
                    value: b"yes".to_vec(),
                }])
                .expect("commit");
            backend.flush().expect("flush");
        }

        // Session 2: read
        {
            let backend = LmdbBackend::open(test_config(tmp.path(), vec![ns])).expect("reopen");
            let reader = backend.reader().expect("reader");
            let val = reader.get(ns, b"durable").expect("get").expect("persisted");
            assert_eq!(val, b"yes");
        }
    }

    /// A `WalkOrdered` namespace rejects a key that is not strictly greater than
    /// its last — both a lower key and a repeat — with the typed
    /// `OutOfOrderAppend` naming it; a `Scattered` namespace accepts the same
    /// descending sequence. This is the append enforcement the spec relies on to
    /// fail a mis-stated key order loudly.
    #[test]
    fn walk_ordered_rejects_descending_key() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let walk = Namespace::new("walk");
        let scattered = Namespace::new("scattered");
        let config = LmdbConfig {
            path: tmp.path().to_path_buf(),
            map_size_bytes: 1 << 20,
            namespaces: vec![
                NamespaceSpec {
                    namespace: walk,
                    key_order: KeyOrder::WalkOrdered,
                },
                NamespaceSpec {
                    namespace: scattered,
                    key_order: KeyOrder::Scattered,
                },
            ],
        };
        let backend = LmdbBackend::open(config).expect("open");
        let mut writer = backend.writer().expect("writer");

        let height_key = |height: u32| height.to_be_bytes().to_vec();
        // The value echoes the key; its exact bytes are immaterial to the test.
        let put = |namespace, height: u32| WriteOp::Put {
            namespace,
            key: height_key(height),
            value: height_key(height),
        };

        // Walk-ordered: an ascending append is fine; a lower key, then a repeat
        // of the last key, each fail with OutOfOrderAppend naming the namespace.
        writer.commit(vec![put(walk, 5)]).expect("ascending append");
        for regress in [3u32, 5u32] {
            let err = writer
                .commit(vec![put(walk, regress)])
                .expect_err("a non-ascending key must be rejected");
            assert!(
                matches!(&err, CommitError::OutOfOrderAppend { namespace, .. } if namespace == walk.as_str()),
                "expected OutOfOrderAppend naming {walk}, got: {err:?}"
            );
        }

        // Scattered: the same descending sequence is accepted and stored.
        writer.commit(vec![put(scattered, 5)]).expect("scattered 5");
        writer
            .commit(vec![put(scattered, 3)])
            .expect("scattered accepts a descending key");

        let reader = backend.reader().expect("reader");
        assert_eq!(
            reader.get(scattered, &height_key(3)).expect("get"),
            Some(height_key(3))
        );
        assert_eq!(
            reader.get(scattered, &height_key(5)).expect("get"),
            Some(height_key(5))
        );
        // The rejected walk puts left nothing behind: only the one accepted key.
        assert_eq!(reader.get(walk, &height_key(3)).expect("get"), None);
        assert_eq!(
            reader.get(walk, &height_key(5)).expect("get"),
            Some(height_key(5))
        );
    }
}

/// The generic backend conformance suite ([`zaino_persistence::conformance`]),
/// run against the LMDB backend.
///
/// LMDB is persistent, so the factory's [`reopen`](conformance::BackendFactory::reopen)
/// reopens the same temp-dir environment and the persistence and restart
/// properties run for real (unlike the in-memory backend, whose `reopen` is
/// `None`). One `#[test]` per property gives a precise failure site; the
/// aggregate [`run_all`](conformance::run_all) guards against a property being
/// added upstream and not wired here.
#[cfg(test)]
mod conformance_tests {
    use std::sync::Mutex;

    use super::{LmdbBackend, LmdbConfig};
    use zaino_persistence::conformance::{self, BackendFactory};
    use zaino_persistence::NamespaceSpec;

    /// Opens LMDB environments under one temp dir for a conformance run.
    ///
    /// [`fresh`](BackendFactory::fresh) must hand back an *empty* backend every
    /// time — [`run_all`](conformance::run_all) calls it once per property on the
    /// same factory — while [`reopen`](BackendFactory::reopen) must reopen the
    /// exact storage the most recent `fresh` created. So each `fresh` allocates a
    /// new, never-before-used generation subdirectory (LMDB creates it empty) and
    /// records it; `reopen` reopens that same generation.
    struct LmdbFactory {
        root: tempfile::TempDir,
        generation: Mutex<u32>,
    }

    impl LmdbFactory {
        fn new() -> Self {
            Self {
                root: tempfile::tempdir().expect("tempdir"),
                generation: Mutex::new(0),
            }
        }

        fn config(&self, generation: u32, namespaces: &[NamespaceSpec]) -> LmdbConfig {
            LmdbConfig {
                path: self.root.path().join(format!("gen-{generation}")),
                map_size_bytes: 1 << 20, // 1 MiB: the conformance data is tiny.
                namespaces: namespaces.to_vec(),
            }
        }

        fn next_generation(&self) -> u32 {
            let mut generation = self.generation.lock().expect("generation mutex poisoned");
            *generation += 1;
            *generation
        }

        fn current_generation(&self) -> u32 {
            *self.generation.lock().expect("generation mutex poisoned")
        }
    }

    impl BackendFactory for LmdbFactory {
        type B = LmdbBackend;

        fn fresh(&self, namespaces: &[NamespaceSpec]) -> Self::B {
            let generation = self.next_generation();
            LmdbBackend::open(self.config(generation, namespaces)).expect("open lmdb backend")
        }

        fn reopen(&self, namespaces: &[NamespaceSpec]) -> Option<Self::B> {
            let generation = self.current_generation();
            Some(
                LmdbBackend::open(self.config(generation, namespaces))
                    .expect("reopen lmdb backend"),
            )
        }
    }

    #[test]
    fn get_put_delete_round_trip() {
        conformance::get_put_delete_round_trip(&LmdbFactory::new());
    }

    #[test]
    fn commit_is_atomic_across_namespaces() {
        conformance::commit_is_atomic_across_namespaces(&LmdbFactory::new());
    }

    #[test]
    fn scan_returns_bytewise_key_order() {
        conformance::scan_returns_bytewise_key_order(&LmdbFactory::new());
    }

    #[test]
    fn scan_range_is_ascending_and_half_open() {
        conformance::scan_range_is_ascending_and_half_open(&LmdbFactory::new());
    }

    #[test]
    fn first_key_is_smallest_or_none() {
        conformance::first_key_is_smallest_or_none(&LmdbFactory::new());
    }

    #[test]
    fn namespaces_are_isolated() {
        conformance::namespaces_are_isolated(&LmdbFactory::new());
    }

    #[test]
    fn walk_ordered_rejects_or_stores_non_ascending_put() {
        conformance::walk_ordered_rejects_or_stores_non_ascending_put(&LmdbFactory::new());
    }

    #[test]
    fn reopen_persists_committed_data() {
        conformance::reopen_persists_committed_data(&LmdbFactory::new());
    }

    #[test]
    fn bulk_disabled_matches_direct() {
        conformance::bulk_disabled_matches_direct(&LmdbFactory::new());
    }

    #[test]
    fn bulk_enabled_after_finish_matches_direct() {
        conformance::bulk_enabled_after_finish_matches_direct(&LmdbFactory::new());
    }

    #[test]
    fn is_complete_true_outside_bulk_mode() {
        conformance::is_complete_true_outside_bulk_mode(&LmdbFactory::new());
    }

    #[test]
    fn is_complete_inside_bulk_mode() {
        conformance::is_complete_inside_bulk_mode(&LmdbFactory::new());
    }

    #[test]
    fn finish_bulk_is_idempotent() {
        conformance::finish_bulk_is_idempotent(&LmdbFactory::new());
    }

    #[test]
    fn restart_in_bulk_mode_matches_direct() {
        conformance::restart_in_bulk_mode_matches_direct(&LmdbFactory::new());
    }

    #[test]
    fn run_all_aggregate() {
        conformance::run_all(&LmdbFactory::new());
    }
}
