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
    pub trait Carry: Send + Sync + 'static {}
}

// ---------------------------------------------------------------------------
// Axis 1 — InputScope (what data the extractor needs beyond the block)
// ---------------------------------------------------------------------------

/// Extraction uses only the current block's context.
/// No DepsReader, no prior state, no source handle.
pub struct BlockLocal;

/// Extraction needs this index's own accumulated state from prior blocks.
///
/// The type parameter `C` is the **carry algebra**: how that accumulated state
/// composes from block to block. It is meaningful *only* for a cumulative
/// scope, so it lives here as a parameter of the marker rather than on every
/// descriptor — [`BlockLocal`] and [`CrossIndex`] have no carry and cannot name
/// one. `C` defaults to [`Sequential`] (today's one-block-at-a-time extraction),
/// so an index that writes `type Scope = SelfCumulative;` keeps that behaviour
/// unchanged.
///
/// A carry is meaningful only here, and the type system enforces it: the
/// non-cumulative scope markers are not generic over a carry, so they cannot
/// name one. A [`BlockLocal`] index given a carry does not compile:
///
/// ```compile_fail
/// use zaino_sync::descriptor::{BlockLocal, OrderedMonoid};
/// // `BlockLocal` takes no type arguments — only `SelfCumulative` carries one.
/// type NotAllowed = BlockLocal<OrderedMonoid>;
/// ```
pub struct SelfCumulative<C: CarryAlgebra = Sequential>(core::marker::PhantomData<C>);

/// Extraction needs committed output from other indexes (via DepsReader).
pub struct CrossIndex;

impl sealed::Scope for BlockLocal {}
impl<C: CarryAlgebra> sealed::Scope for SelfCumulative<C> {}
impl sealed::Scope for CrossIndex {}

/// Runtime-inspectable mirror of the type-level scope marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, strum::Display)]
pub enum InputScope {
    /// Only needs the current block's context.
    BlockLocal,
    /// Needs own accumulated state from prior blocks, composed by `carry`.
    SelfCumulative {
        /// How the accumulated state composes from block to block.
        carry: CarryType,
    },
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
impl<C: CarryAlgebra> Scope for SelfCumulative<C> {
    const VALUE: InputScope = InputScope::SelfCumulative { carry: C::VALUE };
}
impl Scope for CrossIndex {
    const VALUE: InputScope = InputScope::CrossIndex;
}

// ---------------------------------------------------------------------------
// Carry algebra — a parameter of the SelfCumulative scope only
// ---------------------------------------------------------------------------

/// How a cumulative index's carried state composes from block to block.
///
/// Sealed — the set of carry algebras is fixed here. A carry algebra is only
/// ever named through [`SelfCumulative<C>`], so no non-cumulative scope can
/// declare one.
pub trait CarryAlgebra: sealed::Carry + Send + Sync + 'static {
    /// The runtime-inspectable value matching this carry marker type.
    const VALUE: CarryType;
}

/// The carry is an opaque step: `extract(ctx, prior)`, one block at a time.
pub struct Sequential;

/// The carry is an ordered monoid with a measure, so a batch's per-height state
/// can be built by measure → lift → ordered reduce → projection rather than a
/// serial fold. The engine machinery for this lands in a later task; today the
/// marker exists so a cumulative index can *declare* this algebra.
pub struct OrderedMonoid;

impl sealed::Carry for Sequential {}
impl sealed::Carry for OrderedMonoid {}

impl CarryAlgebra for Sequential {
    const VALUE: CarryType = CarryType::Sequential;
}
impl CarryAlgebra for OrderedMonoid {
    const VALUE: CarryType = CarryType::OrderedMonoid;
}

/// Runtime-inspectable mirror of the carry algebra marker `C` in
/// [`SelfCumulative<C>`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, strum::Display)]
pub enum CarryType {
    /// One block at a time: `extract(ctx, prior)`.
    Sequential,
    /// An ordered monoid with a measure (parallel measure → lift → reduce).
    OrderedMonoid,
}

/// The scope markers that carry accumulated state, exposing the carry algebra
/// as an associated type.
///
/// Implemented only for [`SelfCumulative<C>`] (its supertrait [`Scope`] is
/// sealed, so no other type can satisfy it). A bound `Self::Scope:
/// IsSelfCumulative` therefore admits exactly the cumulative scopes — rejecting
/// [`BlockLocal`] and [`CrossIndex`] — while letting a cumulative trait name the
/// carry generically, so an index never has to restate it.
pub trait IsSelfCumulative: Scope {
    /// The carry algebra of this cumulative scope.
    type Carry: CarryAlgebra;
}

impl<C: CarryAlgebra> IsSelfCumulative for SelfCumulative<C> {
    type Carry = C;
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

#[cfg(test)]
mod tests {
    use super::{
        BlockLocal, CarryType, CrossIndex, InputScope, OrderedMonoid, Scope, SelfCumulative,
        Sequential,
    };
    use crate::traits::IndexDef;

    // The carry parameter defaults to `Sequential`, so a bare `SelfCumulative`
    // marker (what every existing cumulative index writes) reports a sequential
    // carry in its runtime scope.
    #[test]
    fn bare_self_cumulative_defaults_to_sequential_carry() {
        assert_eq!(
            <SelfCumulative as Scope>::VALUE,
            InputScope::SelfCumulative {
                carry: CarryType::Sequential
            }
        );
        assert_eq!(
            <SelfCumulative<Sequential> as Scope>::VALUE,
            InputScope::SelfCumulative {
                carry: CarryType::Sequential
            }
        );
    }

    // The ordered-monoid carry is reflected distinctly in the runtime mirror.
    #[test]
    fn ordered_monoid_carry_reflected_in_scope() {
        assert_eq!(
            <SelfCumulative<OrderedMonoid> as Scope>::VALUE,
            InputScope::SelfCumulative {
                carry: CarryType::OrderedMonoid
            }
        );
    }

    #[test]
    fn non_cumulative_scopes_have_no_carry() {
        assert_eq!(<BlockLocal as Scope>::VALUE, InputScope::BlockLocal);
        assert_eq!(<CrossIndex as Scope>::VALUE, InputScope::CrossIndex);
    }

    // A real cumulative index (the `(SelfCumulative, Append)` archetype, the
    // in-crate analogue of `ChainMetadataIndex`) reports a sequential carry
    // without naming one — it inherits the default through `type Scope =
    // SelfCumulative;`.
    #[test]
    fn cumulative_index_descriptor_scope_carries_sequential() {
        use crate::testing::toy_indexes::cumulative_series_index::CumulativeSeriesIndex;
        let scope =
            <CumulativeSeriesIndex as IndexDef>::descriptor(super::KeyOrder::WalkOrdered).scope;
        assert_eq!(
            scope,
            InputScope::SelfCumulative {
                carry: CarryType::Sequential
            }
        );
    }

    // A block-local index (the in-crate analogue of `HeadersIndex`) reports
    // `BlockLocal`, with no carry to name.
    #[test]
    fn block_local_index_descriptor_scope_is_block_local() {
        use crate::testing::toy_indexes::value_index::ValueIndex;
        let scope = <ValueIndex as IndexDef>::descriptor(super::KeyOrder::WalkOrdered).scope;
        assert_eq!(scope, InputScope::BlockLocal);
    }
}
