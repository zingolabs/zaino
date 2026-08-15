//! Where a transaction lives in the chain.

use super::Height;

/// Where in the chain a transaction was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransactionLocation {
    /// In the best chain at this height.
    BestChain(Height),
    /// In a non-best chain branch (orphaned).
    NonBestChain,
    /// In the mempool (not yet mined).
    Mempool,
}
