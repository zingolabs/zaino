//! The locally backed capabilities, each declared **once** with the indexes it
//! composes from.
//!
//! One declaration per capability yields both facts a store needs:
//!
//! - the **type-level** bound a serving read puts on its materialisation —
//!   `M: Backs<Blocks>` — so a store lacking one of the indexes does not have
//!   the read;
//! - the **runtime** list the serviceability manifest checks stamps for —
//!   `Blocks::INDEXES`.
//!
//! Because both come from the same list, the read a store *has* and the read
//! its manifest *advertises* cannot disagree about which indexes they need.
//!
//! ```text
//! has(M, C)        ⟺  indexes(C) ⊆ built(M)          checked by rustc
//! advertises(M, C) ⟺  indexes(C) ⊆ stamped(backend)  checked at snapshot
//! ```
//!
//! The grain is per capability, not per index: a capability composes several
//! indexes, and one index (headers) serves several capabilities, so "which
//! capability does this index enable" has no single answer — "which indexes
//! does this capability need" does.

use zaino_core::Capability;
use zaino_sync::primitives::IndexId;
use zaino_sync::traits::IndexDef;

use crate::indexes::address_history::AddressHistoryIndex;
use crate::indexes::chain_metadata::ChainMetadataIndex;
use crate::indexes::hash_to_height::HashToHeightIndex;
use crate::indexes::headers::HeadersIndex;
use crate::indexes::ironwood::IronwoodIndex;
use crate::indexes::orchard::OrchardIndex;
use crate::indexes::sapling::SaplingIndex;
use crate::indexes::transparent_data::TransparentDataIndex;
use crate::indexes::transparent_spends::TransparentSpendsIndex;
use crate::indexes::txid_location::TxidLocationIndex;
use crate::indexes::txids::TxidsIndex;
use crate::materialisation::{Builds, Materialisation};

/// A capability the finalised store can back, as a type carrying the indexes
/// it composes from.
pub trait LocalCapability: Send + Sync + 'static {
    /// The capability this declares.
    const CAPABILITY: Capability;
    /// Every index a read of this capability composes from.
    const INDEXES: &'static [IndexId];
}

/// Type-level: the materialisation `M` builds every index `C` needs.
///
/// Blanket-implemented from the capability's declaration, so a serving read
/// bounds on one name rather than repeating the index list.
pub trait Backs<C: LocalCapability>: Materialisation {}

/// Declare a locally backed capability from its index list.
macro_rules! local_capability {
    (
        $(#[$meta:meta])*
        $name:ident = $variant:ident backed by [$($index:ty),+ $(,)?]
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
        pub struct $name;

        impl LocalCapability for $name {
            const CAPABILITY: Capability = Capability::$variant;
            const INDEXES: &'static [IndexId] = &[$(<$index as IndexDef>::NAME),+];
        }

        impl<M> Backs<$name> for M where M: Materialisation $(+ Builds<$index>)+ {}
    };
}

local_capability! {
    /// A compact block is composed from the granular per-pool indexes; by-hash
    /// access adds the hash→height index. Every pool the compact read requires
    /// is listed, ironwood included — a manifest that omitted one would
    /// advertise a read that fails on a store lacking it.
    Blocks = Blocks backed by [
        HeadersIndex,
        TxidsIndex,
        HashToHeightIndex,
        TransparentDataIndex,
        SaplingIndex,
        OrchardIndex,
        IronwoodIndex,
        ChainMetadataIndex,
    ]
}

local_capability! {
    /// Where a transaction was mined — a local lookup. Its raw bytes are a
    /// *separate* capability (`RawTransaction`), with no local index.
    TransactionLocation = TransactionLocation backed by [TxidLocationIndex]
}

local_capability! {
    /// Transparent address history.
    AddressHistory = AddressHistory backed by [AddressHistoryIndex]
}

local_capability! {
    /// Whether, and where, an outpoint was spent.
    SpendStatus = SpendStatus backed by [TransparentSpendsIndex]
}
