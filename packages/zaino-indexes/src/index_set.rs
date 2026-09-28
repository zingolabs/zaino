//! Index sets: which indexes a finalised store builds, as a **type**.
//!
//! An index set is the *static* half of what a finalised store can answer:
//! the index set a deployment was set up to build. It is a type, not a value,
//! so that a serving read whose backing indexes are not built is a read that
//! does not exist on the store — checked where the store is wired into a use
//! case, not discovered per request. The *dynamic* half — to what height each
//! built index has reached — is the serviceability manifest, derived at
//! snapshot time from the committed watermark.
//!
//! # Membership
//!
//! [`Builds<I>`] is type-level membership: `M: Builds<HeadersIndex>` says the
//! index set `M` builds the headers index. A reader over `M` implements a
//! serving read exactly where every index that read composes on is built; a
//! use case that demands a read the index set cannot back fails at its
//! wiring bound.
//!
//! # One list, two facts
//!
//! [`index_set!`](macro@crate::index_set) declares an index set from a single index list and
//! emits both facts from it: the runtime [`IndexSet`] the sync engine builds
//! and the `Builds` impls the type promises. They cannot drift because there is
//! nothing to keep in step — one list is the source of both.
//!
//! ```text
//! built(M)   = { I : M: Builds<I> }              static, from the type
//! reach(M,w) = ToHeight(w) for every I ∈ built(M)  dynamic, from the watermark
//! ```

pub use zaino_sync::index_pipelines::IndexPipelines;
pub use zaino_sync::primitives::IndexId;
pub use zaino_sync::traits::IndexDef;

/// A named finalised-store index set, as a type: the indexes it builds.
pub trait IndexSet: Send + Sync + 'static {
    /// The set-wide provisioning context every index in this set projects from.
    type Context: Send + Sync + 'static;

    /// The runtime index set the sync engine builds for this index set.
    ///
    /// Emitted from the same list as the [`Builds`] impls, so the set the engine
    /// builds is the set the type promises.
    fn pipelines() -> IndexPipelines<Self::Context>;

    /// The identity of every index this index set builds, in declaration
    /// order — the static index set as data, for the manifest derivation.
    const INDEXES: &'static [IndexId];
}

/// Type-level membership: the index set builds index `I`.
///
/// A serving read bounds on the indexes it composes from; an index set
/// lacking one of them does not have that read.
pub trait Builds<I: IndexDef>: IndexSet {}

/// Declare an index set from one index list.
///
/// ```ignore
/// index_set! {
///     /// Compact-block serving only.
///     pub struct LightWallet over CurrentZainoContext {
///         HeadersIndex, TxidsIndex, /* … */
///     }
/// }
/// ```
///
/// Emits the marker type, its [`IndexSet`] impl (whose `pipelines`
/// registers exactly the listed indexes, in order), and one [`Builds`] impl per
/// listed index.
#[macro_export]
macro_rules! index_set {
    (
        $(#[$meta:meta])*
        $vis:vis struct $name:ident over $ctx:ty { $($index:ty),+ $(,)? }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
        $vis struct $name;

        impl $crate::index_set::IndexSet for $name {
            type Context = $ctx;

            fn pipelines() -> $crate::index_set::IndexPipelines<$ctx> {
                $crate::index_set::IndexPipelines::new()
                    $(.with::<$index>())+
            }

            const INDEXES: &'static [$crate::index_set::IndexId] = &[
                $(<$index as $crate::index_set::IndexDef>::NAME),+
            ];
        }

        $(
            impl $crate::index_set::Builds<$index> for $name {}
        )+
    };
}
