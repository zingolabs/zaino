//! Bridge implementations connecting typed traits to [`IndexPipeline`].
//!
//! # What this module does
//!
//! The typed trait hierarchy ([`ExtractLocal`], [`MergeAppend`], etc.)
//! gives compile-time safety at the index definition site. The engine
//! needs runtime dispatch via [`IndexPipeline<Ctx>`]. Bridges are the
//! glue: each bridge struct holds internal state (delta buffer, merge
//! result) and implements `IndexPipeline<Ctx>` by calling the type-level
//! extract and merge traits internally. The `Delta` type never leaves
//! the bridge.
//!
//! # Single bridge struct
//!
//! [`LocalBridge<I>`] handles all three BlockLocal composition types.
//! The composition-specific logic (how deltas are combined, how the
//! merged result becomes WriteOps) is dispatched through the
//! [`MergeStrategy`] trait, which each merge trait (`MergeAppend`,
//! `MergeMonoidal`, `MergeFold`) satisfies via blanket impls.
//!
//! SelfCumulative and CrossIndex bridges are not yet implemented — they
//! need backend reader access that the pipeline interface doesn't yet
//! provide.
//!
//! # Three-phase pipeline
//!
//! - **`extract_one`**: computes a delta, pushes into internal buffer.
//! - **`merge`**: drains deltas, combines per composition type, stores
//!   domain-typed result. No serialization.
//! - **`persist`**: converts domain result to `WriteOp`s. This is the
//!   serialization boundary.
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
/// [`IntoIndexPipeline`]: crate::pipeline::IntoIndexPipeline
pub trait BridgeDispatch<I: IndexDef, Ctx>: sealed::Sealed {
    /// Produce the boxed pipeline for index `I` over set-wide context `Ctx`.
    fn dispatch() -> Box<dyn IndexPipeline<Ctx>>;
}

impl sealed::Sealed for (BlockLocal, Append) {}
impl sealed::Sealed for (BlockLocal, Monoidal) {}
impl sealed::Sealed for (BlockLocal, Fold) {}

impl<I, Ctx> BridgeDispatch<I, Ctx> for (BlockLocal, Append)
where
    I: ExtractLocal + MergeAppend + IndexDef<Scope = BlockLocal, Composition = Append>,
    Ctx: ProvideContext<I::BlockContext> + Send + Sync + 'static,
{
    fn dispatch() -> Box<dyn IndexPipeline<Ctx>> {
        Box::new(LocalBridge::<I, AppendStrategy>::new())
    }
}

impl<I, Ctx> BridgeDispatch<I, Ctx> for (BlockLocal, Monoidal)
where
    I: ExtractLocal + MergeMonoidal + IndexDef<Scope = BlockLocal, Composition = Monoidal>,
    Ctx: ProvideContext<I::BlockContext> + Send + Sync + 'static,
{
    fn dispatch() -> Box<dyn IndexPipeline<Ctx>> {
        Box::new(LocalBridge::<I, MonoidalStrategy>::new())
    }
}

impl<I, Ctx> BridgeDispatch<I, Ctx> for (BlockLocal, Fold)
where
    I: ExtractLocal + MergeFold + IndexDef<Scope = BlockLocal, Composition = Fold>,
    Ctx: ProvideContext<I::BlockContext> + Send + Sync + 'static,
{
    fn dispatch() -> Box<dyn IndexPipeline<Ctx>> {
        Box::new(LocalBridge::<I, FoldStrategy>::new())
    }
}

// ===========================================================================
// MergeStrategy — composition-specific logic
// ===========================================================================

/// Composition-specific merge and persist logic.
///
/// Abstracts the difference between Append, Monoidal, and Fold so that
/// [`LocalBridge`] can be a single generic struct. Each strategy defines
/// how to combine a `Vec<Delta>` into a merged result, and how to
/// convert that result into `WriteOp`s.
pub(crate) trait MergeStrategy<I: IndexDef>: Send + Sync + 'static {
    /// The domain-typed result of merging a batch of deltas.
    type MergedState: Send + Sync;

    /// Combine a batch of deltas into a merged domain result.
    fn merge_deltas(deltas: Vec<I::Delta>) -> Self::MergedState;

    /// Convert the merged domain result into write operations.
    /// This is the serialization boundary.
    fn to_write_ops(state: Self::MergedState) -> Vec<WriteOp>;
}

