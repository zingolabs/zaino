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
//! boundary. The engine sees only `&[Ctx]` in, `Vec<WriteOp>` out.
//!
//! Bridge types in [`crate::bridge`] connect the typed traits to this
//! interface.
//!
//! # Current limitation: `process_batch` collapses extract and merge
//!
//! The current interface exposes a single `process_batch` method that
//! takes `&[Ctx]` and returns `Vec<WriteOp>`. This means the engine
//! cannot control extraction parallelism — it hands a whole batch to
//! each index and gets final results back. Extraction within the bridge
//! runs sequentially.
//!
//! This is a **conscious MVP decision** to validate that the type algebra
//! composes end-to-end without solving the intermediate type problem yet.
//!
//! # True north: split `extract_one` + `merge_batch`
//!
//! The intended design splits the interface into two methods so the
//! engine can schedule per-block extractions onto a shared thread pool
//! and control parallelism across indexes:
//!
//! ```text
//! fn extract_one(&self, ctx: &Ctx, deps: ...) -> Result<DeltaToken, ...>;
//! fn merge_batch(&self, tokens: Vec<DeltaToken>) -> Result<Vec<WriteOp>, ...>;
//! ```
//!
//! `DeltaToken` would be an opaque handle (e.g., an index into a
//! bridge-internal `Vec<Delta>`) that does not expose the concrete
//! `Delta` type across the trait boundary — no `dyn Any`, no
//! downcasting. The bridge owns the typed storage; the engine holds
//! and routes opaque tokens.
//!
//! This split is required to unlock:
//! - Per-block parallel extraction for `BlockLocal` indexes.
//! - Engine-controlled work-stealing across indexes sharing a thread pool.
//! - Streaming extraction (process blocks as the provisioner delivers
//!   them, rather than buffering a full batch upfront).

use crate::descriptor::Descriptor;
use crate::traits::{ExtractError, WriteOp};

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
/// Each call to [`process_batch`](Self::process_batch) encapsulates the
/// full extract → merge → to_write_ops pipeline for one batch of blocks.
/// The `Delta` type never crosses this boundary.
///
/// **MVP shape.** This will be split into `extract_one` + `merge_batch`
/// once the `DeltaToken` intermediate design is resolved. See module docs.
pub trait IndexPipeline<Ctx>: Send + Sync {
    /// The declarative descriptor.
    fn descriptor(&self) -> &Descriptor;

    /// Process a batch of blocks through the full pipeline.
    ///
    /// Internally performs:
    /// 1. Extraction: produce a delta per block (scope-specific inputs).
    /// 2. Merge: combine deltas according to composition type.
    /// 3. Conversion: turn merged result into write operations.
    ///
    /// `blocks` is in chain order. The engine does not inspect
    /// intermediates — it receives final `WriteOp`s ready for commit.
    fn process_batch(
        &self,
        blocks: &[Ctx],
        deps: Option<&crate::traits::DepsReader>,
    ) -> Result<Vec<WriteOp>, PipelineError>;
}
