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
//! [`CumulativeBridge<I>`] handles all SelfCumulative composition types.
//! It threads a running [`PriorState`](crate::traits::ExtractCumulative::PriorState)
//! through sequential extractions and snapshots the accumulated state
//! at batch end for persistence.
//!
//! CrossIndex bridges are not yet implemented — they need backend reader
//! access that the pipeline interface doesn't yet provide.
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

use crate::backend::BackendReader;
use crate::descriptor::{Append, BlockLocal, Descriptor, Fold, Monoidal, SelfCumulative};
use crate::encode::{Decode, Encode};
use crate::pipeline::{IndexPipeline, PipelineError};
use crate::traits::{
    ExtractCumulative, ExtractLocal, IndexDef, MergeAppend, MergeFold, MergeMonoidal,
    ProvideContext, Schema, WriteOp,
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

impl sealed::Sealed for (SelfCumulative, Monoidal) {}
impl sealed::Sealed for (SelfCumulative, Fold) {}

impl<I, Ctx> BridgeDispatch<I, Ctx> for (BlockLocal, Append)
where
    I: ExtractLocal
        + MergeAppend
        + Schema<<AppendStrategy as MergeStrategy<I>>::MergedState>
        + IndexDef<Scope = BlockLocal, Composition = Append>,
    Ctx: ProvideContext<I::BlockContext> + Send + Sync + 'static,
{
    fn dispatch() -> Box<dyn IndexPipeline<Ctx>> {
        Box::new(LocalBridge::<I, AppendStrategy>::new())
    }
}

impl<I, Ctx> BridgeDispatch<I, Ctx> for (BlockLocal, Monoidal)
where
    I: ExtractLocal
        + MergeMonoidal
        + Schema<<MonoidalStrategy as MergeStrategy<I>>::MergedState>
        + IndexDef<Scope = BlockLocal, Composition = Monoidal>,
    Ctx: ProvideContext<I::BlockContext> + Send + Sync + 'static,
{
    fn dispatch() -> Box<dyn IndexPipeline<Ctx>> {
        Box::new(LocalBridge::<I, MonoidalStrategy>::new())
    }
}

impl<I, Ctx> BridgeDispatch<I, Ctx> for (BlockLocal, Fold)
where
    I: ExtractLocal
        + MergeFold
        + Schema<<FoldStrategy as MergeStrategy<I>>::MergedState>
        + IndexDef<Scope = BlockLocal, Composition = Fold>,
    Ctx: ProvideContext<I::BlockContext> + Send + Sync + 'static,
{
    fn dispatch() -> Box<dyn IndexPipeline<Ctx>> {
        Box::new(LocalBridge::<I, FoldStrategy>::new())
    }
}

impl<I, Ctx> BridgeDispatch<I, Ctx> for (SelfCumulative, Monoidal)
where
    I: ExtractCumulative<PriorState = <MonoidalStrategy as MergeStrategy<I>>::MergedState>
        + MergeMonoidal
        + Schema<<MonoidalStrategy as MergeStrategy<I>>::MergedState>
        + IndexDef<Scope = SelfCumulative, Composition = Monoidal>,
    <MonoidalStrategy as MergeStrategy<I>>::MergedState: Clone,
    Ctx: ProvideContext<I::BlockContext> + Send + Sync + 'static,
{
    fn dispatch() -> Box<dyn IndexPipeline<Ctx>> {
        Box::new(CumulativeBridge::<I, MonoidalStrategy>::new())
    }
}

impl<I, Ctx> BridgeDispatch<I, Ctx> for (SelfCumulative, Fold)
where
    I: ExtractCumulative<PriorState = <FoldStrategy as MergeStrategy<I>>::MergedState>
        + MergeFold
        + Schema<<FoldStrategy as MergeStrategy<I>>::MergedState>
        + IndexDef<Scope = SelfCumulative, Composition = Fold>,
    <FoldStrategy as MergeStrategy<I>>::MergedState: Clone,
    Ctx: ProvideContext<I::BlockContext> + Send + Sync + 'static,
{
    fn dispatch() -> Box<dyn IndexPipeline<Ctx>> {
        Box::new(CumulativeBridge::<I, FoldStrategy>::new())
    }
}

// ===========================================================================
// MergeStrategy — composition-specific logic
// ===========================================================================

/// Composition-specific merge logic. Pure domain — no schema, no encoding.
///
/// Abstracts the difference between Append, Monoidal, and Fold so that
/// [`LocalBridge`] and [`CumulativeBridge`] can each be a single generic
/// struct. Two primitive methods — [`initial_state`](Self::initial_state)
/// and [`accumulate_one`](Self::accumulate_one) — define the algebra.
/// [`merge_deltas`](Self::merge_deltas) is provided from them.
///
/// Schema and encoding are handled separately in the bridge's `persist`
/// method via [`Schema`] + [`Encode`].
pub(crate) trait MergeStrategy<I: IndexDef>: Send + Sync + 'static {
    /// The domain-typed result of merging a batch of deltas.
    type MergedState: Send + Sync;

    /// The identity/initial state before any deltas.
    fn initial_state() -> Self::MergedState;

    /// Fold one delta into the running state.
    fn accumulate_one(state: &mut Self::MergedState, delta: I::Delta);

    /// Combine a batch of deltas into a merged domain result.
    fn merge_deltas(deltas: Vec<I::Delta>) -> Self::MergedState {
        let mut state = Self::initial_state();
        for delta in deltas {
            Self::accumulate_one(&mut state, delta);
        }
        state
    }
}

/// Strategy marker for Append composition.
struct AppendStrategy;

