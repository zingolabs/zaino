//! The index set a deployment builds to answer transparent history locally.
//!
//! Compact-block serving, plus the four indexes a local address read composes
//! from. A light-wallet deployment that places address history [`Local`] rather
//! than relaying it to the validator builds this set; one that relays it builds
//! [`CompactBlocks`](super::compact_blocks::CompactBlocks) and nothing more.
//!
//! The difference is a privacy choice, which is why it is a second set and not
//! a knob: relaying address reads discloses the queried addresses to the
//! validator, and a store over the smaller set has no local address read for a
//! deployment to accidentally claim.
//!
//! Provisioned from the same [`CurrentZainoContext`] as the other sets — a
//! superset of the compact-block indexes, each projecting from the same
//! context.
//!
//! [`Local`]: https://docs.rs/zaino-core

use super::current_zaino::CurrentZainoContext;
use crate::index_set;
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

index_set! {
    /// Compact-block serving plus the transparent history indexes: address
    /// receives, the spends that consume them, and the transaction locations a
    /// spend is reported at.
    pub struct TransparentHistory over CurrentZainoContext {
        HeadersIndex,
        TxidsIndex,
        HashToHeightIndex,
        TransparentDataIndex,
        SaplingIndex,
        OrchardIndex,
        IronwoodIndex,
        ChainMetadataIndex,
        AddressHistoryIndex,
        TransparentSpendsIndex,
        TxidLocationIndex,
    }
}
