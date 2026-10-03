//! A whole block decoded into its transactions, each with its envelope detail.
//!
//! The indexing [`Block`](super::Block) keeps only what an index keys on and
//! drops the per-transaction facts the explorer surface needs (the envelope, the
//! coinbase input, the Sprout pool values — see [`TransactionDetail`]). This
//! shape is the explorer's: every transaction paired with its
//! [`TransactionDetail`], decoded from a single raw-block fetch so the whole
//! block is one consistent snapshot rather than a transaction at a time.

use super::{Transaction, TransactionDetail};

/// One transaction paired with the envelope facts the indexing shape drops.
///
/// Carries no location: a transaction's slot is the block's to know, and this
/// pairing only ever travels inside a [`DecodedBlock`], which is the block.
#[derive(Debug, Clone)]
pub struct DetailedTransaction {
    /// The transaction, decomposed by pool.
    pub transaction: Transaction,
    /// The envelope, coinbase input, and Sprout values the indexing shape
    /// ([`transaction`](Self::transaction)) drops.
    pub detail: TransactionDetail,
    /// The transaction's raw consensus bytes, carried so the explorer's `hex`
    /// field renders from the same decode rather than a refetch.
    pub raw: Vec<u8>,
}

/// A block decoded into every transaction with its detail.
///
/// The product of one raw-block fetch: [`size`](Self::size) is the block's
/// serialized byte length and [`transactions`](Self::transactions) are its
/// transactions in block order, so the coinbase is first.
#[derive(Debug, Clone)]
pub struct DecodedBlock {
    /// Serialized byte length of the whole block.
    pub size: u64,
    /// The block's transactions, in block order.
    pub transactions: Vec<DetailedTransaction>,
}
