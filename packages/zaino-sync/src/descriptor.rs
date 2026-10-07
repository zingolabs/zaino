//! Declarative index descriptors and type-level axis markers.

use crate::primitives::IndexId;

/// Re-exported so [`Descriptor::key_order`] and its construction sites can name
/// the storage fact without reaching across crates. The canonical definition is
/// in [`zaino_persistence`]; each index *states* it on its codec.
pub use zaino_persistence_codec::KeyOrder;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, strum::Display)]
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

/// Overlapping keys combined by an associative operation with an identity.
///
/// The combine is **not** assumed commutative: the engine combines deltas in
/// chain order, and that order is part of the contract. A reduce tree may still
/// parallelise the merge, but only a balanced one that preserves left-to-right
/// order — never one that reorders operands. Nothing here or downstream may
/// rely on commutativity; a commutative fast path, if ever wanted, must be a
/// separately named composition.
pub struct Monoidal;

/// Order-dependent. Must apply in chain order. Merge is sequential.
pub struct Fold;

impl sealed::Composition for Append {}
impl sealed::Composition for Monoidal {}
impl sealed::Composition for Fold {}

/// Runtime-inspectable mirror of the type-level composition marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, strum::Display)]
pub enum CompositionType {
    /// Disjoint keys. Merge = collect.
    Append,
    /// Associative with identity, NOT assumed commutative. Merge = reduce in
    /// chain order.
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, strum::Display)]
pub enum SourceAccess {
    /// Extraction is pure: BlockContext + (optional deps/prior state) only.
    None,
    /// Extraction may call the source for non-local data.
    /// The engine provides a source handle and adjusts scheduling.
    NonLocal,
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
/// Provisioner requirements are not declared here — they are implicit
/// in each index's [`BlockContext`](super::traits::IndexDef::BlockContext)
/// type. The set-wide context must implement
/// [`ProvideContext`](super::traits::ProvideContext) for each index's
/// block context, and the compiler enforces this at registration time.
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
    /// Whether extraction may reach the source for non-local data.
    pub source_access: SourceAccess,
    /// How this index's keys order relative to the chain walk — a storage fact
    /// taken from the index's codec ([`EntryCodec::KEY_ORDER`]), not a type-level
    /// marker. Filled in the bridge constructor, which has the codec bound; it is
    /// deliberately not declared on [`IndexDef`](super::traits::IndexDef), which
    /// knows nothing of the byte layout.
    ///
    /// [`EntryCodec::KEY_ORDER`]: zaino_persistence_codec::EntryCodec::KEY_ORDER
    pub key_order: KeyOrder,
}

impl core::fmt::Display for Descriptor {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{} ({} × {})", self.name, self.scope, self.composition,)?;
        if !self.dependencies.is_empty() {
            write!(f, " deps=[")?;
            for (i, dep) in self.dependencies.iter().enumerate() {
                if i > 0 {
                    write!(f, ", ")?;
                }
                write!(f, "{dep}")?;
            }
            write!(f, "]")?;
        }
        if self.source_access != SourceAccess::None {
            write!(f, " source={}", self.source_access)?;
        }
        Ok(())
    }
}