impl<I> MergeStrategy<I> for AppendStrategy
where
    I: MergeAppend,
{
    type MergedState = Vec<I::Delta>;

    fn initial_state() -> Self::MergedState {
        Vec::new()
    }

    fn accumulate_one(state: &mut Self::MergedState, delta: I::Delta) {
        state.push(delta);
    }
}

/// Strategy marker for Monoidal composition.
struct MonoidalStrategy;

impl<I> MergeStrategy<I> for MonoidalStrategy
where
    I: MergeMonoidal,
{
    type MergedState = I::Accumulator;

    fn initial_state() -> Self::MergedState {
        I::identity()
    }

    fn accumulate_one(state: &mut Self::MergedState, delta: I::Delta) {
        let prev = std::mem::replace(state, I::identity());
        *state = I::combine(prev, I::lift(delta));
    }
}

/// Strategy marker for Fold composition.
struct FoldStrategy;

impl<I> MergeStrategy<I> for FoldStrategy
where
    I: MergeFold,
{
    type MergedState = I::FoldState;

    fn initial_state() -> Self::MergedState {
        I::initial_state()
    }

    fn accumulate_one(state: &mut Self::MergedState, delta: I::Delta) {
        I::fold(state, delta);
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
    I: ExtractLocal + Schema<S::MergedState>,
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

        let ops = I::into_entries(state)
            .into_iter()
            .map(|(key, value)| WriteOp::Put {
                index: I::NAME,
                key: key.encode(),
                value: value.encode(),
            })
            .collect();

        Ok(ops)
    }
}

// ===========================================================================
// CumulativeBridge — single struct for all SelfCumulative compositions
// ===========================================================================

/// Stateful bridge for all SelfCumulative indexes.
///
/// Unlike [`LocalBridge`], extraction is sequential within each index:
/// the bridge maintains a `running_state` that threads through blocks.
/// Different SelfCumulative indexes still extract in parallel with each
/// other — the scheduler guarantees at most one pending extraction per
/// index.
///
/// **State threading:**
/// - Starts at the merge strategy's
///   [`initial_state`](MergeStrategy::initial_state).
/// - After each extraction, the delta is folded into the running state
///   via [`accumulate_one`](MergeStrategy::accumulate_one). No separate
///   delta buffer — the running state IS the accumulated merge result.
/// - At batch end, the running state is snapshotted for persistence.
/// - The running state carries across batch boundaries — no reset.
///
/// **Persistence:**
/// The merge result (running state snapshot) is mapped to entries via
/// [`Schema`]. For (S, M) indexes where `PriorState = Accumulator`,
/// this persists the cumulative accumulator.
pub(crate) struct CumulativeBridge<I: IndexDef, S: MergeStrategy<I>> {
    descriptor: Descriptor,
    running_state: Mutex<S::MergedState>,
    merged: Mutex<Option<S::MergedState>>,
    _phantom: PhantomData<(I, S)>,
}

impl<I: IndexDef, S: MergeStrategy<I>> CumulativeBridge<I, S> {
    fn new() -> Self
    where
        S::MergedState: Clone,
    {
        Self {
            descriptor: I::descriptor(),
            running_state: Mutex::new(S::initial_state()),
            merged: Mutex::new(None),
            _phantom: PhantomData,
        }
    }
}

impl<Ctx, I, S> IndexPipeline<Ctx> for CumulativeBridge<I, S>
where
    I: ExtractCumulative<PriorState = S::MergedState> + Schema<S::MergedState>,
    S: MergeStrategy<I>,
    S::MergedState: Clone,
    Ctx: ProvideContext<I::BlockContext> + Send + Sync + 'static,
{
    fn descriptor(&self) -> &Descriptor {
        &self.descriptor
    }

    fn load_state(&self, reader: &dyn BackendReader) -> Result<(), PipelineError> {
        let raw_entries = reader
            .scan(I::NAME)
            .map_err(|e| PipelineError::Persist(e.to_string()))?;

        if raw_entries.is_empty() {
            return Ok(());
        }

        let entries: Vec<_> = raw_entries
            .into_iter()
            .map(|(k, v)| {
                let key = <I as Schema<S::MergedState>>::Key::decode(&k)
                    .map_err(|e| PipelineError::Persist(e.to_string()))?;
                let value = <I as Schema<S::MergedState>>::Value::decode(&v)
                    .map_err(|e| PipelineError::Persist(e.to_string()))?;
                Ok((key, value))
            })
            .collect::<Result<_, PipelineError>>()?;

        let state = I::from_entries(entries);
        *self.running_state.lock().expect("running state mutex poisoned") = state;
        Ok(())
    }

    fn extract_one(&self, ctx: &Ctx) -> Result<(), PipelineError> {
        let mut running = self.running_state.lock().expect("running state mutex poisoned");
        let delta = I::extract(&ctx.context(), &running)?;
        S::accumulate_one(&mut running, delta);
        Ok(())
    }

    fn merge(&self) -> Result<(), PipelineError> {
        let snapshot = self
            .running_state
            .lock()
            .expect("running state mutex poisoned")
            .clone();
        *self.merged.lock().expect("merged mutex poisoned") = Some(snapshot);
        Ok(())
    }

    fn persist(&self) -> Result<Vec<WriteOp>, PipelineError> {
        let state = self
            .merged
            .lock()
            .expect("merged mutex poisoned")
            .take()
            .ok_or_else(|| PipelineError::Persist("no merged state to persist".into()))?;

        let ops = I::into_entries(state)
            .into_iter()
            .map(|(key, value)| WriteOp::Put {
                index: I::NAME,
                key: key.encode(),
                value: value.encode(),
            })
            .collect();

        Ok(ops)
    }
}

