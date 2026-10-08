//! The index set a node-RPC / explorer deployment builds to answer the
//! explorer's local reads — transparent history, spend lookups and commitment
//! treestate — without a validator round trip.
//!
//! [`TransparentHistory`]'s indexes — compact-block serving plus the transparent
//! history a local address read and a local spend lookup compose from — plus the
//! `tree_state` index, which serves `z_gettreestate` from zaino's own
//! commitment-tree frontiers, and the three per-pool subtree-roots indexes
//! (`subtrees_{sapling,orchard,ironwood}`) that back `z_getsubtreesbyindex`.
//!
//! The list mirrors [`LightWalletLocal`](super::light_wallet_local::LightWalletLocal):
//! both deployments serve treestate and subtree roots locally, so both build the
//! same commitment-tree indexes over the transparent-history base. It is a
//! distinct set, not a shared alias, because the set is a property of the
//! deployment and the two could diverge (a node-RPC set could gain an
//! explorer-only index a wallet never needs). The `index_set!` macro declares a
//! set from one explicit index list and does not compose one set into another, so
//! the shared content is stated once per set.
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
use crate::indexes::subtrees::{IronwoodSubtreesIndex, OrchardSubtreesIndex, SaplingSubtreesIndex};
use crate::indexes::transparent_data::TransparentDataIndex;
use crate::indexes::transparent_spends::TransparentSpendsIndex;
use crate::indexes::tree_state::TreeStateIndex;
use crate::indexes::txid_location::TxidLocationIndex;
use crate::indexes::txids::TxidsIndex;

index_set! {
    /// Transparent history serving plus the local commitment-tree (treestate)
    /// index: everything [`TransparentHistory`](super::transparent_history::TransparentHistory)
    /// builds, and `tree_state` plus the per-pool `subtrees_*` indexes for local
    /// `z_gettreestate` / `z_getsubtreesbyindex` serving.
    pub struct NodeRpcLocal over CurrentZainoContext {
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
        SaplingSubtreesIndex,
        OrchardSubtreesIndex,
        IronwoodSubtreesIndex,
    }
}
