//! Bridge implementations connecting typed traits to [`IndexPipeline`].
//!
//! # What this module does
//!
//! The typed trait hierarchy ([`ExtractLocal`], [`MergeAppend`], etc.)
//! gives compile-time safety at the index definition site. The engine
//! needs runtime dispatch via [`IndexPipeline<Ctx>`]. Bridges are the
//! glue: each bridge struct wraps a concrete index type (via
//! `PhantomData`) and implements `IndexPipeline<Ctx>` by calling the
//! type-level extract and merge traits internally. The `Delta` type
//! never leaves the bridge.
//!
//! One bridge per (scope × composition) cell. Currently implemented:
//! - [`LocalAppendBridge`] — BlockLocal × Append
//! - [`LocalMonoidalBridge`] — BlockLocal × Monoidal
//! - [`LocalFoldBridge`] — BlockLocal × Fold
//!
//! SelfCumulative and CrossIndex bridges are not yet implemented — they
//! need backend reader access that the pipeline interface doesn't yet
//! provide.
//!
//! # MVP limitation: sequential extraction within each bridge
//!
//! Every `process_batch` impl currently loops sequentially over
//! `blocks`. For `BlockLocal` indexes, extraction *could* run in
//! parallel across blocks — the type system already proves there are no
//! inter-block dependencies (that is the whole point of the `BlockLocal`
//! scope marker). But because `process_batch` collapses extract + merge
//! into one call, the engine has no way to schedule individual
//! extractions onto a shared thread pool or interleave work across
//! indexes.
//!
//! This is intentional for the MVP: it validates that the type algebra
//! composes end-to-end without solving the intermediate type problem.
//!
//! # True north: split `extract_one` + `merge_batch`
//!
//! The intended production design splits the pipeline interface so the
//! engine controls extraction parallelism:
//!
//! ```text
//! fn extract_one(&self, ctx: &Ctx, ...) -> Result<DeltaToken, ...>;
//! fn merge_batch(&self, tokens: Vec<DeltaToken>) -> Result<Vec<WriteOp>, ...>;
//! ```
//!
//! `DeltaToken` would be an opaque handle (e.g., an index into a
//! bridge-internal `Vec<Delta>`) — no `dyn Any`, no downcasting. The
//! bridge owns the typed delta storage; the engine holds and routes
//! opaque tokens. See [`crate::pipeline`] module docs for details.
//!
//! [`IndexPipeline`]: crate::pipeline::IndexPipeline
//! [`ExtractLocal`]: crate::traits::ExtractLocal
//! [`MergeAppend`]: crate::traits::MergeAppend

use std::marker::PhantomData;

use crate::descriptor::{Append, BlockLocal, Descriptor, Fold, Monoidal};
use crate::pipeline::{IndexPipeline, PipelineError};
use crate::traits::{
    DepsReader, ExtractLocal, IndexDef, MergeAppend, MergeFold, MergeMonoidal, ProvideContext,
    WriteOp,
};

// ===========================================================================
// BridgeDispatch — sealed trait mapping (Scope, Composition) → bridge fn
// ===========================================================================

mod sealed {
    pub trait Sealed {}
}

/// Maps a (Scope, Composition) marker pair to the correct bridge constructor.
///
/// Sealed — only implemented in this module for the marker pairs defined
/// in [`crate::descriptor`]. The blanket [`IntoIndexPipeline`] impl in
/// [`crate::pipeline`] delegates to this trait, so index authors never
/// need to write `IntoIndexPipeline` by hand.
///
/// `I` is the index type, `Ctx` is the set-wide block context. The bridge
/// requires `Ctx: ProvideContext<I::BlockContext>` to project the set-wide
/// context down to what the index needs.
///
/// [`IntoIndexPipeline`]: crate::pipeline::IntoIndexPipeline
pub trait BridgeDispatch<I: IndexDef, Ctx>: sealed::Sealed {
    /// Produce the boxed pipeline for index `I` over set-wide context `Ctx`.
    fn dispatch() -> Box<dyn IndexPipeline<Ctx>>;
}

// Seal the marker pairs.
impl sealed::Sealed for (BlockLocal, Append) {}
impl sealed::Sealed for (BlockLocal, Monoidal) {}
impl sealed::Sealed for (BlockLocal, Fold) {}

impl<I, Ctx> BridgeDispatch<I, Ctx> for (BlockLocal, Append)
where
    I: ExtractLocal + MergeAppend + IndexDef<Scope = BlockLocal, Composition = Append>,
    Ctx: ProvideContext<I::BlockContext> + Send + Sync + 'static,
{
    fn dispatch() -> Box<dyn IndexPipeline<Ctx>> {
        local_append::<I, Ctx>()
    }
}

impl<I, Ctx> BridgeDispatch<I, Ctx> for (BlockLocal, Monoidal)
where
    I: ExtractLocal + MergeMonoidal + IndexDef<Scope = BlockLocal, Composition = Monoidal>,
    Ctx: ProvideContext<I::BlockContext> + Send + Sync + 'static,
{
    fn dispatch() -> Box<dyn IndexPipeline<Ctx>> {
        local_monoidal::<I, Ctx>()
    }
}

impl<I, Ctx> BridgeDispatch<I, Ctx> for (BlockLocal, Fold)
where
    I: ExtractLocal + MergeFold + IndexDef<Scope = BlockLocal, Composition = Fold>,
    Ctx: ProvideContext<I::BlockContext> + Send + Sync + 'static,
{
    fn dispatch() -> Box<dyn IndexPipeline<Ctx>> {
        local_fold::<I, Ctx>()
    }
}

