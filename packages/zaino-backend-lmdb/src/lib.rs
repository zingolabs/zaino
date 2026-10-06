//! LMDB backend adapter for zaino-persistence.
//!
//! One LMDB named database per [`Namespace`]. Atomic commits via
//! write transactions. Zero-copy reads via memory-mapped files.
//!
//! ```ignore
//! let backend = LmdbBackend::open(LmdbConfig {
//!     path: "/tmp/zaino-db".into(),
//!     map_size_bytes: 1 << 30, // 1 GB
//!     namespaces: &["headers", "tx_count", "_engine_meta"],
//! })?;
//! ```
//!
//! # Every method here blocks
//!
//! There is no async in this crate and none of it is cheap:
//!
//! - `get` and `scan` fault pages in from disk.
//! - `commit` waits on LMDB's single-writer lock, then on page writes.
//! - `flush` waits on `fsync`.
//!
//! None of that may run on an async runtime's worker. A caller inside a task
//! places the call itself — `tokio::task::spawn_blocking` for one commit batch
//! or one scan, never once per key, and not `block_in_place`, which panics on a
//! current-thread runtime.

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
    Backend, BackendReader, BackendWriter, CommitError, FlushError, Namespace, OpenError,
    RangeVisitor, RawKey, RawValue, ReadError, WriteOp,
};

/// Configuration for [`LmdbBackend`].
pub struct LmdbConfig {
    /// Path to the LMDB environment directory.
    pub path: PathBuf,
    /// Maximum database size in bytes. LMDB requires this upfront.
    /// Defaults to 1 GB if not set.
    pub map_size_bytes: usize,
    /// Namespaces to create (one LMDB named database each).
    pub namespaces: Vec<Namespace>,
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
    /// Commits so far, shared across the writers this backend hands out, so
    /// the periodic env-stats cadence holds across the fresh writer each batch
    /// opens. Present only under `sync-profile`.
    #[cfg(feature = "sync-profile")]
    commit_counter: Arc<std::sync::atomic::AtomicU64>,
}

/// More namespaces than LMDB can be told to hold: the count plus the root
/// database does not fit the `u32` [`Environment::set_max_dbs`] takes.
///
/// Unreachable for any real index set, which is the point of naming it — the
/// alternative is a cast that would wrap and quietly configure a smaller limit
/// than the caller asked for.
#[derive(Debug, thiserror::Error)]
#[error("{count} namespaces exceeds the maximum database count LMDB accepts")]
struct TooManyNamespaces {
    count: usize,
}

