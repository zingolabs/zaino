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

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use lmdb::{
    Cursor, Database, DatabaseFlags, Environment, EnvironmentFlags, Transaction, WriteFlags,
};
use zaino_persistence::{
    Backend, BackendReader, BackendWriter, CommitError, FlushError, Namespace, OpenError,
    RawKey, RawValue, ReadError, WriteOp,
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
pub struct LmdbBackend {
    env: Arc<Environment>,
    dbs: HashMap<Namespace, Database>,
}

impl LmdbBackend {
    /// Open or create an LMDB environment with the given namespaces.
    pub fn open(config: LmdbConfig) -> Result<Self, OpenError> {
        std::fs::create_dir_all(&config.path)
            .map_err(|e| OpenError::Unavailable(format!("create dir: {e}")))?;

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
            .map_err(|e| OpenError::Unavailable(format!("lmdb open: {e}")))?;

        let mut dbs = HashMap::new();
        for ns in &config.namespaces {
            let db = open_or_create_db(&env, ns.as_str())
                .map_err(|e| OpenError::Unavailable(format!("create db {ns}: {e}")))?;
            dbs.insert(*ns, db);
        }

        Ok(Self {
            env: Arc::new(env),
            dbs,
        })
    }

    fn resolve_db(&self, namespace: Namespace) -> Result<Database, ReadError> {
        self.dbs
            .get(&namespace)
            .copied()
            .ok_or_else(|| ReadError::NamespaceNotFound(namespace.to_string()))
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
        })
    }

    fn flush(&self) -> Result<(), FlushError> {
        self.env
            .sync(true)
            .map_err(|e| FlushError::IoError(format!("lmdb sync: {e}")))
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
            .map_err(|e| ReadError::ReadFailed(format!("begin ro txn: {e}")))?;

        match txn.get(db, &key) {
            Ok(bytes) => Ok(Some(bytes.to_vec())),
            Err(lmdb::Error::NotFound) => Ok(None),
            Err(e) => Err(ReadError::ReadFailed(format!("get: {e}"))),
        }
    }

    fn scan(&self, namespace: Namespace) -> Result<Vec<(RawKey, RawValue)>, ReadError> {
        let db = self.resolve_db(namespace)?;
        let txn = self
            .env
            .begin_ro_txn()
            .map_err(|e| ReadError::ReadFailed(format!("begin ro txn: {e}")))?;

        let mut cursor = txn
            .open_ro_cursor(db)
            .map_err(|e| ReadError::ReadFailed(format!("open cursor: {e}")))?;

        let entries: Vec<(RawKey, RawValue)> = cursor
            .iter()
            .map(|(k, v)| (k.to_vec(), v.to_vec()))
            .collect();

        Ok(entries)
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

impl BackendWriter for LmdbWriter {
    fn commit(&mut self, ops: Vec<WriteOp>) -> Result<(), CommitError> {
        let mut txn = self
            .env
            .begin_rw_txn()
            .map_err(|e| CommitError::WriteFailed(format!("begin rw txn: {e}")))?;

        for op in ops {
            match op {
                WriteOp::Put {
                    namespace,
                    key,
                    value,
                } => {
                    let db = self.resolve_db(namespace)?;
                    txn.put(db, &key, &value, WriteFlags::empty())
                        .map_err(|e| CommitError::WriteFailed(format!("put: {e}")))?;
                }
                WriteOp::Delete { namespace, key } => {
                    let db = self.resolve_db(namespace)?;
                    match txn.del(db, &key, None) {
                        Ok(()) | Err(lmdb::Error::NotFound) => {}
                        Err(e) => {
                            return Err(CommitError::WriteFailed(format!("delete: {e}")));
                        }
                    }
                }
            }
        }

        txn.commit()
            .map_err(|e| CommitError::WriteFailed(format!("commit: {e}")))?;

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
        let backend =
            LmdbBackend::open(test_config(tmp.path(), vec![Namespace::new("known")])).expect("open");

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
            let val = reader
                .get(ns, b"durable")
                .expect("get")
                .expect("persisted");
            assert_eq!(val, b"yes");
        }
    }
}
