//! Where the pinned view places a transaction.

use crate::block_id::BlockId;

/// Where the pinned view places a transaction.
///
/// The status speaks only of chain state. There is deliberately no
/// mempool variant: a snapshot answers about the chain it pinned, and
/// mempool presence is learned from the port's mempool surface, which
/// ADR 0001 keeps apart from chain state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionStatus {
    /// Mined in the pinned best chain, in this block.
    MinedAt(BlockId),
    /// Known, but only on a non-best branch (orphaned by a reorg).
    NotInBestChain,
    /// Not known to the pinned view.
    Unknown,
}
