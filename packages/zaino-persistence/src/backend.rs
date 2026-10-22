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
//! own key ordering. Each is declared at construction by a [`NamespaceSpec`]:
//! a **sequence** (records by position, append-only) or a **map** (fixed-width
//! keys and values, each key inserted once, optionally removed once). Every read
//! and write targets a declared namespace, and every commit obeys the rules
//! [`check_commit`](crate::check_commit) enforces.

use core::num::{NonZeroU32, NonZeroUsize};
use core::ops::Bound;

use zaino_primitives::types::BlockRef;

use crate::error::{CommitError, FlushError, FootprintError, OpenError, ReadError, VerifyError};
use crate::verify::{NamespaceFootprint, Verification};

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

/// Bytes per record, key or value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Width {
    /// Every item is exactly this many bytes.
    Fixed(NonZeroU32),
    /// Items of any length.
    Variable,
}

/// The shape of a namespace's data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamespaceKind {
    /// Records by position: each `Put` key is the [`sequence_key`] of the next
    /// position; records are never deleted.
    Sequence {
        /// Record width.
        record: Width,
    },
    /// Fixed-width keys to fixed-width values; each key inserted at most once.
    Map {
        /// Key width.
        key: NonZeroU32,
        /// Value width.
        value: NonZeroU32,
        /// Keys removable, each at most once and never re-inserted.
        deletes: bool,
    },
}

/// A namespace and the shape of its data, declared at construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NamespaceSpec {
    /// The namespace.
    pub namespace: Namespace,
    /// Its shape.
    pub kind: NamespaceKind,
}

impl NamespaceSpec {
    /// A sequence of `record`-width records.
    pub const fn sequence(namespace: Namespace, record: Width) -> Self {
        Self {
            namespace,
            kind: NamespaceKind::Sequence { record },
        }
    }

    /// A map of `key`-byte keys to `value`-byte values, removable when `deletes`.
    pub const fn map(
        namespace: Namespace,
        key: NonZeroU32,
        value: NonZeroU32,
        deletes: bool,
    ) -> Self {
        Self {
            namespace,
            kind: NamespaceKind::Map {
                key,
                value,
                deletes,
            },
        }
    }
}

/// Bytes in a [`sequence_key`].
pub const SEQUENCE_KEY_LEN: usize = 8;

/// Key of a sequence record: its position, big-endian (key order = position order).
pub fn sequence_key(position: u64) -> RawKey {
    position.to_be_bytes().to_vec()
}

/// Position named by a sequence key; `None` = not a [`sequence_key`].
pub fn sequence_position(key: &[u8]) -> Option<u64> {
    <[u8; SEQUENCE_KEY_LEN]>::try_from(key)
        .ok()
        .map(u64::from_be_bytes)
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
///
/// - Sequence: `Put` appends at the key's position, which must be the next one;
///   `Delete` is refused.
/// - Map: `Put` inserts a key that is absent; `Delete` removes a present key of
///   a namespace declared with `deletes`. A removed key must not be re-inserted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOp {
    /// Insert a key-value pair (append, for a sequence).
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

    /// Obtain a read handle: a snapshot of every commit so far. May be called
    /// concurrently.
    fn reader(&self) -> Result<Self::Reader, OpenError>;

    /// Obtain a write handle.
    fn writer(&self) -> Result<Self::Writer, OpenError>;

    /// Make every commit durable (a backend may buffer commits until then).
    fn flush(&self) -> Result<(), FlushError>;

    /// Check that the stored bytes are exactly the bytes last made durable.
    fn verify(&self) -> Result<Verification, VerifyError>;

    /// Bytes each declared namespace holds: one entry per namespace, in
    /// declaration order.
    fn footprint(&self) -> Result<Vec<NamespaceFootprint>, FootprintError>;
}

/// Write handle. The engine sends batches of [`WriteOp`]s through this.
pub trait BackendWriter: Send {
    /// Commit a batch of write operations atomically, as of block `tip`.
    ///
    /// - Visible to readers opened once this returns; durable after
    ///   [`Backend::flush`] or the backend's own buffer limit.
    /// - `tip` must be above the last committed tip.
    /// - A batch breaking any rule of [`check_commit`](crate::check_commit)
    ///   fails with that rule's [`CommitError`] and applies nothing.
    fn commit(&mut self, tip: BlockRef, ops: Vec<WriteOp>) -> Result<(), CommitError>;
}

/// Read handle. Used for state loading and query serving.
///
/// A snapshot: every read through one handle, [`tip`](Self::tip) included,
/// sees the state as of [`Backend::reader`]; later commits need a new handle.
///
/// Every read of an undeclared namespace fails with
/// [`ReadError::NamespaceNotFound`]; a declared, empty namespace reads as empty.
pub trait BackendReader: Send {
    /// The snapshot's tip; `None` = nothing committed.
    fn tip(&self) -> Result<Option<BlockRef>, ReadError>;

    /// Read a single key from the given namespace.
    ///
    /// A key not of the namespace's key width is [`ReadError::WidthMismatch`].
    fn get(&self, namespace: Namespace, key: &[u8]) -> Result<Option<RawValue>, ReadError>;

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
    /// - Every chunk reads the reader's snapshot, even after the handle is
    ///   dropped. No key is returned twice.
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

/// Whole-namespace reads for maintenance, tests and tooling: **never** a client-serving path.
///
/// [`scan`](Self::scan) loads an entire namespace into memory at once, so its cost grows with
/// the namespace, not the request; many callers at once can exhaust memory. Serve clients with
/// [`BackendReader::scan_range`] (lazy, budget-bounded) instead.
pub trait MaintenanceReader: BackendReader {
    /// Every entry of `namespace`, in key order, from the reader's snapshot.
    fn scan(&self, namespace: Namespace) -> Result<Vec<(RawKey, RawValue)>, ReadError>;
}
