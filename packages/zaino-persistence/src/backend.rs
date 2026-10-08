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

/// How a namespace's keys order relative to the chain walk.
///
/// A **storage fact**, not a policy: it describes the bytes a namespace receives,
/// and the backend is free to ignore it. It is stated by each index's key codec
/// ([`EntryCodec::KEY_ORDER`](../zaino_persistence_codec/trait.EntryCodec.html))
/// and carried to the backend on the namespace list ([`NamespaceSpec`]). It
/// enables deferral of scattered writes; it never forces it.
///
/// Defined here, in the low KV crate, rather than in `zaino-persistence-codec`
/// (where the codec states it) because [`NamespaceSpec`] lives here and that
/// crate already depends on this one — the reverse dependency would cycle. The
/// codec crate re-exports it, so `zaino_persistence_codec::KeyOrder` names this
/// same type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyOrder {
    /// Keys for later blocks sort strictly after keys for earlier blocks (the
    /// key leads with the height, big-endian). Safe to write with a sorted
    /// append.
    WalkOrdered,
    /// Keys are not ordered by the chain walk (hash-led keys). The conservative
    /// claim: a namespace stated `Scattered` is never append-ordered, so a
    /// mis-statement costs only speed, never correctness.
    Scattered,
    /// Not an index namespace: engine bookkeeping (the watermark, format-version
    /// stamps, future manifests) the backend writes directly. Never deferred,
    /// never appended. No codec ever states this — it is attached to the
    /// reserved namespaces when the namespace list is assembled.
    Meta,
}

/// A namespace paired with its [`KeyOrder`] — one entry of the namespace list a
/// backend is opened with.
///
/// Replaces a bare [`Namespace`] in the list so the backend learns each
/// namespace's key order up front, uniformly, including the reserved meta
/// namespaces (tagged [`KeyOrder::Meta`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NamespaceSpec {
    /// The namespace to open.
    pub namespace: Namespace,
    /// How this namespace's keys order relative to the chain walk.
    pub key_order: KeyOrder,
}

impl NamespaceSpec {
    /// A spec for a reserved engine-bookkeeping namespace — tagged
    /// [`KeyOrder::Meta`].
    pub const fn meta(namespace: Namespace) -> Self {
        Self {
            namespace,
            key_order: KeyOrder::Meta,
        }
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

/// Policy handed to [`Backend::begin_bulk`]: whether, and later how, the backend
/// may defer [`Scattered`](KeyOrder::Scattered) namespaces during a bulk load.
///
/// A struct rather than a bare `bool` so the policy can grow — a deferral
/// threshold, a disk budget — without changing the method signature or breaking
/// callers. A backend that never defers ignores it entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BulkPolicy {
    /// Whether deferral is permitted at all. `false` reproduces the direct write
    /// path exactly: the backend must not defer, every commit is immediately
    /// visible, and [`is_complete`](BackendReader::is_complete) stays `true`.
    pub enabled: bool,
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

    /// Enter bulk mode: the backend MAY defer [`Scattered`](KeyOrder::Scattered)
    /// namespaces' writes to a cheaper append path while the bulk load runs.
    ///
    /// `commit` keeps its whole contract while deferred: when it returns, every
    /// op is durable and the watermark it carries is truthful. Deferral changes
    /// only *where* a scattered op becomes durable (a run log, not the tree), not
    /// *whether*. A deferred namespace reads as incomplete
    /// ([`is_complete`](BackendReader::is_complete) returns `false`) until
    /// [`finish_bulk`](Self::finish_bulk) completes it.
    ///
    /// The default is a no-op: a backend with no cheaper bulk path (the in-memory
    /// backend, an LSM backend) ignores bulk mode and writes directly, so
    /// `is_complete` stays `true` throughout.
    ///
    /// Bulk state is durable where the backend is durable: calling this again
    /// after a restart re-enters bulk mode on the existing deferral state rather
    /// than starting a fresh one.
    fn begin_bulk(&self, policy: BulkPolicy) -> Result<(), CommitError> {
        let _ = policy;
        Ok(())
    }

    /// Leave bulk mode: make every deferred namespace complete and readable.
    ///
    /// Resumable and idempotent: calling it again after a crash continues where
    /// it stopped, and calling it when nothing is deferred (including the default
    /// no-op) succeeds without effect. After it returns, every namespace's
    /// [`is_complete`](BackendReader::is_complete) is `true` and holds every
    /// committed entry.
    fn finish_bulk(&self) -> Result<(), CommitError> {
        Ok(())
    }

    /// Whether a previous run left an unfinished bulk load — some namespace is
    /// still deferred, because a crash interrupted the catch-up (between a
    /// deferred commit and its watermark) or [`finish_bulk`](Self::finish_bulk)
    /// itself (a partly-merged run log).
    ///
    /// A freshly opened durable backend answers this from its own persisted
    /// deferral state, so a resuming caller can re-enter bulk mode
    /// ([`begin_bulk`](Self::begin_bulk)) and complete the load regardless of how
    /// small the remaining gap is, rather than leaving the deferred namespaces
    /// unreadable. The default is `false`: a backend that never defers has
    /// nothing pending.
    fn bulk_pending(&self) -> Result<bool, ReadError> {
        Ok(false)
    }
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

    /// Return all entries for a namespace as raw key-value byte pairs, in
    /// ascending bytewise key order (the same order [`scan_range`](Self::scan_range)
    /// visits).
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

    /// Whether `namespace` holds every committed entry — `false` only while the
    /// backend is deferring this namespace's writes in bulk mode
    /// ([`Backend::begin_bulk`]).
    ///
    /// Always `true` for a backend that does not defer, `true` for every
    /// namespace outside bulk mode, and `true` again for every namespace once
    /// [`Backend::finish_bulk`] returns. The serving layer maps `false` to
    /// "not yet serviceable" rather than serving a partial namespace as if
    /// complete. The default is `true`, matching a backend that never defers.
    fn is_complete(&self, namespace: Namespace) -> Result<bool, ReadError> {
        let _ = namespace;
        Ok(true)
    }
}
