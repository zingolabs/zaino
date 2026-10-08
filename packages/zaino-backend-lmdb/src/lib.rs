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

use lmdb::{
    Cursor, Database, DatabaseFlags, Environment, EnvironmentFlags, Transaction, WriteFlags,
};
use zaino_persistence::{
    Backend, BackendReader, BackendWriter, CommitError, FlushError, Namespace, OpenError, RawKey,
    RawValue, ReadError, WriteOp,
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

        let mut entries = Vec::new();
        let mut step = lmdb_sys::MDB_FIRST;
        loop {
            match cursor.get(None, None, step) {
                Ok((key, value)) => {
                    let key = key.expect("MDB_FIRST/MDB_NEXT always yield a key");
                    entries.push((key.to_vec(), value.to_vec()));
                }
                // The one non-failure: the cursor ran past the last entry.
                Err(lmdb::Error::NotFound) => return Ok(entries),
                Err(e) => return Err(read_error("scan", e)),
            }
            step = lmdb_sys::MDB_NEXT;
        }
    }
}

/// LMDB write handle.
pub struct LmdbWriter {
    env: Arc<Environment>,
    dbs: HashMap<Namespace, Database>,
}

impl LmdbWriter {
    fn resolve_db(&self, namespace: Namespace) -> Result<Database, CommitError> {
        self.dbs
            .get(&namespace)
            .copied()
            .ok_or_else(|| CommitError::NamespaceNotFound(namespace.to_string()))
    }
}

/// Blocking: `commit` waits on LMDB's single-writer lock, then on page writes.
/// See the crate docs.
impl BackendWriter for LmdbWriter {
    fn commit(&mut self, ops: Vec<WriteOp>) -> Result<(), CommitError> {
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

        txn.commit().map_err(|e| commit_error("commit", e))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
