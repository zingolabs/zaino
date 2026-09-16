//! Backend trait — the storage interface for indexed data.
//!
//! The sync engine writes through this, the serving layer reads from it.
//! Both depend on the same abstraction; neither knows the concrete
//! storage technology.
//!
//! # Namespaces
//!
//! The backend organises data into **namespaces** — independent
//! keyspaces, each with its own key ordering. In LMDB these map to
//! named databases; in RocksDB to column families; in the in-memory
//! backend to separate `HashMap`s.
//!
//! Namespaces are declared at construction time. All writes and reads
//! target a declared namespace.
//!
//! > **Note:** the upfront declaration requirement exists because LMDB
//! > (our primary backend) needs the full set of named databases at
//! > environment open time. A backend backed by RocksDB or a HashMap
//! > could support dynamic namespace creation. If we add such a backend,
//! > consider relaxing this to an `ensure_namespace` method.

use crate::error::{CommitError, FlushError, OpenError, ReadError};

/// A namespace identifier — names an independent keyspace within the backend.
///
/// Not an `IndexId`: a namespace is a storage concept (where bytes live),
/// an index is a domain concept (what the bytes mean). The engine maps
/// index IDs and its own metadata to separate namespaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Namespace(&'static str);

impl Namespace {
    /// Create a namespace from a static string.
    pub const fn new(name: &'static str) -> Self {
        Self(name)
    }

    /// The string value.
    pub const fn as_str(&self) -> &'static str {
        self.0
    }
}

impl core::fmt::Display for Namespace {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.0)
    }
}

impl From<zaino_primitives::types::IndexId> for Namespace {
    fn from(id: zaino_primitives::types::IndexId) -> Self {
        Self(id.as_str())
    }
}

/// Encoded key bytes, as produced by the index's schema encoding.
///
/// Opaque to the backend — it stores and retrieves these without
/// interpretation. Key ordering is lexicographic on the raw bytes.
pub type RawKey = Vec<u8>;

/// Encoded value bytes, as produced by the index's schema encoding.
///
/// Opaque to the backend — it stores and retrieves these without
/// interpretation.
pub type RawValue = Vec<u8>;

/// A write operation: put or delete a key-value pair in a namespace.
#[derive(Debug)]
pub enum WriteOp {
    /// Insert or overwrite a key-value pair.
    Put {
        /// Target namespace.
        namespace: Namespace,
        /// Encoded key.
        key: RawKey,
        /// Encoded value.
        value: RawValue,
    },
    /// Remove a key.
    Delete {
        /// Target namespace.
        namespace: Namespace,
        /// Encoded key.
        key: RawKey,
    },
}

/// The storage backend.
///
/// Generic — no blockchain or storage-technology knowledge.
/// Concurrency is the backend's concern: if the underlying store
/// only supports one writer at a time, the backend locks internally.
pub trait Backend: Send + Sync {
    /// Reader handle type.
    type Reader: BackendReader;
    /// Writer handle type.
    type Writer: BackendWriter;

    /// Obtain a read handle. May be called concurrently.
    fn reader(&self) -> Result<Self::Reader, OpenError>;

    /// Obtain a write handle.
    fn writer(&self) -> Result<Self::Writer, OpenError>;

    /// Force durability of all committed data.
    fn flush(&self) -> Result<(), FlushError>;
}

/// Write handle. The engine sends batches of [`WriteOp`]s through this.
pub trait BackendWriter: Send {
    /// Commit a batch of write operations atomically.
    fn commit(&mut self, ops: Vec<WriteOp>) -> Result<(), CommitError>;
}

/// Read handle. Used for state loading and query serving.
pub trait BackendReader: Send {
    /// Read a single key from the given namespace.
    fn get(&self, namespace: Namespace, key: &[u8]) -> Result<Option<RawValue>, ReadError>;

    /// Return all entries for a namespace as raw key-value byte pairs.
    fn scan(&self, namespace: Namespace) -> Result<Vec<(RawKey, RawValue)>, ReadError>;
}
