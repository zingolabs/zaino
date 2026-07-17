//! A transaction as mined in the pinned best chain.

use zaino_primitives::types::{BlockTime, ConsensusBranchId};

use crate::block_id::BlockId;
use crate::raw::RawTransaction;

/// A transaction as mined in the pinned best chain: its consensus
/// serialization plus the context a consumer needs to parse and place
/// it.
///
/// The branch id names the consensus rules in force where the
/// transaction was mined — deserializing the raw bytes requires it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MinedTransaction {
    /// The transaction, consensus-serialized.
    pub raw: RawTransaction,
    /// The consensus branch id in force at the mined height.
    pub branch_id: ConsensusBranchId,
    /// The block the transaction was mined in.
    pub mined_at: BlockId,
    /// The header time of that block.
    pub block_time: BlockTime,
}
