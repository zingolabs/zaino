//! Block field implementations.

use std::io::Cursor;
use crate::{
    error::ParseError,
    utils::{read_bytes, read_i32, read_u32, CompactSize},
};
use super::{
    context::BlockParsingContext,
    reader::{BlockField, BlockFieldSize},
};

/// Block version field
pub struct BlockVersion;

impl BlockField for BlockVersion {
    type Value = i32;
    const SIZE: BlockFieldSize = BlockFieldSize::Fixed(4);

    fn read_from_cursor(cursor: &mut Cursor<&[u8]>) -> Result<Self::Value, ParseError> {
        read_i32(cursor, "BlockVersion")
    }

    fn validate(value: &Self::Value, _context: &BlockParsingContext) -> Result<(), ParseError> {
        if *value < 4 {
            return Err(ParseError::InvalidData(format!(
                "Block version {} must be at least 4", value
            )));
        }
        Ok(())
    }
}

/// Previous block hash field
pub struct PreviousBlockHash;

impl BlockField for PreviousBlockHash {
    type Value = Vec<u8>;
    const SIZE: BlockFieldSize = BlockFieldSize::Fixed(32);

    fn read_from_cursor(cursor: &mut Cursor<&[u8]>) -> Result<Self::Value, ParseError> {
        read_bytes(cursor, 32, "PreviousBlockHash")
    }

    fn validate(value: &Self::Value, _context: &BlockParsingContext) -> Result<(), ParseError> {
        if value.len() != 32 {
            return Err(ParseError::InvalidData(format!(
                "Previous block hash must be 32 bytes, got {}", value.len()
            )));
        }
        Ok(())
    }
}

/// Merkle root hash field
pub struct MerkleRoot;

impl BlockField for MerkleRoot {
    type Value = Vec<u8>;
    const SIZE: BlockFieldSize = BlockFieldSize::Fixed(32);

    fn read_from_cursor(cursor: &mut Cursor<&[u8]>) -> Result<Self::Value, ParseError> {
        read_bytes(cursor, 32, "MerkleRoot")
    }

    fn validate(value: &Self::Value, _context: &BlockParsingContext) -> Result<(), ParseError> {
        if value.len() != 32 {
            return Err(ParseError::InvalidData(format!(
                "Merkle root must be 32 bytes, got {}", value.len()
            )));
        }
        Ok(())
    }
}

/// Final Sapling root hash field
pub struct FinalSaplingRoot;

impl BlockField for FinalSaplingRoot {
    type Value = Vec<u8>;
    const SIZE: BlockFieldSize = BlockFieldSize::Fixed(32);

    fn read_from_cursor(cursor: &mut Cursor<&[u8]>) -> Result<Self::Value, ParseError> {
        read_bytes(cursor, 32, "FinalSaplingRoot")
    }

    fn validate(value: &Self::Value, _context: &BlockParsingContext) -> Result<(), ParseError> {
        if value.len() != 32 {
            return Err(ParseError::InvalidData(format!(
                "Final Sapling root must be 32 bytes, got {}", value.len()
            )));
        }
        Ok(())
    }
}

/// Block timestamp field
pub struct BlockTime;

impl BlockField for BlockTime {
    type Value = u32;
    const SIZE: BlockFieldSize = BlockFieldSize::Fixed(4);

    fn read_from_cursor(cursor: &mut Cursor<&[u8]>) -> Result<Self::Value, ParseError> {
        read_u32(cursor, "BlockTime")
    }

    fn validate(value: &Self::Value, context: &BlockParsingContext) -> Result<(), ParseError> {
        // Basic sanity check - timestamp should be reasonable
        // Zcash genesis block was created around 2016
        const ZCASH_GENESIS_APPROX: u32 = 1477600000; // Roughly October 2016
        
        if context.strict_validation && *value < ZCASH_GENESIS_APPROX {
            return Err(ParseError::InvalidData(format!(
                "Block timestamp {} is before Zcash genesis", value
            )));
        }
        Ok(())
    }
}

/// nBits (difficulty target) field
pub struct NBits;

impl BlockField for NBits {
    type Value = Vec<u8>;
    const SIZE: BlockFieldSize = BlockFieldSize::Fixed(4);

    fn read_from_cursor(cursor: &mut Cursor<&[u8]>) -> Result<Self::Value, ParseError> {
        read_bytes(cursor, 4, "NBits")
    }

    fn validate(value: &Self::Value, _context: &BlockParsingContext) -> Result<(), ParseError> {
        if value.len() != 4 {
            return Err(ParseError::InvalidData(format!(
                "nBits must be 4 bytes, got {}", value.len()
            )));
        }
        Ok(())
    }
}

/// Block nonce field
pub struct BlockNonce;

impl BlockField for BlockNonce {
    type Value = Vec<u8>;
    const SIZE: BlockFieldSize = BlockFieldSize::Fixed(32);

    fn read_from_cursor(cursor: &mut Cursor<&[u8]>) -> Result<Self::Value, ParseError> {
        read_bytes(cursor, 32, "BlockNonce")
    }

    fn validate(value: &Self::Value, _context: &BlockParsingContext) -> Result<(), ParseError> {
        if value.len() != 32 {
            return Err(ParseError::InvalidData(format!(
                "Block nonce must be 32 bytes, got {}", value.len()
            )));
        }
        Ok(())
    }
}

