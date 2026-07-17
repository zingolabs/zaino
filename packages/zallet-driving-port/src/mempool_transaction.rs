//! A mempool transaction, tagged with the tip it was validated
//! against.

use zaino_primitives::types::{ConsensusBranchId, TransactionHash};

use crate::block_id::BlockId;
use crate::raw::RawTransaction;

/// A transaction awaiting mining, as the mempool stream delivers it.
///
/// Consensus binds a mempool transaction to chain *state* — anchors,
/// nullifiers, prevouts, expiry, branch id — never to a block, so its
/// view is coherent only relative to the tip it was validated
/// against, and every delivery carries that tag (ADR 0001).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MempoolTransaction {
    /// The transaction, consensus-serialized.
    pub raw: RawTransaction,
    /// The transaction's txid.
    pub txid: TransactionHash,
    /// The consensus branch id the transaction was validated under.
    pub branch_id: ConsensusBranchId,
    /// The chain tip the transaction was validated against.
    pub validated_against: BlockId,
}
