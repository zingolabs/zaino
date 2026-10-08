//! The index set a light-wallet deployment builds to answer a wallet's full
//! sync critical path locally.
//!
//! [`TransparentHistory`]'s indexes — compact-block serving plus the transparent
//! history a local address read composes from — plus the `tree_state` index,
//! which serves `GetTreeState` / `GetLatestTreeState` from zaino's own
//! commitment-tree frontiers rather than a validator round trip per scan batch.
//! Task 7 adds the per-pool subtree-roots indexes; Task 8 flips a light-wallet
//! deployment's routing onto this set.
//!
//! Provisioned from the same [`CurrentZainoContext`] as the other sets: the
//! tree-state index projects its per-pool note commitments from the context the
//! compact-block indexes already carry.
//!
//! [`TransparentHistory`]: super::transparent_history::TransparentHistory
//! [`CurrentZainoContext`]: super::current_zaino::CurrentZainoContext

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
use crate::indexes::tree_state::TreeStateIndex;
use crate::indexes::txid_location::TxidLocationIndex;
use crate::indexes::txids::TxidsIndex;

index_set! {
    /// Transparent history serving plus the local commitment-tree (treestate)
    /// index: everything [`TransparentHistory`](super::transparent_history::TransparentHistory)
    /// builds, and `tree_state` for local `GetTreeState` serving.
    pub struct LightWalletLocal over CurrentZainoContext {
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
        TreeStateIndex,
    }
}
