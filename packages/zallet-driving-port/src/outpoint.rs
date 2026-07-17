//! A transparent transaction output's identity.

use zaino_primitives::types::{OutputIndex, TransactionHash};

/// A transparent output identified by the transaction that created it
/// and the output's index within that transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Outpoint {
    /// The transaction that created the output.
    pub txid: TransactionHash,
    /// The output's index within that transaction.
    pub index: OutputIndex,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_txid_different_index_are_distinct() {
        let txid = TransactionHash::from([7u8; 32]);
        let a = Outpoint { txid, index: 0 };
        let b = Outpoint { txid, index: 1 };
        assert_ne!(a, b);
    }
}
