//! Why bytes did not decode into a block or transaction, one variant per
//! rejection.

use zaino_primitives::types::{
    BlockError, CompactDifficultyError, HeightOverflow, SignedZatoshisOverflow, ZatoshisOverflow,
};

/// Bytes that do not decode into a domain block or transaction.
#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    /// The encoding ended before a field did.
    #[error("truncated: {needed} more bytes needed, {available} available")]
    Truncated {
        /// Bytes the field still needed.
        needed: usize,
        /// Bytes left in the input.
        available: usize,
    },
    /// A compact-size prefix used a longer form than its value needs.
    #[error("non-canonical compact-size prefix")]
    NonCanonicalCompactSize,
    /// A compact-size prefix exceeds the protocol's maximum.
    #[error("compact-size prefix {0} exceeds the protocol maximum")]
    CompactSizeTooLarge(u64),
    /// Bytes remain after the last field.
    #[error("{0} trailing bytes after the encoding")]
    Trailing(usize),
    /// The version word names no transaction format.
    #[error("unknown transaction format: header 0x{header:08x}, version group {group:x?}")]
    UnknownTransactionVersion {
        /// The version word as read, overwinter flag included.
        header: u32,
        /// The version group id that followed an overwintered version word.
        group: Option<u32>,
    },
    /// The block holds no transactions.
    #[error("block has no transactions")]
    NoTransactions,
    /// The first transaction does not have exactly one coinbase input.
    #[error("the first transaction is not a coinbase")]
    NoCoinbase,
    /// The coinbase script does not open with a canonical height push.
    #[error("coinbase height: {0}")]
    CoinbaseHeight(&'static str),
    /// The block header's version is negative.
    #[error("negative block header version {0}")]
    NegativeHeaderVersion(i32),
    /// The equihash solution has neither the standard nor the regtest length.
    #[error("equihash solution of {0} bytes is neither the standard 1344 nor regtest's 36")]
    Solution(usize),
    /// The coinbase height is outside the domain's range.
    #[error("coinbase height out of range")]
    Height(#[from] HeightOverflow),
    /// The header's difficulty target is malformed.
    #[error("difficulty target")]
    Difficulty(#[from] CompactDifficultyError),
    /// A transparent output's value is out of range.
    #[error("transparent output value")]
    OutputValue(#[from] ZatoshisOverflow),
    /// A pool's value balance is out of range.
    #[error("value balance")]
    ValueBalance(#[from] SignedZatoshisOverflow),
    /// The decoded parts do not assemble into a block.
    #[error("block")]
    Block(#[from] BlockError),
}
