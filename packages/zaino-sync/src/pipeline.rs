//! Partially-erased index pipeline.
//!
//! The typed trait hierarchy in [`crate::traits`] enforces correct
//! implementations at the index definition site. The engine, however,
//! needs to hold heterogeneous indexes in a single collection and
//! dispatch uniformly.
//!
//! [`IndexPipeline<Ctx>`] is the trait-object-safe interface the engine
//! works with. Each index's `Delta`, `Accumulator`, and `FoldState` stay
//! *inside* its bridge implementation — they never cross the trait
//! boundary.
//!
//! Bridge types in [`crate::bridge`] connect the typed traits to this
//! interface.
//!
//! # Three-phase pipeline
//!
//! The interface exposes three methods that the engine calls in sequence,
//! driven by the [`Scheduler`](crate::scheduler::Scheduler):
//!
//! 1. **`extract_one`** — called per block. Computes a delta from the
//!    block context and stores it in the bridge's internal buffer.
//!    The engine may call this in parallel for `BlockLocal` indexes.
//!
//! 2. **`merge`** — called once per batch after all extractions complete.
//!    Combines stored deltas according to the composition type (collect
//!    for Append, reduce for Monoidal, sequential fold for Fold).
//!
//! 3. **`persist`** — called once per batch after merge. Drains the
//!    merged state into `WriteOp`s for the backend. This is the
//!    serialization boundary — domain types cross into persistence
//!    types here. (Currently the merge traits own this step; it will
//!    move to a dedicated persistence layer.)

use crate::bridge::BridgeDispatch;
use crate::descriptor::Descriptor;
use crate::traits::{ExtractError, IndexDef, ProvideContext, WriteOp};

/// Errors during pipeline operations.
#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    /// Extraction failed.
    #[error(transparent)]
    Extract(#[from] ExtractError),
    /// Merge failed.
    #[error("merge failed: {0}")]
    Merge(String),
    /// Persist failed.
    #[error("persist failed: {0}")]
    Persist(String),
}

/// The trait-object-safe interface the engine dispatches through.
///
/// `Ctx` is the provisioner's block context type — shared across all
/// indexes, kept concrete (not erased). The engine is generic over
/// `Ctx` once, not per-index.
///
/// The bridge implementations hold internal state (delta buffer, merge
/// accumulator) behind interior mutability (`Mutex`), so all methods
/// take `&self` for trait-object safety.
pub trait IndexPipeline<Ctx>: Send + Sync {
    /// The declarative descriptor.
    fn descriptor(&self) -> &Descriptor;

    /// Extract a delta from one block's context.
    ///
    /// Stores the delta in the bridge's internal buffer. The engine
    /// calls this once per block, potentially in parallel for
    /// `BlockLocal` indexes. The scheduler tracks completion counts
    /// and transitions to merge when the batch is full.
    fn extract_one(&self, ctx: &Ctx) -> Result<(), PipelineError>;

    /// Merge all accumulated deltas for the current batch.
    ///
    /// Consumes the delta buffer and combines deltas according to the
    /// composition type:
    /// - **Append**: collect (no-op — deltas are already independent).
    /// - **Monoidal**: parallel-reducible fold via `combine`.
    /// - **Fold**: strictly sequential application in chain order.
    ///
    /// The merged state is held internally until [`persist`](Self::persist).
    fn merge(&self) -> Result<(), PipelineError>;

    /// Drain the merged state into write operations.
    ///
    /// Converts domain-typed merge results into `WriteOp`s for the
    /// backend. This is the serialization boundary. Clears the
    /// internal state, readying the bridge for the next batch.
    fn persist(&self) -> Result<Vec<WriteOp>, PipelineError>;

    /// Convenience: run all three phases sequentially on a batch.
    ///
    /// Exists for backward compatibility with the batch-loop engine.
    /// The streaming scheduler calls the three methods individually.
    fn process_batch(
        &self,
        blocks: &[Ctx],
        _deps: Option<&crate::traits::DepsReader>,
    ) -> Result<Vec<WriteOp>, PipelineError> {
        for ctx in blocks {
            self.extract_one(ctx)?;
        }
        self.merge()?;
        self.persist()
    }
}

/// Capstone trait: a fully-defined index that can produce its own pipeline.
///
/// `Ctx` is the set-wide block context. The index's [`BlockContext`] may
/// differ — the bridge inserts a [`ProvideContext`] projection. Index
/// authors never implement this trait by hand; the blanket impl below
/// derives it from the (Scope, Composition) marker pair.
///
/// [`BlockContext`]: IndexDef::BlockContext
/// [`ProvideContext`]: crate::traits::ProvideContext
pub trait IntoIndexPipeline<Ctx: Send + Sync + 'static>: IndexDef {
    /// Produce a boxed pipeline for this index over set-wide context `Ctx`.
    fn into_pipeline() -> Box<dyn IndexPipeline<Ctx>>;
}

/// Blanket impl: any index whose (Scope, Composition) pair has a
/// [`BridgeDispatch`] impl gets `IntoIndexPipeline` for free, for any
/// `Ctx` that can [`ProvideContext`] the index's [`BlockContext`].
impl<I, Ctx> IntoIndexPipeline<Ctx> for I
where
    I: IndexDef,
    Ctx: ProvideContext<I::BlockContext> + Send + Sync + 'static,
    (I::Scope, I::Composition): BridgeDispatch<I, Ctx>,
{
    fn into_pipeline() -> Box<dyn IndexPipeline<Ctx>> {
        <(I::Scope, I::Composition) as BridgeDispatch<I, Ctx>>::dispatch()
    }
}
