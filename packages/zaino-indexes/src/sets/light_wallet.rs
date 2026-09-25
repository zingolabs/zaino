//! The light-wallet materialisation: exactly the indexes compact-block serving
//! composes on.
//!
//! A lightwalletd-shaped deployment reads compact blocks locally and passes
//! everything else (treestate, raw transactions, transparent history) through
//! to the validator, so its finalised store builds nothing beyond the
//! compact-block set. Address history and spend indexes are *not* built here —
//! not as an omission but as the materialisation's statement: a store over
//! [`LightWallet`] has no local address read, and a use case that wants one
//! cannot be wired over it.
//!
//! Provisioned from the same [`CurrentZainoContext`] as the full set — a
//! subset of its indexes, each projecting from the same context.

use super::current_zaino::CurrentZainoContext;
use crate::indexes::chain_metadata::ChainMetadataIndex;
use crate::indexes::hash_to_height::HashToHeightIndex;
use crate::indexes::headers::HeadersIndex;
use crate::indexes::ironwood::IronwoodIndex;
use crate::indexes::orchard::OrchardIndex;
use crate::indexes::sapling::SaplingIndex;
use crate::indexes::transparent_data::TransparentDataIndex;
use crate::indexes::txids::TxidsIndex;
use crate::materialisation;

materialisation! {
    /// Compact-block serving only: headers, txids, hash→height, the per-pool
    /// compact data, and the cumulative tree sizes.
    pub struct LightWallet over CurrentZainoContext {
        HeadersIndex,
        TxidsIndex,
        HashToHeightIndex,
        TransparentDataIndex,
        SaplingIndex,
        OrchardIndex,
        IronwoodIndex,
        ChainMetadataIndex,
    }
}
