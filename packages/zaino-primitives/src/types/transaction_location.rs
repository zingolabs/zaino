//! Where a transaction lives in the chain

use zcash_protocol::consensus::BlockHeight;

/// `NonBestChain` = orphaned branch
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransactionLocation {
    BestChain(BlockHeight),
    NonBestChain,
    Mempool,
}
