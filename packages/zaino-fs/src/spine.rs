//! The finalised-state spine — the always-present core.
//!
//! The irreducible finalised block store: the compact-block spine, its intrinsic
//! height/hash/treestate derivations, and the ingest lifecycle. *Every*
//! deployment has this. The heavier reverse indexes (tx-location, spend,
//! address) are addon traits in [`crate::indexes`], selected per deployment.

use std::future::Future;

use zaino_core::{BlockHash, CompactBlock, Height, PreIndexCompactBlock, Treestate};

use crate::error::{BuildError, FreezeError, HeightReadError, LookupError};

/// A block handed over the freeze boundary (NFS → FS): now final, to be indexed.
/// Carries enough to store the compact block and extract the aux indexes.
pub type FrozenBlock = PreIndexCompactBlock;

/// The finalised-state spine: the block store + intrinsic derivations + ingest.
/// Everything it answers is at or below [`FinalisedSpine::watermark`] and
/// immutable — so no reorg machinery. Each method carries the error type
/// appropriate to *its* failure modes.
pub trait FinalisedSpine: Send + Sync {
    /// The finalised tip. Reads are valid for heights `<= watermark`. Infallible.
    fn watermark(&self) -> Height;

    // --- height-keyed reads (can be above the watermark) ---
    fn compact_block(
        &self,
        height: Height,
    ) -> impl Future<Output = Result<Option<CompactBlock>, HeightReadError>> + Send;
    fn treestate(
        &self,
        height: Height,
    ) -> impl Future<Output = Result<Treestate, HeightReadError>> + Send;

    // --- hash lookup (miss is Ok, only the backend can fail) ---
    fn height_of(
        &self,
        hash: BlockHash,
    ) -> impl Future<Output = Result<Option<Height>, LookupError>> + Send;

    // --- ingest ---
    /// Bulk-build the finalised state up to `target` (boot catch-up), pulling
    /// from `source` (a `zaino-source`-shaped validator port — bounded in the
    /// impl). Where the sync engine's parallel pipeline runs.
    fn bulk_build_to<S: Send + Sync>(
        &self,
        target: Height,
        source: &S,
    ) -> impl Future<Output = Result<(), BuildError>> + Send;
    /// Extend by one finalised block (steady-state freeze).
    fn freeze(&self, block: FrozenBlock) -> impl Future<Output = Result<(), FreezeError>> + Send;
}
