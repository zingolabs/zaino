//! Consensus-derived constants with a single source of truth.
//!
//! Each value derives from zebra's authoritative upstream constant; nothing else in
//! the workspace should hard-code these — reference this module instead.

/// Number of confirmations before a coinbase output becomes spendable.
///
/// Single source of truth, from zebra's transparent-coinbase maturity rule.
pub const COINBASE_MATURITY: u32 = zebra_chain::transparent::MIN_TRANSPARENT_COINBASE_MATURITY;

/// Distance below the best-chain tip of the finalised / non-finalised seam: a block
/// buried deeper than this is finalised (reorg-stable).
///
/// Derived from zebra's protocol reorg limit (`MAX_BLOCK_REORG_HEIGHT`). The `+ 1`
/// accounts for the tip block itself, preserving the historical seam semantics.
pub const MAX_NONFINALISED_DEPTH: u32 =
    zebra_chain::parameters::constants::MAX_BLOCK_REORG_HEIGHT + 1;

/// A tractable one-tenth of [`MAX_NONFINALISED_DEPTH`], for fast tests that need a
/// finalised seam without building a full ~[`MAX_NONFINALISED_DEPTH`]-block chain.
///
/// Integer division, so this is `100` when the real depth is `1001`. Test-only: it
/// lets in-crate tests select a shallow seam that still derives from the single
/// source of truth rather than a hard-coded literal.
pub const FAST_TEST_MAX_NONFINALISED_DEPTH: u32 = MAX_NONFINALISED_DEPTH / 10;

/// Why a client's raw transaction was rejected before it reached a validator.
///
/// A local rejection: nothing here needs the chain, only the protocol's size
/// limit and hex encoding. Callers map it onto whatever their interface's error
/// vocabulary is — the zcashd legacy codes live in the serving layer, not here.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RawTransactionError {
    /// The submitted string is not valid hex.
    #[error("invalid hex")]
    InvalidHex,

    /// The decoded transaction exceeds the protocol's maximum size.
    #[error(
        "transaction size {size} bytes exceeds maximum allowed size of {MAX_BLOCK_BYTES} bytes"
    )]
    TooLarge {
        /// Size of the decoded transaction, in bytes.
        size: usize,
    },
}

/// Maximum serialised size of a transaction, in bytes.
///
/// A transaction must fit in a block, so the block limit is the transaction
/// limit. From zebra's authoritative constant.
pub const MAX_BLOCK_BYTES: u64 = zebra_chain::block::MAX_BLOCK_BYTES;

/// Validates that `bytes` does not exceed the protocol transaction size limit.
pub fn validate_raw_transaction_bytes(bytes: &[u8]) -> Result<(), RawTransactionError> {
    if bytes.len() > MAX_BLOCK_BYTES as usize {
        return Err(RawTransactionError::TooLarge { size: bytes.len() });
    }
    Ok(())
}

/// Validates hex encoding and decoded transaction size before forwarding to a
/// validator.
///
/// Cheap and local, so it runs before the network round trip: an oversized or
/// malformed submission is rejected here rather than costing a validator call.
pub fn validate_raw_transaction_hex(raw_transaction_hex: &str) -> Result<(), RawTransactionError> {
    let bytes = hex::decode(raw_transaction_hex).map_err(|_| RawTransactionError::InvalidHex)?;
    validate_raw_transaction_bytes(&bytes)
}

#[cfg(test)]
mod raw_transaction_tests {
    use super::*;

    #[test]
    fn rejects_invalid_hex() {
        assert_eq!(
            validate_raw_transaction_hex("notahexstring"),
            Err(RawTransactionError::InvalidHex)
        );
    }

    /// An odd-length string is not a rejection of the *transaction* but of the
    /// encoding, and must report as such — a client that sent a truncated hex
    /// string needs to know that, not that its transaction was too big.
    #[test]
    fn rejects_odd_length_hex() {
        assert_eq!(
            validate_raw_transaction_hex("abc"),
            Err(RawTransactionError::InvalidHex)
        );
    }

    #[test]
    fn rejects_oversized_decoded_transaction() {
        let oversized = hex::encode(vec![0u8; MAX_BLOCK_BYTES as usize + 1]);

        assert_eq!(
            validate_raw_transaction_hex(&oversized),
            Err(RawTransactionError::TooLarge {
                size: MAX_BLOCK_BYTES as usize + 1
            })
        );
    }

    /// The limit is inclusive: a transaction of exactly the maximum size is
    /// valid, and rejecting it would refuse a transaction the network accepts.
    #[test]
    fn accepts_max_size_transaction() {
        let max_size = hex::encode(vec![0u8; MAX_BLOCK_BYTES as usize]);

        assert_eq!(validate_raw_transaction_hex(&max_size), Ok(()));
    }

    #[test]
    fn validate_raw_transaction_bytes_rejects_oversized() {
        let oversized = vec![0u8; MAX_BLOCK_BYTES as usize + 1];

        assert!(validate_raw_transaction_bytes(&oversized).is_err());
    }
}
