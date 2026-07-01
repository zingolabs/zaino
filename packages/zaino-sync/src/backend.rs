//! Backend trait — the storage layer the engine commits to and reads from.

use crate::traits::WriteOp;

/// Errors from backend operations.
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    /// A write/commit/flush operation failed.
    #[error("backend write error: {0}")]
    Write(String),
    /// A read operation failed.
    #[error("backend read error: {0}")]
    Read(String),
}

/// Whether the backend supports per-index writers or forces a shared writer.
///
/// This affects how the engine parallelises the commit step:
/// - `SharedWriter`: all indexes' write ops go through a single serialised writer.
/// - `PerIndexWriter`: each index gets its own writer; commits parallelise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriterTopology {
    /// All indexes share a single writer. Writes are serialised.
    SharedWriter,
    /// Each index has its own writer. Writes parallelise.
    PerIndexWriter,
}

/// The storage backend the engine commits to and reads from.
///
/// Generic — no blockchain or LMDB knowledge. Concrete implementations
/// (e.g. LMDB, in-memory for tests) live outside this crate.
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

    /// Whether this backend supports per-index writers.
    fn topology(&self) -> WriterTopology;
}

/// Write handle. The engine sends batches of [`WriteOp`]s through this.
pub trait BackendWriter: Send {
    /// Commit a batch of write operations atomically.
    fn commit(&mut self, ops: Vec<WriteOp>) -> Result<(), BackendError>;
}

/// Read handle. Used by [`DepsReader`](crate::traits::DepsReader) and
/// the engine's progress tracking.
pub trait BackendReader: Send {
    /// Read a single key from the given index.
    fn get(&self, index: crate::primitives::IndexId, key: &[u8]) -> Result<Option<Vec<u8>>, BackendError>;
}
