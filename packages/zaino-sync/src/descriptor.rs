//! Declarative index descriptors and type-level axis markers.

use bitflags::bitflags;

use crate::primitives::IndexId;

// ---------------------------------------------------------------------------
// Sealed marker traits — one per axis.
// Implementors live only in this module; downstream code selects but cannot
// extend the set of axis values.
// ---------------------------------------------------------------------------

mod sealed {
    pub trait Scope: Send + Sync + 'static {}
    pub trait Composition: Send + Sync + 'static {}
}

// ---------------------------------------------------------------------------
// Axis 1 — InputScope (what data the extractor needs beyond the block)
// ---------------------------------------------------------------------------

/// Extraction uses only the current block's context.
/// No DepsReader, no prior state, no source handle.
pub struct BlockLocal;

/// Extraction needs this index's own accumulated state from prior blocks.
pub struct SelfCumulative;

/// Extraction needs committed output from other indexes (via DepsReader).
pub struct CrossIndex;

impl sealed::Scope for BlockLocal {}
impl sealed::Scope for SelfCumulative {}
impl sealed::Scope for CrossIndex {}

/// Runtime-inspectable mirror of the type-level scope marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InputScope {
    /// Only needs the current block's context.
    BlockLocal,
    /// Needs own accumulated state from prior blocks.
    SelfCumulative,
    /// Needs committed output from other indexes.
    CrossIndex,
}

/// Bridge from type-level marker to runtime enum.
pub trait Scope: sealed::Scope {
    /// The runtime-inspectable value matching this marker type.
    const VALUE: InputScope;
}

impl Scope for BlockLocal {
    const VALUE: InputScope = InputScope::BlockLocal;
}
impl Scope for SelfCumulative {
    const VALUE: InputScope = InputScope::SelfCumulative;
}
impl Scope for CrossIndex {
    const VALUE: InputScope = InputScope::CrossIndex;
}

// ---------------------------------------------------------------------------
// Axis 2 — CompositionType (how per-block deltas are merged)
// ---------------------------------------------------------------------------

/// Disjoint keys across blocks. Merge = collect.
pub struct Append;

/// Overlapping keys with associative + commutative combine.
/// Merge can be parallelised with a reduce tree.
pub struct Monoidal;

/// Order-dependent. Must apply in chain order. Merge is sequential.
pub struct Fold;

impl sealed::Composition for Append {}
impl sealed::Composition for Monoidal {}
impl sealed::Composition for Fold {}

/// Runtime-inspectable mirror of the type-level composition marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CompositionType {
    /// Disjoint keys. Merge = collect.
    Append,
    /// Associative + commutative. Merge = parallel reduce.
    Monoidal,
    /// Order-dependent. Merge = sequential fold.
    Fold,
}

/// Bridge from type-level marker to runtime enum.
pub trait Composition: sealed::Composition {
    /// The runtime-inspectable value matching this marker type.
    const VALUE: CompositionType;
}

impl Composition for Append {
    const VALUE: CompositionType = CompositionType::Append;
}
impl Composition for Monoidal {
    const VALUE: CompositionType = CompositionType::Monoidal;
}
impl Composition for Fold {
    const VALUE: CompositionType = CompositionType::Fold;
}

// ---------------------------------------------------------------------------
// Source access — whether extraction may reach the source for non-local data
// ---------------------------------------------------------------------------

/// Whether an extractor needs a source handle beyond the block context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceAccess {
    /// Extraction is pure: BlockContext + (optional deps/prior state) only.
    None,
    /// Extraction may call the source for non-local data.
    /// The engine provides a source handle and adjusts scheduling.
    NonLocal,
}

// ---------------------------------------------------------------------------
// Source requirements — what the provisioner must fetch per block
// ---------------------------------------------------------------------------

bitflags! {
    /// Declared per-index. The provisioner computes the union across all
    /// registered indexes and fetches only what is needed.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct SourceRequirements: u32 {
        /// Raw block data (always implied).
        const BLOCK       = 0b0001;
        /// Sapling/Orchard commitment tree roots.
        const TREE_ROOTS  = 0b0010;
        /// Cumulative tree sizes.
        const TREE_SIZES  = 0b0100;
        /// Chainwork of parent block.
        const PARENT_WORK = 0b1000;
    }
}

// ---------------------------------------------------------------------------
// ContextRequirements — type-level source of truth for provisioner needs
// ---------------------------------------------------------------------------

/// Declares what data a block context type requires from the provisioner.
///
/// Each index's [`BlockContext`](super::traits::IndexDef::BlockContext)
/// implements this trait. The engine unions the requirements across all
/// registered indexes and configures the provisioner accordingly.
///
/// This is the single source of truth — indexes don't declare
/// requirements separately. The context type *is* the requirement.
pub trait ContextRequirements: Send + Sync + 'static {
    /// The provisioner requirements needed to populate this context.
    const REQUIREMENTS: SourceRequirements;
}

/// Unit context: no data needed (e.g. a pure counting index).
impl ContextRequirements for () {
    const REQUIREMENTS: SourceRequirements = SourceRequirements::empty();
}

// ---------------------------------------------------------------------------
// Descriptor — the full declarative spec of an index
// ---------------------------------------------------------------------------

/// Static, declarative properties of an index. No logic.
///
/// Carries both runtime-inspectable enums (for the engine's DAG builder)
/// and is associated with type-level markers (via [`IndexDef`]) for
/// compile-time enforcement of valid operations.
///
/// [`IndexDef`]: super::traits::IndexDef
#[derive(Debug, Clone)]
pub struct Descriptor {
    /// Unique name, used as the key in the DAG and in WriteOps.
    pub name: IndexId,
    /// What data the extractor needs beyond the block.
    pub scope: InputScope,
    /// How per-block deltas are merged.
    pub composition: CompositionType,
    /// Indexes this one depends on (must form a DAG).
    pub dependencies: &'static [IndexId],
    /// What the provisioner must fetch for this index.
    ///
    /// Derived from the index's `BlockContext` type via
    /// [`ContextRequirements`]. Not declared manually.
    pub requirements: SourceRequirements,
    /// Whether extraction may reach the source for non-local data.
    pub source_access: SourceAccess,
}
