//! Local validation for raw transaction submission.

use crate::jsonrpsee::connector::RpcError;
use zebra_chain::block::MAX_BLOCK_BYTES;
use zebra_rpc::server::error::LegacyCode;

/// Validates that `bytes` does not exceed the Zcash protocol transaction size limit.
// `RpcError` is a large error type, so returning it by value trips `clippy::result_large_err`.
// Suppressed here to avoid a boxing refactor that would ripple through every call site.
#[allow(clippy::result_large_err)]
pub fn validate_raw_transaction_bytes(bytes: &[u8]) -> Result<(), RpcError> {
    if bytes.len() > MAX_BLOCK_BYTES as usize {
        return Err(RpcError::new_from_legacycode(
            LegacyCode::InvalidParameter,
            format!(
                "transaction size {} bytes exceeds maximum allowed size of {MAX_BLOCK_BYTES} bytes",
                bytes.len(),
            ),
        ));
    }
    Ok(())
}

/// Validates hex encoding and decoded transaction size before forwarding to a validator.
#[allow(clippy::result_large_err)]
pub fn validate_raw_transaction_hex(raw_transaction_hex: &str) -> Result<(), RpcError> {
    let bytes = hex::decode(raw_transaction_hex)
        .map_err(|_| RpcError::new_from_legacycode(LegacyCode::InvalidParameter, "invalid hex"))?;
    validate_raw_transaction_bytes(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_hex() {
        let err = validate_raw_transaction_hex("notahexstring").unwrap_err();
        assert_eq!(err.code, LegacyCode::InvalidParameter as i64);
        assert_eq!(err.message, "invalid hex");
    }

    #[test]
    fn rejects_odd_length_hex() {
        let err = validate_raw_transaction_hex("abc").unwrap_err();
        assert_eq!(err.code, LegacyCode::InvalidParameter as i64);
        assert_eq!(err.message, "invalid hex");
    }

    #[test]
    fn rejects_oversized_decoded_transaction() {
        let oversized = vec![0u8; MAX_BLOCK_BYTES as usize + 1];
        let hex_str = hex::encode(oversized);
        let err = validate_raw_transaction_hex(&hex_str).unwrap_err();
        assert_eq!(err.code, LegacyCode::InvalidParameter as i64);
        assert!(err.message.contains("exceeds maximum"));
    }

    #[test]
    fn accepts_max_size_transaction() {
        let max_size = vec![0u8; MAX_BLOCK_BYTES as usize];
        let hex_str = hex::encode(max_size);
        validate_raw_transaction_hex(&hex_str).unwrap();
    }

    #[test]
    fn validate_raw_transaction_bytes_rejects_oversized() {
        let oversized = vec![0u8; MAX_BLOCK_BYTES as usize + 1];
        let err = validate_raw_transaction_bytes(&oversized).unwrap_err();
        assert_eq!(err.code, LegacyCode::InvalidParameter as i64);
        assert!(err.message.contains("exceeds maximum"));
    }
}