// ===========================================================================
// BlockLocal × Append
// ===========================================================================

/// Bridge for indexes that are block-local with disjoint-key (append) merge.
///
/// **Parallelism profile (true north):**
/// - Extraction: fully parallel across blocks (no inter-block deps).
/// - Merge: trivial collect — no combine step needed. Each delta's
///   write ops can be emitted independently.
///
/// **MVP:** extraction runs sequentially over blocks. No parallelism
/// because `process_batch` owns the full loop. See module docs.
pub struct LocalAppendBridge<I: IndexDef> {
    descriptor: Descriptor,
    _index: PhantomData<I>,
}

impl<I: IndexDef> LocalAppendBridge<I> {
    fn new() -> Self {
        Self {
            descriptor: I::descriptor(),
            _index: PhantomData,
        }
    }
}

impl<Ctx, I> IndexPipeline<Ctx> for LocalAppendBridge<I>
where
    I: ExtractLocal + MergeAppend,
    Ctx: ProvideContext<I::BlockContext> + Send + Sync + 'static,
{
    fn descriptor(&self) -> &Descriptor {
        &self.descriptor
    }

    fn process_batch(
        &self,
        blocks: &[Ctx],
        _deps: Option<&DepsReader>,
    ) -> Result<Vec<WriteOp>, PipelineError> {
        let mut all_ops = Vec::new();
        for ctx in blocks {
            let delta = I::extract(ctx.context())?;
            all_ops.extend(I::to_write_ops(delta));
        }
        Ok(all_ops)
    }
}

/// Register a BlockLocal × Append index into the pipeline.
pub fn local_append<I, Ctx>() -> Box<dyn IndexPipeline<Ctx>>
where
    I: ExtractLocal + MergeAppend,
    Ctx: ProvideContext<I::BlockContext> + Send + Sync + 'static,
{
    Box::new(LocalAppendBridge::<I>::new())
}

// ===========================================================================
// BlockLocal × Monoidal
// ===========================================================================

/// Bridge for indexes that are block-local with monoidal (associative +
/// commutative) merge.
///
/// **Parallelism profile (true north):**
/// - Extraction: fully parallel across blocks (no inter-block deps).
/// - Merge: parallel reduce tree — `combine` is associative +
///   commutative, so deltas can be reduced in any order.
///
/// **MVP:** both extraction and merge run sequentially. See module docs.
pub struct LocalMonoidalBridge<I: IndexDef> {
    descriptor: Descriptor,
    _index: PhantomData<I>,
}

impl<I: IndexDef> LocalMonoidalBridge<I> {
    fn new() -> Self {
        Self {
            descriptor: I::descriptor(),
            _index: PhantomData,
        }
    }
}

impl<Ctx, I> IndexPipeline<Ctx> for LocalMonoidalBridge<I>
where
    I: ExtractLocal + MergeMonoidal,
    Ctx: ProvideContext<I::BlockContext> + Send + Sync + 'static,
{
    fn descriptor(&self) -> &Descriptor {
        &self.descriptor
    }

    fn process_batch(
        &self,
        blocks: &[Ctx],
        _deps: Option<&DepsReader>,
    ) -> Result<Vec<WriteOp>, PipelineError> {
        let mut acc = I::identity();
        for ctx in blocks {
            let delta = I::extract(ctx.context())?;
            acc = I::combine(acc, I::lift(delta));
        }
        Ok(I::to_write_ops(acc))
    }
}

/// Register a BlockLocal × Monoidal index into the pipeline.
pub fn local_monoidal<I, Ctx>() -> Box<dyn IndexPipeline<Ctx>>
where
    I: ExtractLocal + MergeMonoidal,
    Ctx: ProvideContext<I::BlockContext> + Send + Sync + 'static,
{
    Box::new(LocalMonoidalBridge::<I>::new())
}

// ===========================================================================
// BlockLocal × Fold
// ===========================================================================

/// Bridge for indexes that are block-local with order-dependent (fold) merge.
///
/// **Parallelism profile (true north):**
/// - Extraction: fully parallel across blocks (no inter-block deps).
/// - Merge: strictly sequential — `fold` must apply deltas in chain
///   order. This is inherent to the Fold composition type, not an MVP
///   limitation.
///
/// **MVP:** extraction also runs sequentially. See module docs.
pub struct LocalFoldBridge<I: IndexDef> {
    descriptor: Descriptor,
    _index: PhantomData<I>,
}

impl<I: IndexDef> LocalFoldBridge<I> {
    fn new() -> Self {
        Self {
            descriptor: I::descriptor(),
            _index: PhantomData,
        }
    }
}

impl<Ctx, I> IndexPipeline<Ctx> for LocalFoldBridge<I>
where
    I: ExtractLocal + MergeFold,
    Ctx: ProvideContext<I::BlockContext> + Send + Sync + 'static,
{
    fn descriptor(&self) -> &Descriptor {
        &self.descriptor
    }

    fn process_batch(
        &self,
        blocks: &[Ctx],
        _deps: Option<&DepsReader>,
    ) -> Result<Vec<WriteOp>, PipelineError> {
        let mut state = I::initial_state();
        for ctx in blocks {
            let delta = I::extract(ctx.context())?;
            I::fold(&mut state, delta);
        }
        Ok(I::to_write_ops(state))
    }
}

/// Register a BlockLocal × Fold index into the pipeline.
pub fn local_fold<I, Ctx>() -> Box<dyn IndexPipeline<Ctx>>
where
    I: ExtractLocal + MergeFold,
    Ctx: ProvideContext<I::BlockContext> + Send + Sync + 'static,
{
    Box::new(LocalFoldBridge::<I>::new())
}
