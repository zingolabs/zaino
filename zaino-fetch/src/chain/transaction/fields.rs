//! Transaction field implementations.
//!
//! This module contains self-contained field types that implement the
//! TransactionField trait. Each field knows how to read itself from
//! a cursor and perform field-specific validation.

use std::io::Cursor;
use crate::{
    error::{ParseError, ValidationError},
    utils::{read_u32, read_i64, CompactSize},
};
use super::{
    reader::{TransactionField, FieldSize},
    context::TransactionContext,
};

/// Version Group ID field (4 bytes, present in v3+)
pub struct VersionGroupId;

impl TransactionField for VersionGroupId {
    type Value = u32;
    const SIZE: FieldSize = FieldSize::Fixed(4);
    
    fn read_from_cursor(cursor: &mut Cursor<&[u8]>) -> Result<Self::Value, ParseError> {
        read_u32(cursor, "VersionGroupId")
    }
    
    fn validate(value: &Self::Value, context: &TransactionContext) -> Result<(), ParseError> {
        // Version group ID validation depends on transaction version
        // We'll need to extract version from context or peek at header
        // For now, we'll do basic validation
        let expected = match context.block_context.height {
            h if h >= context.block_context.activation_heights.nu5 => 0x26A7270A, // v5
            h if h >= context.block_context.activation_heights.sapling => 0x892F2085, // v4  
            h if h >= context.block_context.activation_heights.overwinter => 0x03C48270, // v3
            _ => return Ok(()), // Not applicable for older versions
        };
        
        if *value != expected {
            return Err(ParseError::Validation(ValidationError::InvalidVersionGroupId {
                expected,
                found: *value,
            }));
        }
        
        Ok(())
    }
}

/// Lock Time field (4 bytes, present in all versions)
pub struct LockTime;

impl TransactionField for LockTime {
    type Value = u32;
    const SIZE: FieldSize = FieldSize::Fixed(4);
    
    fn read_from_cursor(cursor: &mut Cursor<&[u8]>) -> Result<Self::Value, ParseError> {
        read_u32(cursor, "LockTime")
    }
}

/// Expiry Height field (4 bytes, present in v3+)
pub struct ExpiryHeight;

impl TransactionField for ExpiryHeight {
    type Value = u32;
    const SIZE: FieldSize = FieldSize::Fixed(4);
    
    fn read_from_cursor(cursor: &mut Cursor<&[u8]>) -> Result<Self::Value, ParseError> {
        read_u32(cursor, "ExpiryHeight")
    }
    
    fn validate(value: &Self::Value, context: &TransactionContext) -> Result<(), ParseError> {
        // Expiry height should be reasonable relative to current block
        if *value > 0 && *value <= context.block_context.height {
            return Err(ParseError::Validation(ValidationError::Generic {
                message: format!(
                    "Expiry height {} is not greater than current block height {}",
                    value,
                    context.block_context.height
                ),
            }));
        }
        Ok(())
    }
}

/// Sapling Value Balance field (8 bytes, present in v4+)
pub struct ValueBalanceSapling;

impl TransactionField for ValueBalanceSapling {
    type Value = i64;
    const SIZE: FieldSize = FieldSize::Fixed(8);
    
    fn read_from_cursor(cursor: &mut Cursor<&[u8]>) -> Result<Self::Value, ParseError> {
        read_i64(cursor, "ValueBalanceSapling")
    }
}

/// Simple transaction input representation
#[derive(Debug, Clone)]
pub struct TxIn {
    pub prev_txid: [u8; 32],
    pub prev_index: u32,
    pub script: Vec<u8>,
}

impl TxIn {
    pub fn is_coinbase(&self) -> bool {
        self.prev_txid == [0; 32] && self.prev_index == 0xFFFFFFFF
    }
    
    fn read_from_cursor(cursor: &mut Cursor<&[u8]>) -> Result<Self, ParseError> {
        use crate::utils::{read_bytes, read_u32};
        
        // Read previous transaction hash (32 bytes)
        let prev_txid_bytes = read_bytes(cursor, 32, "TxIn::prev_txid")?;
        let mut prev_txid = [0u8; 32];
        prev_txid.copy_from_slice(&prev_txid_bytes);
        
        // Read previous output index (4 bytes)
        let prev_index = read_u32(cursor, "TxIn::prev_index")?;
        
        // Read script length and script
        let script_len = CompactSize::read(cursor)? as usize;
        let script = read_bytes(cursor, script_len, "TxIn::script")?;
        
        // Skip sequence number (4 bytes) - we don't use it
        let _sequence = read_u32(cursor, "TxIn::sequence")?;
        
        Ok(TxIn {
            prev_txid,
            prev_index,
            script,
        })
    }
}

/// Transparent Inputs field (CompactSize count + array of TxIn)
pub struct TransparentInputs;

impl TransactionField for TransparentInputs {
    type Value = Vec<TxIn>;
    const SIZE: FieldSize = FieldSize::CompactSizeArray;
    
    fn read_from_cursor(cursor: &mut Cursor<&[u8]>) -> Result<Self::Value, ParseError> {
        let count = CompactSize::read(cursor)? as usize;
        let mut inputs = Vec::with_capacity(count);
        
        for _ in 0..count {
            inputs.push(TxIn::read_from_cursor(cursor)?);
        }
        
        Ok(inputs)
    }
    
