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

use rayon::prelude::*;

use crate::backend::{BackendReader, Namespace, WriteOp};
use crate::descriptor::{
    Append, BlockLocal, CrossIndex, Descriptor, Fold, Monoidal, OrderedMonoid, SelfCumulative,
    Sequential,
};
use crate::pipeline::{IndexPipeline, PipelineError};
use crate::primitives::{BlockHeight, BlockOffset};
use crate::traits::{
    CumulativeAppend, DepsReader, ExtractCross, ExtractCumulative, ExtractLocal, IndexDef,
    MergeAppend, MergeFold, MergeMonoidal, OrderedMonoidCarry, ProvideContext, Schema,
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

impl sealed::Sealed for (SelfCumulative<Sequential>, Append) {}
impl sealed::Sealed for (SelfCumulative<OrderedMonoid>, Append) {}
impl sealed::Sealed for (SelfCumulative<Sequential>, Monoidal) {}
impl sealed::Sealed for (SelfCumulative<Sequential>, Fold) {}

impl sealed::Sealed for (CrossIndex, Append) {}

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

impl<I, Ctx> BridgeDispatch<I, Ctx> for (SelfCumulative<Sequential>, Append)
where
    I: CumulativeAppend
        + Schema<Vec<<I as IndexDef>::Delta>>
        + IndexDef<Scope = SelfCumulative<Sequential>, Composition = Append>
        + zaino_persistence_codec::EntryCodec<Key = BlockHeight>,
    I::PriorState: Clone,
    Ctx: ProvideContext<I::BlockContext> + Send + Sync + 'static,
{
    fn dispatch() -> Box<dyn IndexPipeline<Ctx>> {
        Box::new(CumulativeAppendBridge::<I, Sequential>::new())
    }
}

impl<I, Ctx> BridgeDispatch<I, Ctx> for (SelfCumulative<OrderedMonoid>, Append)
where
    I: OrderedMonoidCarry
        + Schema<Vec<<I as IndexDef>::Delta>>
        + IndexDef<Scope = SelfCumulative<OrderedMonoid>, Composition = Append>
        + zaino_persistence_codec::EntryCodec<Key = BlockHeight>,
    I::PriorState: Clone,
    I::BlockContext: Send + Sync,
    Ctx: ProvideContext<I::BlockContext> + Send + Sync + 'static,
{
    fn dispatch() -> Box<dyn IndexPipeline<Ctx>> {
        Box::new(CumulativeAppendBridge::<I, OrderedMonoid>::new())
    }
}

impl<I, Ctx> BridgeDispatch<I, Ctx> for (SelfCumulative<Sequential>, Monoidal)
where
    I: ExtractCumulative<PriorState = <MonoidalStrategy as MergeStrategy<I>>::MergedState>
        + MergeMonoidal
        + Schema<<MonoidalStrategy as MergeStrategy<I>>::MergedState>
        + IndexDef<Scope = SelfCumulative<Sequential>, Composition = Monoidal>,
    <MonoidalStrategy as MergeStrategy<I>>::MergedState: Clone,
    Ctx: ProvideContext<I::BlockContext> + Send + Sync + 'static,
{
    fn dispatch() -> Box<dyn IndexPipeline<Ctx>> {
        Box::new(CumulativeBridge::<I, MonoidalStrategy>::new())
    }
}

impl<I, Ctx> BridgeDispatch<I, Ctx> for (SelfCumulative<Sequential>, Fold)
where
    I: ExtractCumulative<PriorState = <FoldStrategy as MergeStrategy<I>>::MergedState>
        + MergeFold
        + Schema<<FoldStrategy as MergeStrategy<I>>::MergedState>
        + IndexDef<Scope = SelfCumulative<Sequential>, Composition = Fold>,
    <FoldStrategy as MergeStrategy<I>>::MergedState: Clone,
    Ctx: ProvideContext<I::BlockContext> + Send + Sync + 'static,
{
    fn dispatch() -> Box<dyn IndexPipeline<Ctx>> {
        Box::new(CumulativeBridge::<I, FoldStrategy>::new())
    }
}

impl<I, Ctx> BridgeDispatch<I, Ctx> for (CrossIndex, Append)
where
    I: ExtractCross
        + MergeAppend
        + Schema<Vec<<I as IndexDef>::Delta>>
        + IndexDef<Scope = CrossIndex, Composition = Append>
        + zaino_persistence_codec::EntryCodec,
    Ctx: ProvideContext<I::BlockContext> + Send + Sync + 'static,
{
    fn dispatch() -> Box<dyn IndexPipeline<Ctx>> {
        Box::new(CrossBridge::<I>::new())
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
// Shared resume guard
// ===========================================================================

/// Stale-format guard shared by the cumulative bridges.
///
/// Returns `Ok(true)` when the namespace holds usable (fresh) data to resume
/// from, `Ok(false)` when it is empty/absent (a fresh run — nothing to resume),
/// and an error when it holds data stamped with an incompatible format: a
/// genuine skew this build must not decode. What to *do* about a rejected index
/// (discard and re-index) is a separate policy the caller owns; this layer only
/// detects and rejects.
fn resume_readable<I>(
    reader: &dyn BackendReader,
    namespace: Namespace,
) -> Result<bool, PipelineError>
where
    I: zaino_persistence_codec::EntryCodec,
{
    if zaino_persistence_codec::freshness::<I>(reader, namespace)?
        == zaino_persistence_codec::Freshness::Stale
    {
        let has_data = reader.first_key(namespace)?.is_some();
        if has_data {
            return Err(PipelineError::IncompatibleFormat {
                index: namespace.as_str(),
            });
        }
        return Ok(false);
    }
    Ok(true)
}

// ===========================================================================
// Shared persist step
// ===========================================================================

/// Turn a bridge's merged state into the index's [`WriteOp`]s.
///
/// The persist phase is identical for every bridge: take the merged
/// state out of its slot, map it to typed entries via [`Schema`], and
/// encode each entry. This is the serialization boundary.
fn persist_merged<I, M>(merged: &Mutex<Option<M>>) -> Result<Vec<WriteOp>, PipelineError>
where
    I: Schema<M>,
{
    let namespace: Namespace = I::NAME.into();
    let state = merged.lock().expect("merged mutex poisoned").take().ok_or(
        PipelineError::MissingMergedState {
            index: namespace.as_str(),
        },
    )?;

    // Stamp the namespace's format version first, then the entries — a later open
    // rejects and rebuilds if the recorded version no longer matches the code.
    let mut ops = vec![zaino_persistence_codec::version_stamp::<I>(namespace)];
    ops.extend(
        I::into_entries(state)
            .into_iter()
            .map(|(key, value)| zaino_persistence_codec::put::<I>(namespace, &key, &value)),
    );

    Ok(ops)
}

// ===========================================================================
// Shared merge step for block-parallel bridges
// ===========================================================================

/// Drain an offset-tagged delta buffer into chain order.
///
/// Parallel extraction buffers deltas in rayon completion order, but the merge
/// contract is chain order. Offsets are unique within a batch, so the sort is
/// total and `unstable` is safe. Shared by the block-parallel bridges
/// ([`LocalBridge`] and [`CrossBridge`]), whose extraction both fan out across a
/// batch's blocks; the cumulative bridges extract sequentially and have no such
/// buffer to reorder.
fn drain_reorder<D>(deltas: &Mutex<Vec<(BlockOffset, D)>>) -> Vec<D> {
    let mut tagged: Vec<(BlockOffset, D)> = deltas
        .lock()
        .expect("delta mutex poisoned")
        .drain(..)
        .collect();
    tagged.sort_unstable_by_key(|(offset, _)| *offset);
    tagged.into_iter().map(|(_, delta)| delta).collect()
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
///   inter-block deps), so deltas arrive in rayon completion order, not chain
///   order.
/// - Merge: each delta is tagged with its block [`BlockOffset`] at extraction
///   and the buffer is reordered to chain order before the strategy folds it.
///   Chain order is the merge contract for every composition — `Monoidal`'s
///   `combine` is associative but not commutative, and `Fold` is outright
///   order-dependent — so the reorder is unconditional rather than per-strategy.
pub(crate) struct LocalBridge<I: IndexDef, S: MergeStrategy<I>> {
    descriptor: Descriptor,
    deltas: Mutex<Vec<(BlockOffset, I::Delta)>>,
    merged: Mutex<Option<S::MergedState>>,
    _phantom: PhantomData<(I, S)>,
}

impl<I, S: MergeStrategy<I>> LocalBridge<I, S>
where
    I: IndexDef + zaino_persistence_codec::EntryCodec,
{
    fn new() -> Self {
        Self {
            descriptor: I::descriptor(<I as zaino_persistence_codec::EntryCodec>::KEY_ORDER),
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

    fn extract_one(&self, offset: BlockOffset, ctx: &Ctx) -> Result<(), PipelineError> {
        let delta = I::extract(&ctx.context()).map_err(PipelineError::extract)?;
        self.deltas
            .lock()
            .expect("delta mutex poisoned")
            .push((offset, delta));
        Ok(())
    }

    fn merge(&self) -> Result<(), PipelineError> {
        let deltas = drain_reorder(&self.deltas);
        let state = S::merge_deltas(deltas);
        *self.merged.lock().expect("merged mutex poisoned") = Some(state);
        Ok(())
    }

    fn persist(&self) -> Result<Vec<WriteOp>, PipelineError> {
        persist_merged::<I, S::MergedState>(&self.merged)
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

impl<I, S: MergeStrategy<I>> CumulativeBridge<I, S>
where
    I: IndexDef + zaino_persistence_codec::EntryCodec,
{
    fn new() -> Self
    where
        S::MergedState: Clone,
    {
        Self {
            descriptor: I::descriptor(<I as zaino_persistence_codec::EntryCodec>::KEY_ORDER),
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

    fn load_state(
        &self,
        reader: &dyn BackendReader,
        // Ignored: a collapsed accumulator is rebuilt from all its entries, not
        // point-read at the tip. Only the append-cumulative bridge uses the height.
        _resume_from: Option<BlockHeight>,
    ) -> Result<(), PipelineError> {
        let namespace: Namespace = I::NAME.into();

        if !resume_readable::<I>(reader, namespace)? {
            return Ok(());
        }

        let entries = zaino_persistence_codec::load::<I>(reader, namespace)?;
        if entries.is_empty() {
            return Ok(());
        }

        let state = I::from_entries(entries);
        *self
            .running_state
            .lock()
            .expect("running state mutex poisoned") = state;
        Ok(())
    }

    // Extraction is sequential in chain order (the scheduler emits one block at
    // a time for a SelfCumulative index), so the offset carries no information
    // this bridge needs.
    fn extract_one(&self, _offset: BlockOffset, ctx: &Ctx) -> Result<(), PipelineError> {
        let mut running = self
            .running_state
            .lock()
            .expect("running state mutex poisoned");
        let delta = I::extract(&ctx.context(), &running).map_err(PipelineError::extract)?;
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
        persist_merged::<I, S::MergedState>(&self.merged)
    }
}

// ===========================================================================
// CumulativeAppendBridge — the (SelfCumulative, Append) bridge
// ===========================================================================

/// The append-cumulative bridge's **execution strategy** — the only thing that
/// differs between the two carry algebras of `(SelfCumulative, Append)`.
///
/// `(SelfCumulative<Sequential>, Append)` and
/// `(SelfCumulative<OrderedMonoid>, Append)` share one bridge
/// ([`CumulativeAppendBridge`]): same output, same `key = height` append
/// persistence, same `O(1)` carry resume. They differ only in how a batch's
/// per-height deltas are produced — a serial fold versus a parallel
/// measure→lift→reduce→project scan. That difference is captured here and
/// selected at the type level by the scope's carry-algebra marker, so the engine
/// never detects it at runtime.
///
/// The strategy owns its per-batch [`Buffer`](Self::Buffer): the sequential
/// strategy accumulates finished deltas (it folds during extraction), the
/// ordered-monoid strategy buffers `(offset, owned context)` and does all the
/// work in [`merge`](Self::merge).
pub(crate) trait CumulativeExec<I: CumulativeAppend>: Send + Sync + 'static {
    /// The strategy's per-batch working buffer.
    type Buffer: Send + Sync;

    /// A fresh, empty buffer.
    fn new_buffer() -> Self::Buffer;

    /// Record one block. `ctx` is the owned per-block context (the identity
    /// projection clones once); `carry` is the running carry across batches.
    fn extract(
        buffer: &Mutex<Self::Buffer>,
        carry: &Mutex<I::PriorState>,
        offset: BlockOffset,
        ctx: I::BlockContext,
    ) -> Result<(), PipelineError>;

    /// Turn the batch's buffer into the per-height deltas and advance the carry
    /// to the last height's value.
    fn merge(
        buffer: &Mutex<Self::Buffer>,
        carry: &Mutex<I::PriorState>,
    ) -> Result<Vec<I::Delta>, PipelineError>;
}

/// Sequential strategy: today's path. Extraction folds the carry block by block
/// in chain order (the scheduler emits one block at a time), buffering finished
/// deltas; merge just hands them over.
impl<I: CumulativeAppend> CumulativeExec<I> for Sequential {
    type Buffer = Vec<I::Delta>;

    fn new_buffer() -> Self::Buffer {
        Vec::new()
    }

    fn extract(
        buffer: &Mutex<Self::Buffer>,
        carry: &Mutex<I::PriorState>,
        _offset: BlockOffset,
        ctx: I::BlockContext,
    ) -> Result<(), PipelineError> {
        let mut carry = carry.lock().expect("carry mutex poisoned");
        let delta = I::extract(&ctx, &carry).map_err(PipelineError::extract)?;
        *carry = I::carry(&delta);
        drop(carry);
        buffer.lock().expect("delta mutex poisoned").push(delta);
        Ok(())
    }

    fn merge(
        buffer: &Mutex<Self::Buffer>,
        _carry: &Mutex<I::PriorState>,
    ) -> Result<Vec<I::Delta>, PipelineError> {
        Ok(buffer
            .lock()
            .expect("delta mutex poisoned")
            .drain(..)
            .collect())
    }
}

/// Ordered-monoid strategy: build the whole batch in parallel. Extraction only
/// buffers `(offset, owned context)` — it threads no carry, so blocks may arrive
/// in any order and even concurrently. `merge` does the real work: sort to chain
/// order, prefix-sum the measures from the carry to get each block's start
/// position, `lift` every block in parallel, order-preserving tree-`reduce` the
/// segments, stitch the carry on with one `combine`, then `project` each
/// height's value in parallel.
///
/// **Per-batch memory:** the combined segment retains every complete node of the
/// batch (that is what makes `project` a lookup), bounded by the index's
/// [`Segment`](OrderedMonoidCarry::Segment); for the toy and the real tree index
/// that is `≈ 2 × leaves` nodes (≈25 MB for a spam-era 500k-leaf batch).
impl<I: OrderedMonoidCarry> CumulativeExec<I> for OrderedMonoid
where
    I: Schema<Vec<<I as IndexDef>::Delta>>,
    I::BlockContext: Send + Sync,
{
    type Buffer = Vec<(BlockOffset, I::BlockContext)>;

    fn new_buffer() -> Self::Buffer {
        Vec::new()
    }

    fn extract(
        buffer: &Mutex<Self::Buffer>,
        _carry: &Mutex<I::PriorState>,
        offset: BlockOffset,
        ctx: I::BlockContext,
    ) -> Result<(), PipelineError> {
        buffer
            .lock()
            .expect("context buffer mutex poisoned")
            .push((offset, ctx));
        Ok(())
    }

    fn merge(
        buffer: &Mutex<Self::Buffer>,
        carry: &Mutex<I::PriorState>,
    ) -> Result<Vec<I::Delta>, PipelineError> {
        // Sort to chain order — extraction may have buffered out of order.
        let mut blocks: Vec<(BlockOffset, I::BlockContext)> = buffer
            .lock()
            .expect("context buffer mutex poisoned")
            .drain(..)
            .collect();
        blocks.sort_unstable_by_key(|(offset, _)| *offset);
        if blocks.is_empty() {
            return Ok(Vec::new());
        }

        let mut carry = carry.lock().expect("carry mutex poisoned");

        // Measure prefix sum from the carry: each block's absolute start position
        // and its end (the position whose frontier the block's height records).
        let start0 = I::carry_measure(&carry);
        let mut starts = Vec::with_capacity(blocks.len());
        let mut ends = Vec::with_capacity(blocks.len());
        let mut running = start0;
        for (_, ctx) in &blocks {
            starts.push(running);
            running = I::measure_add(running, I::measure_of(ctx));
            ends.push(running);
        }

        // Lift every block independently — all the hashing, in parallel.
        let segments: Vec<I::Segment> = blocks
            .par_iter()
            .zip(starts.par_iter())
            .map(|((_, ctx), &start)| I::lift(ctx, start))
            .collect::<Result<Vec<_>, I::Error>>()
            .map_err(PipelineError::extract)?;

        // Order-preserving tree reduce: rayon's `reduce` over an indexed parallel
        // iterator combines adjacent operands in order, which an associative
        // `combine` needs (it is not commutative).
        let batch = segments.into_par_iter().reduce(I::identity, I::combine);

        // One seam stitch onto the carried frontier.
        let full = I::combine(I::carry_segment(&carry), batch);

        // Every height's value is a parallel lookup into the combined segment.
        let entries: Vec<(I::Key, I::Value)> = blocks
            .par_iter()
            .zip(ends.par_iter())
            .map(|((_, ctx), &end)| (I::key_of(ctx), I::project(&full, end)))
            .collect();

        // Advance the carry to the last height's frontier (PriorState = Value).
        *carry = I::project(&full, *ends.last().expect("batch is non-empty"));

        Ok(<I as Schema<Vec<<I as IndexDef>::Delta>>>::from_entries(
            entries,
        ))
    }
}

/// Bridge for **append-cumulative** `(SelfCumulative, Append)` indexes:
/// per-height series (commitment-tree sizes, cumulative chainwork) whose value
/// at each height is computed from the previous height's.
///
/// Unlike [`CumulativeBridge`], the two roles the model keeps separate are kept
/// separate here (see the sync model, §3.2):
///
/// - **Carry** — a running [`PriorState`](ExtractCumulative::PriorState)
///   threaded across blocks. Reloaded on resume by point-reading the value at
///   the watermark height (`O(1)`), not by replaying the series. For this class
///   `PriorState = Value` ([`CumulativeAppend`]), so the looked-up value *is* the
///   carry.
/// - **Output** — per-height deltas, persisted as disjoint `key = height`
///   entries. Each batch writes only its own heights; it never rewrites or
///   rescans the whole series (the defect of collapsing the carry and the output
///   into one blob).
///
/// `X` is the [`CumulativeExec`] strategy, selected by the scope's carry algebra:
/// [`Sequential`] folds block by block, [`OrderedMonoid`] builds the batch in
/// parallel. Everything else — resume, append persistence, descriptor — is
/// shared.
pub(crate) struct CumulativeAppendBridge<I: CumulativeAppend, X: CumulativeExec<I> = Sequential> {
    descriptor: Descriptor,
    carry: Mutex<I::PriorState>,
    buffer: Mutex<X::Buffer>,
    merged: Mutex<Option<Vec<I::Delta>>>,
    _phantom: PhantomData<(I, X)>,
}

impl<I: CumulativeAppend, X: CumulativeExec<I>> CumulativeAppendBridge<I, X> {
    fn new() -> Self {
        Self {
            descriptor: I::descriptor(<I as zaino_persistence_codec::EntryCodec>::KEY_ORDER),
            carry: Mutex::new(I::initial_carry()),
            buffer: Mutex::new(X::new_buffer()),
            merged: Mutex::new(None),
            _phantom: PhantomData,
        }
    }
}

impl<Ctx, I, X> IndexPipeline<Ctx> for CumulativeAppendBridge<I, X>
where
    I: CumulativeAppend
        + Schema<Vec<<I as IndexDef>::Delta>>
        + zaino_persistence_codec::EntryCodec<Key = BlockHeight>,
    I::PriorState: Clone,
    X: CumulativeExec<I>,
    Ctx: ProvideContext<I::BlockContext> + Send + Sync + 'static,
{
    fn descriptor(&self) -> &Descriptor {
        &self.descriptor
    }

    fn load_state(
        &self,
        reader: &dyn BackendReader,
        resume_from: Option<BlockHeight>,
    ) -> Result<(), PipelineError> {
        let namespace: Namespace = I::NAME.into();

        if !resume_readable::<I>(reader, namespace)? {
            return Ok(());
        }

        // Resume the carry by point-reading the value at the watermark height: an
        // O(1) tip lookup, not a replay. The value at the last committed height
        // *is* the running state (PriorState = Value for this class). A fresh
        // start (`None`) keeps the genesis carry from `new`.
        let Some(height) = resume_from else {
            return Ok(());
        };
        let raw = reader.get(
            namespace,
            &zaino_persistence_codec::encode_key::<I>(&height),
        )?;
        if let Some(bytes) = raw {
            let value = zaino_persistence_codec::decode_value::<I>(&bytes)?;
            *self.carry.lock().expect("carry mutex poisoned") = value;
        }
        Ok(())
    }

    // The offset matters only to the ordered-monoid strategy, which sorts its
    // buffer by it; the sequential strategy folds in chain order and ignores it.
    // The identity projection owns the context clone once, here.
    fn extract_one(&self, offset: BlockOffset, ctx: &Ctx) -> Result<(), PipelineError> {
        X::extract(&self.buffer, &self.carry, offset, ctx.context())
    }

    fn merge(&self) -> Result<(), PipelineError> {
        let deltas = X::merge(&self.buffer, &self.carry)?;
        *self.merged.lock().expect("merged mutex poisoned") = Some(deltas);
        Ok(())
    }

    fn persist(&self) -> Result<Vec<WriteOp>, PipelineError> {
        persist_merged::<I, Vec<I::Delta>>(&self.merged)
    }
}

// ===========================================================================
// CrossBridge — the (CrossIndex, Append) bridge
// ===========================================================================

/// Stateful bridge for `(CrossIndex, Append)` indexes.
///
/// A cross index's extraction reads other indexes' output through a
/// [`DepsReader`], so it runs in a later DAG phase than its dependencies — the
/// scheduler releases its batch β only once every dependency has persisted β
/// into the engine's pending atomic commit (the `Pipelined` firing rule). Once
/// that gate opens the batch is **block-parallel**: each block's delta is an
/// independent read of the dependencies' already-fixed batch output, with no
/// inter-block carry. The engine therefore extracts the batch much like a
/// [`LocalBridge`], differing only in that it threads a `DepsReader` to
/// [`extract_one_cross`](IndexPipeline::extract_one_cross).
///
/// Composition is [`Append`]: each block emits a disjoint entry. Deltas are
/// tagged with their [`BlockOffset`] and reordered to chain order before persist,
/// matching `LocalBridge` — Append does not depend on order, but the uniform
/// reorder keeps the merged entry sequence deterministic.
pub(crate) struct CrossBridge<I: IndexDef> {
    descriptor: Descriptor,
    deltas: Mutex<Vec<(BlockOffset, I::Delta)>>,
    merged: Mutex<Option<Vec<I::Delta>>>,
    _phantom: PhantomData<I>,
}

impl<I> CrossBridge<I>
where
    I: IndexDef + zaino_persistence_codec::EntryCodec,
{
    fn new() -> Self {
        Self {
            descriptor: I::descriptor(<I as zaino_persistence_codec::EntryCodec>::KEY_ORDER),
            deltas: Mutex::new(Vec::new()),
            merged: Mutex::new(None),
            _phantom: PhantomData,
        }
    }
}

impl<Ctx, I> IndexPipeline<Ctx> for CrossBridge<I>
where
    I: ExtractCross
        + MergeAppend
        + Schema<Vec<<I as IndexDef>::Delta>>
        + zaino_persistence_codec::EntryCodec,
    Ctx: ProvideContext<I::BlockContext> + Send + Sync + 'static,
{
    fn descriptor(&self) -> &Descriptor {
        &self.descriptor
    }

    // A cross index extracts through `extract_one_cross` — it needs a
    // `DepsReader`. The engine routes cross jobs there, so this is never called;
    // reaching it is a scope-routing bug.
    fn extract_one(&self, _offset: BlockOffset, _ctx: &Ctx) -> Result<(), PipelineError> {
        Err(PipelineError::ScopeRouting {
            index: I::NAME.as_str(),
        })
    }

    fn extract_one_cross(
        &self,
        offset: BlockOffset,
        ctx: &Ctx,
        deps: &DepsReader<'_>,
    ) -> Result<(), PipelineError> {
        let delta = I::extract(&ctx.context(), deps).map_err(PipelineError::extract)?;
        self.deltas
            .lock()
            .expect("delta mutex poisoned")
            .push((offset, delta));
        Ok(())
    }

    fn merge(&self) -> Result<(), PipelineError> {
        // Append does not depend on order, but reorder to chain order anyway
        // (shared with `LocalBridge`) so the persisted entry sequence is
        // deterministic regardless of rayon completion order.
        let deltas = drain_reorder(&self.deltas);
        *self.merged.lock().expect("merged mutex poisoned") = Some(deltas);
        Ok(())
    }

    fn persist(&self) -> Result<Vec<WriteOp>, PipelineError> {
        persist_merged::<I, Vec<I::Delta>>(&self.merged)
    }
}
