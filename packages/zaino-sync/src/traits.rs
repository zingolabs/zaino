//! Type-level trait hierarchy for index definitions.
//!
//! Each index implements [`IndexDef`] to declare its two-axis position, then
//! exactly one extract trait (scope axis) and exactly one merge trait
//! (composition axis). The trait bounds make it a compile error to, e.g.,
//! access a `DepsReader` from a `BlockLocal` index or declare a monoidal
//! `combine` on an `Append` index.

use crate::descriptor::{
    Append, BlockLocal, Composition, CrossIndex, Descriptor, Fold, Monoidal, Scope,
    SelfCumulative, SourceAccess,
};
use crate::primitives::IndexId;

// ---------------------------------------------------------------------------
// ProvideContext — projection from set-wide context to index block context
// ---------------------------------------------------------------------------

/// Projection from a set-wide block context to an index's block context.
///
/// The provisioner produces one context (`Ctx`) per block for the whole
/// index set. Each index declares its own [`BlockContext`] — the subset
/// of block data it needs. `ProvideContext<T>` produces a `T` that the
/// bridge passes to extraction.
///
/// The identity blanket impl covers the common case where the index's
/// block context *is* the set-wide context. For richer set-wide contexts,
/// implement this trait for each projection:
///
/// ```text
/// impl ProvideContext<BlockData> for FullBlockContext {
///     fn context(&self) -> BlockData {
///         BlockData { height: self.height, hash: self.hash }
///     }
/// }
/// ```
///
/// [`BlockContext`]: IndexDef::BlockContext
pub trait ProvideContext<T> {
    /// Produce the narrowed block context of type `T`.
    fn context(&self) -> T;
}

/// Identity projection: any type provides itself via clone.
impl<T: Clone> ProvideContext<T> for T {
    fn context(&self) -> T {
        self.clone()
    }
}

// ---------------------------------------------------------------------------
// Placeholder types — will be fleshed out in their own modules
// ---------------------------------------------------------------------------

/// Read handle over committed index state, restricted to earlier phases.
pub struct DepsReader;

/// Handle for non-local source access (the escape hatch).
pub struct SourceHandle;

// WriteOp is defined in zaino-persistence and re-exported via crate::backend.

// ---------------------------------------------------------------------------
// IndexDef — the root trait that pins both axes
// ---------------------------------------------------------------------------

/// Root declaration for any index. Pins the scope and composition axes as
/// associated types, which downstream extract/merge traits use as bounds.
///
/// The implementor declares axes, name, and dependencies. The
/// [`descriptor`](Self::descriptor) method is provided — it derives
/// runtime-inspectable values from the type-level markers automatically.
pub trait IndexDef: Send + Sync + 'static {
    /// Type-level scope marker (BlockLocal | SelfCumulative | CrossIndex).
    type Scope: Scope;

    /// Type-level composition marker (Append | Monoidal | Fold).
    type Composition: Composition;

    /// The per-block contribution produced by extraction.
    type Delta: Send + Sync;

    /// The block context this index needs for extraction.
    ///
    /// This is the index's view of a block — just the data it cares about.
    /// The set-wide context (what the provisioner produces) narrows to this
    /// type via [`ProvideContext`].
    type BlockContext: Send + Sync + 'static;

    /// Unique name for this index. Used as the key in the DAG and in
    /// write operations.
    const NAME: IndexId;

    /// Indexes this one depends on (must form a DAG).
    const DEPENDENCIES: &'static [IndexId] = &[];

    /// Whether extraction may reach the source for non-local data.
    const SOURCE_ACCESS: SourceAccess = SourceAccess::None;

    /// The full declarative descriptor, derived from type-level markers.
    ///
    /// Provided — implementors do not override this.
    fn descriptor() -> Descriptor {
        Descriptor {
            name: Self::NAME,
            scope: Self::Scope::VALUE,
            composition: Self::Composition::VALUE,
            dependencies: Self::DEPENDENCIES,
            source_access: Self::SOURCE_ACCESS,
        }
    }
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
    fn extract(ctx: &Self::BlockContext) -> Result<Self::Delta, ExtractError>;
}

/// Extraction for self-cumulative indexes.
///
/// The extract signature includes this index's own accumulated state
/// through the prior block. Extraction for this index is sequential
/// within a batch (each block depends on the previous block's state),
/// but the index runs in parallel with other indexes that have no
/// dependency on it.
///
/// The bridge threads state automatically using the index's merge
/// trait: [`MergeMonoidal`] provides `identity` + `combine` + `lift`;
/// [`MergeFold`] provides `initial_state` + `fold`. No separate
/// `advance_state` method is needed — the composition axis already
/// declares the algebra.
pub trait ExtractCumulative: IndexDef<Scope = SelfCumulative> {
    /// The accumulated state threaded through extractions.
    ///
    /// For (S, M) indexes this is the same type as the monoidal
    /// `Accumulator`. For (S, F) indexes it matches `FoldState`.
    /// The bridge enforces this via a type equality bound.
    type PriorState: Send + Sync;

