//! Backend trait — the storage interface for indexed data.
//!
//! The sync engine writes through this, the serving layer reads from it.
//! Both depend on the same abstraction; neither knows the concrete
//! storage technology.

use zaino_primitives::types::IndexId;

use crate::error::BackendError;

/// A write operation: put or delete a key-value pair in a named index.
#[derive(Debug)]
pub enum WriteOp {
    /// Insert or overwrite a key-value pair.
    Put {
        /// Target index.
        index: IndexId,
        /// Serialised key.
        key: Vec<u8>,
        /// Serialised value.
        value: Vec<u8>,
    },
    /// Remove a key.
    Delete {
        /// Target index.
        index: IndexId,
        /// Serialised key.
        key: Vec<u8>,
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
    fn reader(&self) -> Result<Self::Reader, BackendError>;

    /// Obtain a write handle.
    fn writer(&self) -> Result<Self::Writer, BackendError>;

    /// Force durability of all committed data.
    fn flush(&self) -> Result<(), BackendError>;
}

/// Write handle. The engine sends batches of [`WriteOp`]s through this.
pub trait BackendWriter: Send {
    /// Commit a batch of write operations atomically.
    fn commit(&mut self, ops: Vec<WriteOp>) -> Result<(), BackendError>;
}

/// Read handle. Used for state loading and query serving.
pub trait BackendReader: Send {
    /// Read a single key from the given index.
    fn get(&self, index: IndexId, key: &[u8]) -> Result<Option<Vec<u8>>, BackendError>;

    /// Return all entries for an index as raw key-value byte pairs.
    fn scan(&self, index: IndexId) -> Result<Vec<(Vec<u8>, Vec<u8>)>, BackendError>;
}
