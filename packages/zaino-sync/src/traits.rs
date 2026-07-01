//! Type-level trait hierarchy for index definitions.
//!
//! Each index implements [`IndexDef`] to declare its two-axis position, then
//! exactly one extract trait (scope axis) and exactly one merge trait
//! (composition axis). The trait bounds make it a compile error to, e.g.,
//! access a `DepsReader` from a `BlockLocal` index or declare a monoidal
//! `combine` on an `Append` index.

use crate::descriptor::{
    Append, BlockLocal, Composition, CrossIndex, Descriptor, Fold, Monoidal, Scope,
    SelfCumulative,
};
use crate::primitives::IndexId;

// ---------------------------------------------------------------------------
// Placeholder types — will be fleshed out in their own modules
// ---------------------------------------------------------------------------

/// Read handle over committed index state, restricted to earlier phases.
pub struct DepsReader;

/// Handle for non-local source access (the escape hatch).
pub struct SourceHandle;

/// A single write operation produced by the merge step.
#[derive(Debug)]
pub enum WriteOp {
    /// Insert or overwrite a key-value pair.
    Put {
        /// Target index.
        index: IndexId,
        /// Serialised key.
        key: Vec<u8>,
        /// Serialised value.
        value: Vec<u8>,
    },
    /// Remove a key.
    Delete {
        /// Target index.
        index: IndexId,
        /// Serialised key.
        key: Vec<u8>,
    },
}

// ---------------------------------------------------------------------------
// IndexDef — the root trait that pins both axes
// ---------------------------------------------------------------------------

/// Root declaration for any index. Pins the scope and composition axes as
/// associated types, which downstream extract/merge traits use as bounds.
///
/// `S` and `C` are marker types from [`crate::descriptor`].
pub trait IndexDef: Send + Sync + 'static {
    /// Type-level scope marker (BlockLocal | SelfCumulative | CrossIndex).
    type Scope: Scope;

    /// Type-level composition marker (Append | Monoidal | Fold).
    type Composition: Composition;

    /// The per-block contribution produced by extraction.
    type Delta: Send + Sync;

    /// The block context type (matches the provisioner's output).
    type Context: Send + Sync;

    /// The full declarative descriptor.
    fn descriptor() -> Descriptor;
}

// ===========================================================================
// Axis 1: Extract traits — one per scope value.
// The bound `IndexDef<Scope = X>` makes it a compile error to implement
// the wrong one.
// ===========================================================================

/// Extraction for block-local indexes.
///
/// The extract signature takes only the block context — no deps reader,
/// no prior state, no source handle. This is the only scope that permits
/// full parallelism across blocks.
pub trait ExtractLocal: IndexDef<Scope = BlockLocal> {
    /// Produce this block's delta from the block context alone.
    fn extract(ctx: &Self::Context) -> Result<Self::Delta, ExtractError>;
}

/// Extraction for self-cumulative indexes.
///
/// The extract signature includes a read handle to this index's own
/// committed prior state. Extraction for this index is sequential (each
/// block depends on the previous block's committed output), but the index
/// runs in parallel with other indexes that have no dependency on it.
pub trait ExtractCumulative: IndexDef<Scope = SelfCumulative> {
    /// Opaque type representing this index's accumulated state, as read
    /// from the backend after the prior batch's commit.
    type PriorState: Send + Sync;

    /// Produce this block's delta given the block context and this index's
    /// own committed state up to (but not including) this block.
    fn extract(
        ctx: &Self::Context,
        prior: &Self::PriorState,
    ) -> Result<Self::Delta, ExtractError>;
}

/// Extraction for cross-index indexes.
///
/// The extract signature includes a [`DepsReader`] — a read handle
/// restricted to committed state from indexes declared in the dependency
/// set. This index runs in a later phase than its dependencies.
pub trait ExtractCross: IndexDef<Scope = CrossIndex> {
    /// Produce this block's delta given the block context and committed
    /// state from dependency indexes.
    fn extract(ctx: &Self::Context, deps: &DepsReader) -> Result<Self::Delta, ExtractError>;
}

// ===========================================================================
// Axis 2: Merge traits — one per composition value.
// The bound `IndexDef<Composition = X>` makes it a compile error to
// implement the wrong one.
// ===========================================================================

/// Merge for append-type indexes (disjoint keys).
///
/// No actual merge logic — each delta's write ops are collected and applied.
/// The engine can batch writes from multiple blocks without any combine step.
pub trait MergeAppend: IndexDef<Composition = Append> {
    /// Convert a single block's delta directly to write operations.
    fn to_write_ops(delta: Self::Delta) -> Vec<WriteOp>;
}

/// Merge for monoidal-type indexes (associative + commutative combine).
///
/// The engine may merge deltas from multiple blocks in any order using a
/// parallel reduce tree. The implementor must ensure `combine` is
/// associative and commutative — the type system cannot enforce these
/// algebraic properties, but the engine relies on them for correctness.
pub trait MergeMonoidal: IndexDef<Composition = Monoidal> {
    /// The intermediate type used during the reduce.
    type Accumulator: Send + Sync;

    /// The identity element: `combine(identity(), x) == x`.
    fn identity() -> Self::Accumulator;

    /// Lift a single delta into the accumulator space.
    fn lift(delta: Self::Delta) -> Self::Accumulator;

    /// Associative, commutative combine of two accumulators.
    fn combine(a: Self::Accumulator, b: Self::Accumulator) -> Self::Accumulator;

    /// Convert the fully-merged accumulator to write operations.
    fn to_write_ops(merged: Self::Accumulator) -> Vec<WriteOp>;
}

/// Merge for fold-type indexes (order-dependent sequential application).
///
/// Deltas must be folded in strict chain order. The engine cannot
/// parallelise the merge step for this composition type.
pub trait MergeFold: IndexDef<Composition = Fold> {
    /// The running state threaded through the fold.
    type FoldState: Send + Sync;

    /// The initial state before the first block in a batch.
    fn initial_state() -> Self::FoldState;

    /// Apply one block's delta to the running state. Called in chain order.
    fn fold(state: &mut Self::FoldState, delta: Self::Delta);

    /// Convert the final fold state to write operations.
    fn to_write_ops(state: Self::FoldState) -> Vec<WriteOp>;
}

// ===========================================================================
// Source-access overlay — orthogonal to both axes.
// ===========================================================================

/// Implemented by indexes that declared `SourceAccess::NonLocal` in their
/// descriptor. Provides a source handle to the extract method.
///
/// This is an escape hatch. The default path is pure extraction from
/// BlockContext + (prior state | deps). When an index needs non-local source
/// data and adding an intermediate index is disproportionate, it implements
/// this trait to receive a [`SourceHandle`] during extraction.
///
/// The engine uses the presence of this impl (via the erased layer) to
/// adjust scheduling — non-local extractions are assumed I/O-bound and
/// may be given dedicated task slots.
pub trait NonLocalSource: IndexDef {
    /// Provide the source handle. Called by the engine just before
    /// extraction for each block.
    fn with_source(source: &SourceHandle) -> SourceGuard<'_>;
}

/// RAII guard holding a reference to the source handle for the duration of
/// one extraction call.
pub struct SourceGuard<'a> {
    _handle: &'a SourceHandle,
}

// ===========================================================================
// Error types (stub)
// ===========================================================================

/// Errors during index extraction.
#[derive(Debug, thiserror::Error)]
pub enum ExtractError {
    /// A generic extraction failure.
    #[error("extraction failed: {0}")]
    Failed(String),
}
