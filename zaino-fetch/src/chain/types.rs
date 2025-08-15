//! Common type definitions for block and transaction parsing.

/// A transaction ID in its canonical fixed-size format
pub type TxId = [u8; 32];

/// A transaction ID as bytes (for RPC compatibility and serialization)
pub type TxIdBytes = Vec<u8>;

/// Multiple transaction IDs from RPC responses
pub type TxIdList = Vec<TxIdBytes>;

/// Multiple transaction IDs in canonical format
pub type CanonicalTxIdList = Vec<TxId>;

/// Convert RPC transaction ID list to canonical format
pub fn txid_list_to_canonical(txid_list: TxIdList) -> Result<CanonicalTxIdList, crate::error::ParseError> {
    txid_list
        .into_iter()
        .map(|txid_bytes| {
            if txid_bytes.len() != 32 {
                return Err(crate::error::ParseError::InvalidData(format!(
                    "Transaction ID must be 32 bytes, got {}", txid_bytes.len()
                )));
            }
            let mut txid = [0u8; 32];
            txid.copy_from_slice(&txid_bytes);
            Ok(txid)
        })
        .collect()
}

/// Convert a single transaction ID bytes to canonical format
pub fn txid_bytes_to_canonical(txid_bytes: TxIdBytes) -> Result<TxId, crate::error::ParseError> {
    if txid_bytes.len() != 32 {
        return Err(crate::error::ParseError::InvalidData(format!(
            "Transaction ID must be 32 bytes, got {}", txid_bytes.len()
        )));
    }
    let mut txid = [0u8; 32];
    txid.copy_from_slice(&txid_bytes);
    Ok(txid)
}

/// Convert hex string to canonical txid format
pub fn hex_to_txid(hex_string: &str) -> Result<TxId, crate::error::ParseError> {
    let bytes = hex::decode(hex_string)
        .map_err(|e| crate::error::ParseError::InvalidData(format!("Invalid hex: {}", e)))?;
    
    if bytes.len() != 32 {
        return Err(crate::error::ParseError::InvalidData(format!(
            "Transaction ID must be 32 bytes, got {}", bytes.len()
        )));
    }
    
    let mut txid = [0u8; 32];
    txid.copy_from_slice(&bytes);
    // Convert from big-endian (RPC) to little-endian (internal)
    txid.reverse();
    Ok(txid)
}

/// Convert list of hex strings to canonical format
pub fn hex_list_to_canonical(hex_list: Vec<String>) -> Result<CanonicalTxIdList, crate::error::ParseError> {
    hex_list
        .into_iter()
        .map(|hex| hex_to_txid(&hex))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_txid_list_conversion() {
        let txid_list = vec![
            vec![1u8; 32],
            vec![2u8; 32],
        ];
        
        let canonical = txid_list_to_canonical(txid_list).unwrap();
        assert_eq!(canonical.len(), 2);
        assert_eq!(canonical[0], [1u8; 32]);
        assert_eq!(canonical[1], [2u8; 32]);
    }

    #[test]
    fn test_invalid_txid_length() {
        let invalid_list = vec![vec![1u8; 30]]; // Wrong length
        let result = txid_list_to_canonical(invalid_list);
        assert!(result.is_err());
    }

    #[test]
    fn test_hex_conversion() {
        let hex = "0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f20";
        let txid = hex_to_txid(hex).unwrap();
        
        // Should be reversed (big-endian to little-endian)
        let expected = [
            0x20, 0x1f, 0x1e, 0x1d, 0x1c, 0x1b, 0x1a, 0x19,
            0x18, 0x17, 0x16, 0x15, 0x14, 0x13, 0x12, 0x11,
            0x10, 0x0f, 0x0e, 0x0d, 0x0c, 0x0b, 0x0a, 0x09,
            0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01
        ];
        assert_eq!(txid, expected);
    }
}