    fn validate(value: &Self::Value, context: &TransactionContext) -> Result<(), ParseError> {
        // Coinbase transaction must have exactly one input
        if context.is_coinbase() {
            if value.len() != 1 {
                return Err(ParseError::Validation(ValidationError::Generic {
                    message: format!(
                        "Coinbase transaction must have exactly 1 input, found {}",
                        value.len()
                    ),
                }));
            }
            
            if !value[0].is_coinbase() {
                return Err(ParseError::Validation(ValidationError::Generic {
                    message: "First transaction in block must be coinbase".to_string(),
                }));
            }
        } else {
            // Non-coinbase transactions cannot have coinbase inputs
            for input in value {
                if input.is_coinbase() {
                    return Err(ParseError::Validation(ValidationError::Generic {
                        message: "Non-coinbase transaction cannot have coinbase inputs".to_string(),
                    }));
                }
            }
        }
        
        Ok(())
    }
}

/// Simple transaction output representation
#[derive(Debug, Clone)]
pub struct TxOut {
    pub value: i64,
    pub script: Vec<u8>,
}

impl TxOut {
    fn read_from_cursor(cursor: &mut Cursor<&[u8]>) -> Result<Self, ParseError> {
        use crate::utils::read_bytes;
        
        // Read value (8 bytes)
        let value = read_i64(cursor, "TxOut::value")?;
        
        // Read script length and script
        let script_len = CompactSize::read(cursor)? as usize;
        let script = read_bytes(cursor, script_len, "TxOut::script")?;
        
        Ok(TxOut { value, script })
    }
}

/// Transparent Outputs field (CompactSize count + array of TxOut)
pub struct TransparentOutputs;

impl TransactionField for TransparentOutputs {
    type Value = Vec<TxOut>;
    const SIZE: FieldSize = FieldSize::CompactSizeArray;
    
    fn read_from_cursor(cursor: &mut Cursor<&[u8]>) -> Result<Self::Value, ParseError> {
        let count = CompactSize::read(cursor)? as usize;
        let mut outputs = Vec::with_capacity(count);
        
        for _ in 0..count {
            outputs.push(TxOut::read_from_cursor(cursor)?);
        }
        
        Ok(outputs)
    }
    
    fn validate(value: &Self::Value, _context: &TransactionContext) -> Result<(), ParseError> {
        // Validate output values are non-negative
        for (i, output) in value.iter().enumerate() {
            if output.value < 0 {
                return Err(ParseError::Validation(ValidationError::Generic {
                    message: format!("Output {} has negative value: {}", i, output.value),
                }));
            }
        }
        
        Ok(())
    }
}

/// Placeholder for Sapling Spends field (will need proper implementation)
pub struct ShieldedSpends;

impl TransactionField for ShieldedSpends {
    type Value = Vec<u8>; // Placeholder - will be proper Spend type
    const SIZE: FieldSize = FieldSize::CompactSizeArray;
    
    fn read_from_cursor(cursor: &mut Cursor<&[u8]>) -> Result<Self::Value, ParseError> {
        let count = CompactSize::read(cursor)?;
        
        // For now, just skip the data - proper implementation would parse Spend structures
        for _ in 0..count {
            // Each spend is 384 bytes in v4
            let _spend_data = crate::utils::read_bytes(cursor, 384, "ShieldedSpend")?;
        }
        
        Ok(Vec::new()) // Placeholder
    }
}

/// Placeholder for Sapling Outputs field (will need proper implementation)
pub struct ShieldedOutputs;

impl TransactionField for ShieldedOutputs {
    type Value = Vec<u8>; // Placeholder - will be proper Output type
    const SIZE: FieldSize = FieldSize::CompactSizeArray;
    
    fn read_from_cursor(cursor: &mut Cursor<&[u8]>) -> Result<Self::Value, ParseError> {
        let count = CompactSize::read(cursor)?;
        
        // For now, just skip the data - proper implementation would parse Output structures
        for _ in 0..count {
            // Each output is 948 bytes in v4
            let _output_data = crate::utils::read_bytes(cursor, 948, "ShieldedOutput")?;
        }
        
        Ok(Vec::new()) // Placeholder
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::context::{BlockContext, TransactionContext, ActivationHeights};
    use zebra_chain::{block::Hash, parameters::Network};
    use std::io::Cursor;

    fn create_test_context() -> (BlockContext, [u8; 32], TransactionContext<'static>) {
        let block_context = BlockContext::new(
            500000, // Height where overwinter is active
            Hash([0; 32]),
            Network::Mainnet,
            vec![[1; 32]],
            ActivationHeights::mainnet(),
            1234567890,
        );
        let txid = [1; 32];
        let tx_context = TransactionContext::new(0, &txid, &block_context);
        (block_context, txid, tx_context)
    }

    #[test]
    fn test_lock_time_field() {
        let data = [0x01, 0x02, 0x03, 0x04]; // 4 bytes little endian
        let mut cursor = Cursor::new(&data[..]);
        
        let value = LockTime::read_from_cursor(&mut cursor).unwrap();
        assert_eq!(value, 0x04030201);
    }

    #[test]
    fn test_version_group_id_validation() {
        let (_block_context, _txid, tx_context) = create_test_context();
        
        // Valid version group ID for v4
        let valid_vgid = 0x892F2085;
        VersionGroupId::validate(&valid_vgid, &tx_context).unwrap();
        
        // Invalid version group ID
        let invalid_vgid = 0x12345678;
        let result = VersionGroupId::validate(&invalid_vgid, &tx_context);
        assert!(matches!(
            result,
            Err(ParseError::Validation(ValidationError::InvalidVersionGroupId { .. }))
        ));
    }
}