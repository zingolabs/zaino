//! The pinned view a snapshot serves.

use crate::find_fork_point::FindForkPoint;
use crate::get_address_transaction_ids::GetAddressTransactionIds;
use crate::get_address_unspent_outpoints::GetAddressUnspentOutpoints;
use crate::get_mined_transaction::GetMinedTransaction;
use crate::get_outpoint_spend_status::GetOutpointSpendStatus;
use crate::get_raw_block::GetRawBlock;
use crate::get_raw_block_header::GetRawBlockHeader;
use crate::get_transaction_status::GetTransactionStatus;
use crate::get_treestate::GetTreestate;
use crate::hash_for_height::GetHashForHeight;
use crate::height_for_hash::GetHeightForHash;
use crate::pinned_tip::GetPinnedTip;
use crate::stream_raw_blocks::StreamRawBlocks;

/// The full pinned-read surface a snapshot serves.
///
/// One umbrella over the single-capability traits, so consumers like
/// Zallet bound one type parameter instead of listing every
/// capability. Cloning is part of the contract: clones share the
/// pinned view, and the view stays readable while any clone lives.
///
/// Blanket-implemented — implementations write the capability traits
/// and receive this for free.
pub trait ChainSnapshot:
    GetPinnedTip
    + GetHashForHeight
    + GetHeightForHash
    + FindForkPoint
    + GetRawBlock
    + GetRawBlockHeader
    + StreamRawBlocks
    + GetMinedTransaction
    + GetTransactionStatus
    + GetTreestate
    + GetAddressUnspentOutpoints
    + GetAddressTransactionIds
    + GetOutpointSpendStatus
    + Clone
    + Send
    + Sync
    + 'static
{
}

impl<T> ChainSnapshot for T where
    T: GetPinnedTip
        + GetHashForHeight
        + GetHeightForHash
        + FindForkPoint
        + GetRawBlock
        + GetRawBlockHeader
        + StreamRawBlocks
        + GetMinedTransaction
        + GetTransactionStatus
        + GetTreestate
        + GetAddressUnspentOutpoints
        + GetAddressTransactionIds
        + GetOutpointSpendStatus
        + Clone
        + Send
        + Sync
        + 'static
{
}