    /// Produce this block's delta given the block context and this index's
    /// own accumulated state up to (but not including) this block.
    fn extract(
        ctx: &Self::BlockContext,
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
    fn extract(ctx: &Self::BlockContext, deps: &DepsReader) -> Result<Self::Delta, ExtractError>;
}

// ===========================================================================
// Axis 2: Merge traits — one per composition value.
// The bound `IndexDef<Composition = X>` makes it a compile error to
// implement the wrong one.
// ===========================================================================

/// Merge for append-type indexes (disjoint keys).
///
/// Marker trait — Append composition has no merge logic. Each delta
/// is independent. The bridge collects deltas and passes them to
/// [`Persist`] at batch boundary.
pub trait MergeAppend: IndexDef<Composition = Append> {}

/// Merge for monoidal-type indexes (associative + commutative combine).
///
/// The engine may merge deltas from multiple blocks in any order using a
/// parallel reduce tree. The implementor must ensure `combine` is
/// associative and commutative — the type system cannot enforce these
/// algebraic properties, but the engine relies on them for correctness.
///
/// Pure domain algebra — no persistence concern. Serialization of the
/// merged accumulator is handled by [`Persist`].
pub trait MergeMonoidal: IndexDef<Composition = Monoidal> {
    /// The intermediate type used during the reduce.
    type Accumulator: Send + Sync;

    /// The identity element: `combine(identity(), x) == x`.
    fn identity() -> Self::Accumulator;

    /// Lift a single delta into the accumulator space.
    fn lift(delta: Self::Delta) -> Self::Accumulator;

    /// Associative, commutative combine of two accumulators.
    fn combine(a: Self::Accumulator, b: Self::Accumulator) -> Self::Accumulator;
}

/// Merge for fold-type indexes (order-dependent sequential application).
///
/// Deltas must be folded in strict chain order. The engine cannot
/// parallelise the merge step for this composition type.
///
/// Pure domain logic — no persistence concern. Serialization of the
/// final fold state is handled by [`Persist`].
pub trait MergeFold: IndexDef<Composition = Fold> {
    /// The running state threaded through the fold.
    type FoldState: Send + Sync;

    /// The initial state before the first block in a batch.
    fn initial_state() -> Self::FoldState;

    /// Apply one block's delta to the running state. Called in chain order.
    fn fold(state: &mut Self::FoldState, delta: Self::Delta);
}

// ===========================================================================
// Schema — index entry declaration, separate from merge logic.
//
// The index declares its key/value types and how its merged result
// maps to entries. The types handle serialization via `Encode`.
// The bridge does the mechanical conversion to `WriteOp`s.
// ===========================================================================

/// Declares an index's key-value schema, entry mapping, and persistence encoding.
///
/// Generic over `M` — the merge result type. Each composition produces
/// a different merge result:
/// - Append: `Vec<Self::Delta>`
/// - Monoidal: `Self::Accumulator`
/// - Fold: `Self::FoldState`
///
/// The index implements `Schema<M>` for its composition's output type.
/// The bridge calls `into_entries` / `from_entries` for domain mapping,
/// and `encode_key` / `encode_value` / `decode_key` / `decode_value`
/// for persistence. The sync engine never touches bytes directly.
///
/// **Encoding lives in the index**, not on the types. The index author
/// defines both the domain mapping AND the byte representation in one
/// place. No orphan-rule issues, versioning is local to the index.
///
/// Using a type parameter instead of an associated type avoids cycle
/// errors that arise when the merged type references the index's own
/// associated types (e.g. `Vec<Self::Delta>`).
pub trait Schema<M>: IndexDef {
    /// The key type for this index's entries.
    type Key: Send + Sync;
    /// The value type for this index's entries.
    type Value: Send + Sync;

    /// Map a merge result to typed key-value entries.
    fn into_entries(merged: M) -> Vec<(Self::Key, Self::Value)>;

    /// Reconstruct a merge result from typed key-value entries.
    ///
    /// The mechanical inverse of [`into_entries`](Self::into_entries).
    fn from_entries(entries: Vec<(Self::Key, Self::Value)>) -> M;

    /// Encode a key to its on-disk byte representation.
    fn encode_key(key: &Self::Key) -> Vec<u8>;

    /// Encode a value to its on-disk byte representation.
    fn encode_value(value: &Self::Value) -> Vec<u8>;

    /// Decode a key from its on-disk byte representation.
    fn decode_key(bytes: &[u8]) -> Result<Self::Key, SchemaDecodeError>;

    /// Decode a value from its on-disk byte representation.
    fn decode_value(bytes: &[u8]) -> Result<Self::Value, SchemaDecodeError>;
}

/// Error from decoding a persisted key or value.
#[derive(Debug, thiserror::Error)]
pub enum SchemaDecodeError {
    /// The byte slice has the wrong length or format.
    #[error("{0}")]
    Invalid(String),
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
