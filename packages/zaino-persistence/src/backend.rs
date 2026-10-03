//! The storage interface for indexed data.
//!
//! [`Backend`] opens [`BackendReader`] and [`BackendWriter`] handles; reads and
//! writes address a [`Namespace`] with raw byte keys and values ([`RawKey`],
//! [`RawValue`]) and mutate through batched [`WriteOp`]s. See the
//! [crate overview](crate) for the seam these traits form.
//!
//! # Namespaces
//!
//! Data is organised into **namespaces**: independent keyspaces, each with its
//! own key ordering (named databases in LMDB, column families in RocksDB,
//! separate maps in the in-memory backend). Namespaces are declared when the
//! backend is constructed, because LMDB — the primary backend — needs the full
//! set of named databases at environment-open time. Every read and write targets
//! a declared namespace.

use core::num::NonZeroUsize;
use core::ops::Bound;

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

/// Order in which a range scan visits keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanDirection {
    /// Ascending key order.
    Forward,
    /// Descending key order.
    Reverse,
}

/// The keys a range scan visits, and how it chunks them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanRange {
    /// Lower key bound.
    pub lower: Bound<RawKey>,
    /// Upper key bound.
    pub upper: Bound<RawKey>,
    /// Visit order.
    pub direction: ScanDirection,
    /// Raw bytes (key + value) of kept entries per chunk.
    pub chunk_bytes: NonZeroUsize,
}

/// Range-scan filter that keeps every entry as owned raw bytes.
pub fn raw_entry(key: &[u8], value: &[u8]) -> Option<(RawKey, RawValue)> {
    Some((key.to_vec(), value.to_vec()))
}

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
    ///
    /// A batch naming an undeclared namespace fails with
    /// [`CommitError::NamespaceNotFound`] and applies nothing.
    fn commit(&mut self, ops: Vec<WriteOp>) -> Result<(), CommitError>;
}

/// Read handle. Used for state loading and query serving.
///
/// Every read of an undeclared namespace fails with
/// [`ReadError::NamespaceNotFound`]; a declared, empty namespace reads as empty.
pub trait BackendReader: Send {
    /// Read a single key from the given namespace.
    fn get(&self, namespace: Namespace, key: &[u8]) -> Result<Option<RawValue>, ReadError>;

    /// Return all entries for a namespace as raw key-value byte pairs.
    fn scan(&self, namespace: Namespace) -> Result<Vec<(RawKey, RawValue)>, ReadError>;

    /// Entries of `namespace` within `range`, mapped through `filter`, as
    /// lazily read chunks in `range.direction` order.
    ///
    /// - Each `next()` reads one chunk; `filter` runs on the borrowed bytes,
    ///   is called at most once per entry, and entries it maps to `None` are
    ///   skipped.
    /// - A chunk ends before an entry whose raw size would take it past
    ///   `range.chunk_bytes`, but always holds at least one entry.
    /// - Chunks are never empty; `None` means the range is exhausted. An
    ///   empty or inverted range yields nothing.
    /// - An undeclared namespace yields [`ReadError::NamespaceNotFound`] first.
    /// - Each chunk reads one consistent state; successive chunks may observe
    ///   later commits. No key is returned twice.
    /// - After an `Err` the iterator is finished.
    fn scan_range<T, F>(
        &self,
        namespace: Namespace,
        range: ScanRange,
        filter: F,
    ) -> impl Iterator<Item = Result<Vec<T>, ReadError>> + Send + 'static + use<Self, T, F>
    where
        T: Send + 'static,
        F: FnMut(&[u8], &[u8]) -> Option<T> + Send + 'static;
}
