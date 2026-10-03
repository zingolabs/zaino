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

use core::ops::ControlFlow;

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

/// A per-entry callback for [`BackendReader::scan_range`].
///
/// Called with borrowed key and value bytes for each entry in the range;
/// returns [`ControlFlow::Break`] to stop the scan early or
/// [`ControlFlow::Continue`] to go on. A trait-object alias (not a generic
/// parameter) keeps [`BackendReader`] object-safe, so it can be used as
/// `&dyn BackendReader`.
pub type RangeVisitor<'a> = dyn FnMut(&[u8], &[u8]) -> ControlFlow<()> + 'a;

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
    ///
    /// Materialises the whole namespace onto the heap. Use only for a bounded,
    /// one-shot full load (state rebuild); a per-key or per-range query must use
    /// [`get`](Self::get) or [`scan_range`](Self::scan_range), which do not copy
    /// the entire keyspace.
    fn scan(&self, namespace: Namespace) -> Result<Vec<(RawKey, RawValue)>, ReadError>;

    /// Visit the entries of `namespace` whose key is in `[start, end_exclusive)`,
    /// in ascending key order, without materialising the range.
    ///
    /// Keys order lexicographically on their raw bytes (the same order
    /// [`scan`](Self::scan) returns), so a caller whose key layout is prefixed by
    /// the dimension it queries — an address id, say — seeks a contiguous slice
    /// rather than scanning the namespace and filtering in memory.
    ///
    /// `visit` is called once per entry with borrowed key and value bytes; it
    /// returns [`ControlFlow::Break`] to stop early (the backend reads no
    /// further) or [`ControlFlow::Continue`] to go on. The borrow lasts only for
    /// the call, so a caller that keeps an entry copies it out. An empty range,
    /// a `start` past every key, and a range matching nothing all visit nothing
    /// and are not errors.
    fn scan_range(
        &self,
        namespace: Namespace,
        start: &[u8],
        end_exclusive: &[u8],
        visit: &mut RangeVisitor<'_>,
    ) -> Result<(), ReadError>;

    /// The lexicographically smallest key in `namespace`, or `None` when it holds
    /// nothing.
    ///
    /// An emptiness probe that reads one key rather than the whole namespace:
    /// `first_key(ns)?.is_some()` answers "is there any data here" in O(1) seeks,
    /// where [`scan`](Self::scan) would copy every entry out only to test the
    /// length.
    fn first_key(&self, namespace: Namespace) -> Result<Option<RawKey>, ReadError>;
}