/// Equihash solution field
pub struct EquihashSolution;

impl BlockField for EquihashSolution {
    type Value = Vec<u8>;
    const SIZE: BlockFieldSize = BlockFieldSize::CompactSize;

    fn read_from_cursor(cursor: &mut Cursor<&[u8]>) -> Result<Self::Value, ParseError> {
        let solution_length = CompactSize::read(cursor)?;
        read_bytes(cursor, solution_length as usize, "EquihashSolution")
    }

    fn validate(value: &Self::Value, context: &BlockParsingContext) -> Result<(), ParseError> {
        // Equihash solution should have a specific length based on parameters
        // For Zcash mainnet, solutions are typically 1344 bytes
        const EXPECTED_SOLUTION_SIZE: usize = 1344;
        
        if context.strict_validation && context.is_mainnet() && value.len() != EXPECTED_SOLUTION_SIZE {
            return Err(ParseError::InvalidData(format!(
                "Equihash solution length {} does not match expected {}", 
                value.len(), EXPECTED_SOLUTION_SIZE
            )));
        }
        Ok(())
    }
}

/// Transaction count field
pub struct TransactionCount;

impl BlockField for TransactionCount {
    type Value = u64;
    const SIZE: BlockFieldSize = BlockFieldSize::CompactSize;

    fn read_from_cursor(cursor: &mut Cursor<&[u8]>) -> Result<Self::Value, ParseError> {
        CompactSize::read(cursor)
    }

    fn validate(value: &Self::Value, context: &BlockParsingContext) -> Result<(), ParseError> {
        // Check against expected count if provided
        if let Some(expected) = context.expected_tx_count {
            if *value as usize != expected {
                return Err(ParseError::InvalidData(format!(
                    "Transaction count {} does not match expected {}", value, expected
                )));
            }
        }

        // Reasonable upper bound check
        const MAX_REASONABLE_TX_COUNT: u64 = 100_000;
        if context.strict_validation && *value > MAX_REASONABLE_TX_COUNT {
            return Err(ParseError::InvalidData(format!(
                "Transaction count {} exceeds reasonable maximum", value
            )));
        }

        // Must have at least one transaction (coinbase)
        if *value == 0 {
            return Err(ParseError::InvalidData(
                "Block must contain at least one transaction (coinbase)".to_string()
            ));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zebra_chain::parameters::Network;
    use std::io::Cursor;

    fn create_test_context() -> BlockParsingContext {
        BlockParsingContext::new(Network::Mainnet)
            .with_height(100000)
    }

    #[test]
    fn test_block_version_field() {
        let data = [0x04, 0x00, 0x00, 0x00]; // Version 4
        let mut cursor = Cursor::new(&data[..]);
        
        let version = BlockVersion::read_from_cursor(&mut cursor).unwrap();
        assert_eq!(version, 4);

        let context = create_test_context();
        assert!(BlockVersion::validate(&version, &context).is_ok());
        
        // Test invalid version
        assert!(BlockVersion::validate(&3, &context).is_err());
    }

    #[test]
    fn test_previous_block_hash_field() {
        let data = [1u8; 32]; // 32 bytes of 0x01
        let mut cursor = Cursor::new(&data[..]);
        
        let hash = PreviousBlockHash::read_from_cursor(&mut cursor).unwrap();
        assert_eq!(hash.len(), 32);
        assert_eq!(hash, vec![1u8; 32]);

        let context = create_test_context();
        assert!(PreviousBlockHash::validate(&hash, &context).is_ok());
    }

    #[test]
    fn test_block_time_validation() {
        let context = create_test_context();
        
        // Valid timestamp (recent)
        let valid_time = 1600000000u32; // September 2020
        assert!(BlockTime::validate(&valid_time, &context).is_ok());
        
        // Invalid timestamp (too early)
        let invalid_time = 1000000000u32; // September 2001 (before Zcash)
        assert!(BlockTime::validate(&invalid_time, &context).is_err());
        
        // Should pass with relaxed validation
        let relaxed_context = create_test_context().with_relaxed_validation();
        assert!(BlockTime::validate(&invalid_time, &relaxed_context).is_ok());
    }

    #[test]
    fn test_transaction_count_field() {
        let context = create_test_context().with_tx_count(5);
        
        // Valid count matching expected
        assert!(TransactionCount::validate(&5, &context).is_ok());
        
        // Invalid count not matching expected
        assert!(TransactionCount::validate(&3, &context).is_err());
        
        // Zero transactions (invalid)
        assert!(TransactionCount::validate(&0, &context).is_err());
        
        // Extremely high count (invalid in strict mode)
        assert!(TransactionCount::validate(&200_000, &context).is_err());
    }

    #[test]
    fn test_equihash_solution_validation() {
        let context = create_test_context();
        
        // Correct length for mainnet
        let correct_solution = vec![0u8; 1344];
        assert!(EquihashSolution::validate(&correct_solution, &context).is_ok());
        
        // Incorrect length for mainnet (strict validation)
        let incorrect_solution = vec![0u8; 1000];
        assert!(EquihashSolution::validate(&incorrect_solution, &context).is_err());
        
        // Should pass with relaxed validation
        let relaxed_context = create_test_context().with_relaxed_validation();
        assert!(EquihashSolution::validate(&incorrect_solution, &relaxed_context).is_ok());
    }
}