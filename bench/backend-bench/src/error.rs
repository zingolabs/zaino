//! Why a benchmark could not be set up or run.

use zaino_persistence::{CommitError, FlushError, OpenError, ReadError};

/// A benchmark failed. Every variant keeps its cause typed: a benchmark that
/// silently measured the wrong thing is worse than one that stops.
#[derive(Debug, thiserror::Error)]
pub enum BenchError {
    /// The backend could not be opened.
    #[error("opening the backend failed")]
    Open(#[from] OpenError),
    /// A batch could not be committed.
    #[error("committing a batch failed")]
    Commit(#[from] CommitError),
    /// A read failed.
    #[error("reading failed")]
    Read(#[from] ReadError),
    /// A flush failed.
    #[error("flushing failed")]
    Flush(#[from] FlushError),
    /// The temporary directory for an on-disk backend could not be made.
    #[error("creating the benchmark directory failed")]
    Directory(#[source] std::io::Error),
    /// A generated record could not be built from its domain value.
    #[error("generating a record failed: {0}")]
    Generate(String),
    /// A read-back value did not equal what was written — the correctness check
    /// a fast backend must still pass.
    #[error("{namespace}: read-back value differs from what was written")]
    Mismatch {
        /// The namespace whose value differed.
        namespace: String,
    },
    /// A read-back key was absent entirely.
    #[error("{namespace}: key written in this run is absent on read-back")]
    Absent {
        /// The namespace whose key was missing.
        namespace: String,
    },
}
