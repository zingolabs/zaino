//! Progress tracking and crash recovery.
//!
//! The engine maintains a persistent watermark: the height of the last
//! fully-flushed batch. On restart, it resumes from the watermark.
//! Partially committed batches are discarded (the backend's atomic commit
//! guarantees no partial state).

use crate::primitives::BlockHeight;

/// Persistent sync progress.
#[derive(Debug, Clone, Copy)]
pub struct SyncProgress {
    /// The height of the last fully-flushed batch (inclusive).
    /// `None` if no batch has been flushed yet.
    pub watermark: Option<BlockHeight>,
}
