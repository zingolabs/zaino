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

use crate::backend::{BackendReader, WriteOp};
use crate::bridge::BridgeDispatch;
use crate::descriptor::Descriptor;
use crate::primitives::BlockHeight;
use crate::traits::{IndexDef, ProvideContext};

/// Errors during pipeline operations.
#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    /// Extraction failed. The boxed source is the index's own typed error
    /// (`ExtractLocal::Error` etc.) — the one place the engine erases it, kept as
    /// the `source()` so an abort trail leads down to the concrete domain cause.
    #[error("index extraction failed")]
    Extract(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
    /// Merge failed.
    #[error("merge failed: {0}")]
    Merge(String),
    /// Reading committed state from the backend failed.
    #[error("backend read")]
    Read(#[from] crate::backend::ReadError),
    /// Loading and decoding persisted entries failed.
    #[error("loading persisted state")]
    Load(#[from] zaino_persistence_codec::LoadError),
    /// Decoding a persisted value failed.
    #[error("decoding persisted value")]
    Decode(#[from] zaino_persistence_codec::DecodeError),
    /// The namespace holds data stamped with a format this build cannot read.
    ///
    /// The persisted bytes must be discarded and the index rebuilt — a policy the
    /// caller owns; this layer only surfaces the skew.
    #[error(
        "incompatible on-disk format for index {index}: persisted bytes do not \
         match this build"
    )]
    IncompatibleFormat {
        /// The index whose stored format no longer matches the running code.
        index: &'static str,
    },
    /// `persist` was called before a batch was merged.
    #[error("persist called with no merged state for index {index}")]
    MissingMergedState {
        /// The index that had no merged state staged.
        index: &'static str,
    },
}

impl PipelineError {
    /// Erase an index's typed extraction error to the pipeline boundary,
    /// preserving it as the [`Extract`](PipelineError::Extract) source.
    pub(crate) fn extract<E: std::error::Error + Send + Sync + 'static>(source: E) -> Self {
        Self::Extract(Box::new(source))
    }
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

    /// Load persisted state from the backend on startup.
    ///
    /// Called once per pipeline before the first batch. `resume_from` is the
    /// committed watermark height (`None` on a fresh backend). The default
    /// implementation is a no-op — BlockLocal indexes have no state to resume.
    /// SelfCumulative bridges override this to reload their carry: the monoidal
    /// bridge rebuilds the collapsed accumulator from its entries, while the
    /// append-cumulative bridge point-reads the value at `resume_from` (an
    /// `O(1)` tip lookup).
    fn load_state(
        &self,
        _reader: &dyn BackendReader,
        _resume_from: Option<BlockHeight>,
    ) -> Result<(), PipelineError> {
        Ok(())
    }

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    #[derive(Debug, thiserror::Error)]
    #[error("boom: {0}")]
    struct Boom(u32);

    #[test]
    fn extract_error_preserves_the_source_chain() {
        // The pipeline boundary erases the index's error type, but the cause stays
        // reachable via source() — so an engine abort trail leads down to the
        // concrete domain error rather than a flattened string. (Downcast here
        // verifies the plumbing; production never recovers the type.)
        let err = PipelineError::extract(Boom(42));
        let source = err.source().expect("the boxed cause is the source");
        assert_eq!(source.to_string(), "boom: 42");
        assert!(source.downcast_ref::<Boom>().is_some());
    }
}
