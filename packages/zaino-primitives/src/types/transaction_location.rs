//! Where a transaction lives in the chain

use super::Height;

/// `NonBestChain` = orphaned branch
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransactionLocation {
    BestChain(Height),
    NonBestChain,
    Mempool,
}
