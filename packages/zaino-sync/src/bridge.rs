//! Bridge implementations connecting typed traits to [`IndexPipeline`].
//!
//! # What this module does
//!
//! The typed trait hierarchy ([`ExtractLocal`], [`MergeAppend`], etc.)
//! gives compile-time safety at the index definition site. The engine
//! needs runtime dispatch via [`IndexPipeline<Ctx>`]. Bridges are the
//! glue: each bridge struct holds internal state (delta buffer, merge
//! accumulator) and implements `IndexPipeline<Ctx>` by calling the
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
//! # Three-phase pipeline
//!
//! Each bridge implements `extract_one` / `merge` / `persist`:
//!
//! - **`extract_one`**: computes a delta from one block context, pushes
//!   it into an internal `Mutex<Vec<Delta>>` buffer.
//! - **`merge`**: drains the delta buffer and combines deltas per the
//!   composition type. Stores the result in the merge state slot.
//! - **`persist`**: converts the merged domain state into `WriteOp`s
//!   and clears the state for the next batch.
//!
//! Interior mutability (`Mutex`) is used throughout because
//! `IndexPipeline` methods take `&self` for trait-object safety.
//!
//! [`IndexPipeline`]: crate::pipeline::IndexPipeline
//! [`ExtractLocal`]: crate::traits::ExtractLocal
//! [`MergeAppend`]: crate::traits::MergeAppend

use std::marker::PhantomData;
use std::sync::Mutex;

use crate::descriptor::{Append, BlockLocal, Descriptor, Fold, Monoidal};
use crate::pipeline::{IndexPipeline, PipelineError};
use crate::traits::{
    ExtractLocal, IndexDef, MergeAppend, MergeFold, MergeMonoidal, ProvideContext, WriteOp,
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

/// Stateful bridge for BlockLocal × Append indexes.
///
/// **Parallelism profile:**
/// - Extraction: fully parallel across blocks (no inter-block deps).
/// - Merge: trivial collect — deltas are independent, no combine step.
///
/// Internal state:
/// - `deltas`: buffer of per-block deltas, pushed by `extract_one`.
/// - `merged_ops`: WriteOps produced by `merge`, drained by `persist`.
pub struct LocalAppendBridge<I: IndexDef> {
    descriptor: Descriptor,
    deltas: Mutex<Vec<I::Delta>>,
    merged_ops: Mutex<Vec<WriteOp>>,
    _index: PhantomData<I>,
}

impl<I: IndexDef> LocalAppendBridge<I> {
    fn new() -> Self {
        Self {
            descriptor: I::descriptor(),
            deltas: Mutex::new(Vec::new()),
            merged_ops: Mutex::new(Vec::new()),
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

    fn extract_one(&self, ctx: &Ctx) -> Result<(), PipelineError> {
        let delta = I::extract(&ctx.context())?;
        self.deltas.lock().expect("delta mutex poisoned").push(delta);
        Ok(())
    }

    fn merge(&self) -> Result<(), PipelineError> {
        let deltas: Vec<I::Delta> = self
            .deltas
            .lock()
            .expect("delta mutex poisoned")
            .drain(..)
            .collect();

        let mut ops = Vec::new();
        for delta in deltas {
            ops.extend(I::to_write_ops(delta));
        }

        *self.merged_ops.lock().expect("ops mutex poisoned") = ops;
        Ok(())
    }

    fn persist(&self) -> Result<Vec<WriteOp>, PipelineError> {
        let ops = self
            .merged_ops
            .lock()
            .expect("ops mutex poisoned")
            .drain(..)
            .collect();
        Ok(ops)
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

/// Stateful bridge for BlockLocal × Monoidal indexes.
///
/// **Parallelism profile:**
/// - Extraction: fully parallel across blocks (no inter-block deps).
/// - Merge: parallel reduce tree — `combine` is associative +
///   commutative, so deltas can be reduced in any order.
///
/// Internal state:
/// - `deltas`: buffer of per-block deltas, pushed by `extract_one`.
/// - `merged_ops`: WriteOps produced by `merge`, drained by `persist`.
pub struct LocalMonoidalBridge<I: IndexDef + MergeMonoidal> {
    descriptor: Descriptor,
    deltas: Mutex<Vec<I::Delta>>,
    merged_ops: Mutex<Vec<WriteOp>>,
    _index: PhantomData<I>,
}

impl<I: IndexDef + MergeMonoidal> LocalMonoidalBridge<I> {
    fn new() -> Self {
        Self {
            descriptor: I::descriptor(),
            deltas: Mutex::new(Vec::new()),
            merged_ops: Mutex::new(Vec::new()),
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

    fn extract_one(&self, ctx: &Ctx) -> Result<(), PipelineError> {
        let delta = I::extract(&ctx.context())?;
        self.deltas.lock().expect("delta mutex poisoned").push(delta);
        Ok(())
    }

    fn merge(&self) -> Result<(), PipelineError> {
        let deltas: Vec<I::Delta> = self
            .deltas
            .lock()
            .expect("delta mutex poisoned")
            .drain(..)
            .collect();

        let mut acc = I::identity();
        for delta in deltas {
            acc = I::combine(acc, I::lift(delta));
        }

        *self.merged_ops.lock().expect("ops mutex poisoned") = I::to_write_ops(acc);
        Ok(())
    }

    fn persist(&self) -> Result<Vec<WriteOp>, PipelineError> {
        let ops = self
            .merged_ops
            .lock()
            .expect("ops mutex poisoned")
            .drain(..)
            .collect();
        Ok(ops)
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

/// Stateful bridge for BlockLocal × Fold indexes.
///
/// **Parallelism profile:**
/// - Extraction: fully parallel across blocks (no inter-block deps).
/// - Merge: strictly sequential — `fold` must apply deltas in chain
///   order. This is inherent to the Fold composition type.
///
/// Internal state:
/// - `deltas`: buffer of per-block deltas, pushed by `extract_one`.
///   **Must be consumed in insertion order** during merge.
/// - `merged_ops`: WriteOps produced by `merge`, drained by `persist`.
pub struct LocalFoldBridge<I: IndexDef + MergeFold> {
    descriptor: Descriptor,
    deltas: Mutex<Vec<I::Delta>>,
    merged_ops: Mutex<Vec<WriteOp>>,
    _index: PhantomData<I>,
}

impl<I: IndexDef + MergeFold> LocalFoldBridge<I> {
    fn new() -> Self {
        Self {
            descriptor: I::descriptor(),
            deltas: Mutex::new(Vec::new()),
            merged_ops: Mutex::new(Vec::new()),
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

    fn extract_one(&self, ctx: &Ctx) -> Result<(), PipelineError> {
        let delta = I::extract(&ctx.context())?;
        self.deltas.lock().expect("delta mutex poisoned").push(delta);
        Ok(())
    }

    fn merge(&self) -> Result<(), PipelineError> {
        let deltas: Vec<I::Delta> = self
            .deltas
            .lock()
            .expect("delta mutex poisoned")
            .drain(..)
            .collect();

        let mut state = I::initial_state();
        for delta in deltas {
            I::fold(&mut state, delta);
        }

        *self.merged_ops.lock().expect("ops mutex poisoned") = I::to_write_ops(state);
        Ok(())
    }

    fn persist(&self) -> Result<Vec<WriteOp>, PipelineError> {
        let ops = self
            .merged_ops
            .lock()
            .expect("ops mutex poisoned")
            .drain(..)
            .collect();
        Ok(ops)
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
