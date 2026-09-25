//! Materialisations: which indexes a finalised store builds, as a **type**.
//!
//! A materialisation is the *static* half of what a finalised store can answer:
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
//! materialisation `M` builds the headers index. A reader over `M` implements a
//! serving read exactly where every index that read composes on is built; a
//! use case that demands a read the materialisation cannot back fails at its
//! wiring bound.
//!
//! # One list, two facts
//!
//! [`materialisation!`] declares a materialisation from a single index list and
//! emits both facts from it: the runtime [`IndexSet`] the sync engine builds
//! and the `Builds` impls the type promises. They cannot drift because there is
//! nothing to keep in step — one list is the source of both.
//!
//! ```text
//! built(M)   = { I : M: Builds<I> }              static, from the type
//! reach(M,w) = ToHeight(w) for every I ∈ built(M)  dynamic, from the watermark
//! ```

pub use zaino_sync::index_set::IndexSet;
pub use zaino_sync::primitives::IndexId;
pub use zaino_sync::traits::IndexDef;

/// A named finalised-store materialisation: the index set it builds.
pub trait Materialisation: Send + Sync + 'static {
    /// The set-wide provisioning context every index in this set projects from.
    type Context: Send + Sync + 'static;

    /// The runtime index set the sync engine builds for this materialisation.
    ///
    /// Emitted from the same list as the [`Builds`] impls, so the set the engine
    /// builds is the set the type promises.
    fn index_set() -> IndexSet<Self::Context>;

    /// The identity of every index this materialisation builds, in declaration
    /// order — the static index set as data, for the manifest derivation.
    const INDEXES: &'static [IndexId];
}

/// Type-level membership: the materialisation builds index `I`.
///
/// A serving read bounds on the indexes it composes from; a materialisation
/// lacking one of them does not have that read.
pub trait Builds<I: IndexDef>: Materialisation {}

/// Declare a materialisation from one index list.
///
/// ```ignore
/// materialisation! {
///     /// Compact-block serving only.
///     pub struct LightWallet over CurrentZainoContext {
///         HeadersIndex, TxidsIndex, /* … */
///     }
/// }
/// ```
///
/// Emits the marker type, its [`Materialisation`] impl (whose `index_set`
/// registers exactly the listed indexes, in order), and one [`Builds`] impl per
/// listed index.
#[macro_export]
macro_rules! materialisation {
    (
        $(#[$meta:meta])*
        $vis:vis struct $name:ident over $ctx:ty { $($index:ty),+ $(,)? }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
        $vis struct $name;

        impl $crate::materialisation::Materialisation for $name {
            type Context = $ctx;

            fn index_set() -> $crate::materialisation::IndexSet<$ctx> {
                $crate::materialisation::IndexSet::new()
                    $(.with::<$index>())+
            }

            const INDEXES: &'static [$crate::materialisation::IndexId] = &[
                $(<$index as $crate::materialisation::IndexDef>::NAME),+
            ];
        }

        $(
            impl $crate::materialisation::Builds<$index> for $name {}
        )+
    };
}
