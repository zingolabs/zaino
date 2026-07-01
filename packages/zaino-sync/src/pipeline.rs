//! Partially-erased index pipeline.
//!
//! The typed trait hierarchy in [`crate::traits`] enforces correct
//! implementations at the index definition site. The engine, however,
//! needs to hold heterogeneous indexes in a single collection and
//! dispatch uniformly.
//!
//! The solution: erase only what must cross the trait-object boundary.
//! `Delta`, `Accumulator`, and `FoldState` stay *inside* the index —
//! they never leave its pipeline methods. The engine sees only:
//!
//! - `Ctx` in (the provisioner's block context — shared, concrete type)
//! - `Vec<WriteOp>` out
//!
//! No `dyn Any`, no downcasting, no runtime type mismatches.

use crate::descriptor::Descriptor;
use crate::traits::{DepsReader, ExtractError, WriteOp};

/// Per-block opaque contribution from one index's extraction.
///
/// The engine holds these between extract and merge. The concrete
/// content is only meaningful to the index that produced it — the
/// engine never inspects it.
///
/// Implemented as an index-specific closure over the delta, avoiding
/// `dyn Any`. The merge step calls back into the index to consume it.
pub struct BlockContribution {
    write_ops: Vec<WriteOp>,
}

impl BlockContribution {
    /// Create a contribution from pre-computed write ops (Append path).
    pub fn from_write_ops(ops: Vec<WriteOp>) -> Self {
        Self { write_ops: ops }
    }

    /// Consume into write operations.
    pub fn into_write_ops(self) -> Vec<WriteOp> {
        self.write_ops
    }
}

/// Errors during pipeline operations.
#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    /// Extraction failed.
    #[error(transparent)]
    Extract(#[from] ExtractError),
    /// Merge/write-ops conversion failed.
    #[error("merge failed: {0}")]
    Merge(String),
}

/// The trait-object-safe interface the engine dispatches through.
///
/// `Ctx` is the provisioner's block context type — shared across all
/// indexes, kept concrete (not erased). The engine is generic over
/// `Ctx` once, not per-index.
///
/// Each method on this trait encapsulates a full extract-or-merge step.
/// The `Delta` type never crosses this boundary — it lives and dies
/// inside the index's implementation of these methods.
pub trait IndexPipeline<Ctx>: Send + Sync {
    /// The declarative descriptor.
    fn descriptor(&self) -> &Descriptor;

    /// Extract one block's contribution.
    ///
    /// For `BlockLocal` indexes: `deps` is `None`.
    /// For `SelfCumulative` indexes: the index reads its own prior state
    ///   from the backend internally (the pipeline impl holds a reader).
    /// For `CrossIndex` indexes: `deps` provides committed state from
    ///   dependency indexes.
    ///
    /// Returns a [`BlockContribution`] — opaque to the engine, consumed
    /// by [`merge_batch`](Self::merge_batch).
    fn extract_block(
        &self,
        ctx: &Ctx,
        deps: Option<&DepsReader>,
    ) -> Result<BlockContribution, PipelineError>;

    /// Merge a batch of block contributions into final write operations.
    ///
    /// `contributions` is in chain order. The index applies its
    /// composition-specific merge internally:
    /// - `Append`: flatten (already done — contributions carry WriteOps).
    /// - `Monoidal`: parallel reduce using the declared monoid.
    /// - `Fold`: sequential application in chain order.
    ///
    /// The engine doesn't need to know which strategy is used — it just
    /// gets `Vec<WriteOp>` back.
    fn merge_batch(
        &self,
        contributions: Vec<BlockContribution>,
    ) -> Result<Vec<WriteOp>, PipelineError>;
}