/// Strategy marker for Append composition.
struct AppendStrategy;

impl<I> MergeStrategy<I> for AppendStrategy
where
    I: MergeAppend,
{
    // For append, the merged state is the collected deltas themselves —
    // each delta independently produces WriteOps.
    type MergedState = Vec<I::Delta>;

    fn merge_deltas(deltas: Vec<I::Delta>) -> Self::MergedState {
        deltas
    }

    fn to_write_ops(state: Self::MergedState) -> Vec<WriteOp> {
        let mut ops = Vec::new();
        for delta in state {
            ops.extend(I::to_write_ops(delta));
        }
        ops
    }
}

/// Strategy marker for Monoidal composition.
struct MonoidalStrategy;

impl<I> MergeStrategy<I> for MonoidalStrategy
where
    I: MergeMonoidal,
{
    type MergedState = I::Accumulator;

    fn merge_deltas(deltas: Vec<I::Delta>) -> Self::MergedState {
        let mut acc = I::identity();
        for delta in deltas {
            acc = I::combine(acc, I::lift(delta));
        }
        acc
    }

    fn to_write_ops(state: Self::MergedState) -> Vec<WriteOp> {
        I::to_write_ops(state)
    }
}

/// Strategy marker for Fold composition.
struct FoldStrategy;

impl<I> MergeStrategy<I> for FoldStrategy
where
    I: MergeFold,
{
    type MergedState = I::FoldState;

    fn merge_deltas(deltas: Vec<I::Delta>) -> Self::MergedState {
        let mut state = I::initial_state();
        for delta in deltas {
            I::fold(&mut state, delta);
        }
        state
    }

    fn to_write_ops(state: Self::MergedState) -> Vec<WriteOp> {
        I::to_write_ops(state)
    }
}

// ===========================================================================
// LocalBridge — single struct for all BlockLocal compositions
// ===========================================================================

/// Stateful bridge for all BlockLocal indexes.
///
/// `I` is the index type, `S` is the [`MergeStrategy`] marker. The
/// bridge stores deltas in a buffer and merged state in an `Option`.
///
/// **Parallelism profile:**
/// - Extraction: fully parallel across blocks (BlockLocal proves no
///   inter-block deps).
/// - Merge: depends on strategy (trivial for Append, parallel-reducible
///   for Monoidal, sequential for Fold).
pub(crate) struct LocalBridge<I: IndexDef, S: MergeStrategy<I>> {
    descriptor: Descriptor,
    deltas: Mutex<Vec<I::Delta>>,
    merged: Mutex<Option<S::MergedState>>,
    _phantom: PhantomData<(I, S)>,
}

impl<I: IndexDef, S: MergeStrategy<I>> LocalBridge<I, S> {
    fn new() -> Self {
        Self {
            descriptor: I::descriptor(),
            deltas: Mutex::new(Vec::new()),
            merged: Mutex::new(None),
            _phantom: PhantomData,
        }
    }
}

impl<Ctx, I, S> IndexPipeline<Ctx> for LocalBridge<I, S>
where
    I: ExtractLocal,
    S: MergeStrategy<I>,
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

        let state = S::merge_deltas(deltas);
        *self.merged.lock().expect("merged mutex poisoned") = Some(state);
        Ok(())
    }

    fn persist(&self) -> Result<Vec<WriteOp>, PipelineError> {
        let state = self
            .merged
            .lock()
            .expect("merged mutex poisoned")
            .take()
            .ok_or_else(|| PipelineError::Persist("no merged state to persist".into()))?;
        Ok(S::to_write_ops(state))
    }
}