impl LmdbBackend {
    /// Open or create an LMDB environment with the given namespaces.
    pub fn open(config: LmdbConfig) -> Result<Self, OpenError> {
        std::fs::create_dir_all(&config.path).map_err(|e| open_error("create directory", e))?;

        // One database per namespace, plus LMDB's unnamed root database, which
        // holds the names of the rest.
        let max_dbs = u32::try_from(config.namespaces.len())
            .ok()
            .and_then(|count| count.checked_add(1))
            .ok_or(TooManyNamespaces {
                count: config.namespaces.len(),
            })
            .map_err(|e| open_error("count namespaces", e))?;

        let env = Environment::new()
            .set_max_dbs(max_dbs)
            .set_map_size(config.map_size_bytes)
            .set_flags(
                // NO_TLS: allows sharing read transactions across threads.
                // NO_READAHEAD: better for random-access patterns.
                // NO_SYNC: no fsync per commit. Durability is the caller's to
                // force, through `Backend::flush`.
                //
                // What this costs, precisely: LMDB documents that under
                // NO_SYNC "a system crash can corrupt the database or lose the
                // last transactions", and the integrity half of that is
                // conditional — transactions keep atomicity, consistency and
                // isolation (losing only durability) *if the filesystem
                // preserves write order* and WRITE_MAP is unused. The second
                // condition holds here; the first is a property of the
                // deployment's filesystem, not something this crate can
                // assert.
                //
                // So a crash can lose every commit since the last `flush`, not
                // one batch, and on a filesystem that reorders writes it can
                // leave an environment that will not open. The watermark keeps
                // a *recoverable* store honest — it is written in the same
                // transaction as the data it vouches for, so it can never lead
                // it — but it cannot help an environment that fails to open.
                EnvironmentFlags::NO_TLS
                    | EnvironmentFlags::NO_READAHEAD
                    | EnvironmentFlags::NO_SYNC,
            )
            .open(&config.path)
            .map_err(|e| open_error("open environment", e))?;

        let mut dbs = HashMap::new();
        for ns in &config.namespaces {
            let db = open_or_create_db(&env, ns.as_str())
                .map_err(|e| open_error("create database", e))?;
            dbs.insert(*ns, db);
        }

        Ok(Self {
            env: Arc::new(env),
            dbs,
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

/// Blocking: `flush` waits on `fsync`. See the crate docs.
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

/// Blocking: both methods fault pages in from disk. See the crate docs.
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

    /// Every entry in `namespace`, in the backend's key order.
    ///
    /// The cursor is stepped explicitly rather than through `Cursor::iter`.
    /// That iterator reports *every* `mdb_cursor_get` failure as the end of the
    /// sequence, guarded only by a `debug_assert!` — so in a release build a
    /// mid-scan `MDB_CORRUPTED` or `MDB_PAGE_NOTFOUND` would end iteration and
    /// return `Ok` holding part of the namespace, which a caller cannot
    /// distinguish from the whole of it. Stepping here keeps "the sequence
    /// ended" (`MDB_NOTFOUND`) and "the read failed" as different outcomes, so
    /// a partial scan is an error and never a short success.
    fn scan(&self, namespace: Namespace) -> Result<Vec<(RawKey, RawValue)>, ReadError> {
        let db = self.resolve_db(namespace)?;
        let txn = self
            .env
            .begin_ro_txn()
            .map_err(|e| read_error("begin read transaction", e))?;

        let cursor = txn
            .open_ro_cursor(db)
            .map_err(|e| read_error("open cursor", e))?;

        // Step with the raw cursor ops (as `scan_range`/`first_key` do) rather
        // than `Cursor::iter`: `MDB_FIRST` then `MDB_NEXT`, so the cursor ends
        // on `MDB_NOTFOUND` and any other LMDB error surfaces instead of being
        // read as the end of the sequence.
        let mut entries = Vec::new();
        let mut step = MDB_FIRST;
        loop {
            match cursor.get(None, None, step) {
                Ok((Some(key), value)) => entries.push((key.to_vec(), value.to_vec())),
                // The one non-failure: the cursor ran past the last entry.
                Ok((None, _)) | Err(lmdb::Error::NotFound) => return Ok(entries),
                Err(e) => return Err(read_error("scan", e)),
            }
            step = MDB_NEXT;
        }
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

/// Blocking: `commit` waits on LMDB's single-writer lock, then on page writes.
/// See the crate docs.
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
                    txn.put(db, &key, &value, WriteFlags::empty())
                        .map_err(|e| commit_error("put", e))?;
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
            namespaces,
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

    /// `scan` yields entries in key order, whatever order they were written
    /// in — the property a height-keyed index relies on, and the reason keys
    /// are encoded big-endian.
    #[test]
    fn scan_yields_entries_in_key_order() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ns = Namespace::new("order_ns");
        let backend = LmdbBackend::open(test_config(tmp.path(), vec![ns])).expect("open");

        // Written back to front, so insertion order cannot explain the result.
        let keys: Vec<Vec<u8>> = [3u32, 1, 2, 0]
            .iter()
            .map(|n| n.to_be_bytes().to_vec())
            .collect();
        let mut writer = backend.writer().expect("writer");
        writer
            .commit(
                keys.iter()
                    .map(|key| WriteOp::Put {
                        namespace: ns,
                        key: key.clone(),
                        value: key.clone(),
                    })
                    .collect(),
            )
            .expect("commit");

        let reader = backend.reader().expect("reader");
        let scanned: Vec<Vec<u8>> = reader
            .scan(ns)
            .expect("scan")
            .into_iter()
            .map(|(key, _)| key)
            .collect();

        let mut expected = keys;
        expected.sort();
        assert_eq!(scanned, expected);
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
            namespaces: vec![ns],
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
}
