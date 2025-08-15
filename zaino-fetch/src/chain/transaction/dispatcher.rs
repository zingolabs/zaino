//! Transaction version dispatcher.
//!
//! This module provides a unified interface for parsing transactions
//! of any version by detecting the version and routing to the appropriate
//! version-specific parser.

use std::io::Cursor;
use crate::error::{ParseError, ValidationError};
use super::{
    context::TransactionContext,
    version_reader::peek_transaction_version,
    versions::{
        v1::{TransactionV1Reader, TransactionV1},
        v4::{TransactionV4Reader, TransactionV4},
    },
};

/// Union type for all supported transaction versions
#[derive(Debug, Clone)]
pub enum Transaction {
    V1(TransactionV1),
    V4(TransactionV4),
    // TODO: Add V2, V3, V5 when implemented
}

impl Transaction {
    /// Get the transaction ID regardless of version
    pub fn txid(&self) -> &[u8; 32] {
        match self {
            Transaction::V1(tx) => tx.txid(),
            Transaction::V4(tx) => tx.txid(),
        }
    }

    /// Get the transaction version
    pub fn version(&self) -> u32 {
        match self {
            Transaction::V1(tx) => tx.version(),
            Transaction::V4(tx) => tx.version(),
        }
    }

    /// Check if this is a coinbase transaction
    pub fn is_coinbase(&self) -> bool {
        match self {
            Transaction::V1(tx) => tx.is_coinbase(),
            Transaction::V4(tx) => tx.is_coinbase(),
        }
    }

    /// Get the number of transparent inputs
    pub fn input_count(&self) -> usize {
        match self {
            Transaction::V1(tx) => tx.input_count(),
            Transaction::V4(tx) => tx.input_count(),
        }
    }

    /// Get the number of transparent outputs
    pub fn output_count(&self) -> usize {
        match self {
            Transaction::V1(tx) => tx.output_count(),
            Transaction::V4(tx) => tx.output_count(),
        }
    }

    /// Get the lock time
    pub fn lock_time(&self) -> u32 {
        match self {
            Transaction::V1(tx) => tx.lock_time(),
            Transaction::V4(tx) => tx.lock_time(),
        }
    }

    /// Get the total transparent output value
    pub fn transparent_output_value(&self) -> Result<i64, ValidationError> {
        match self {
            Transaction::V1(tx) => tx.transparent_output_value(),
            Transaction::V4(tx) => tx.transparent_output_value(),
        }
    }

    /// Check if transaction has any shielded components (V4+ only)
    pub fn has_shielded_components(&self) -> bool {
        match self {
            Transaction::V1(_) => false, // V1 transactions never have shielded components
            Transaction::V4(tx) => tx.has_shielded_components(),
        }
    }

    /// Get version-specific data (for advanced use cases)
    pub fn version_specific(&self) -> TransactionVersionSpecific {
        match self {
            Transaction::V1(tx) => TransactionVersionSpecific::V1(tx),
            Transaction::V4(tx) => TransactionVersionSpecific::V4(tx),
        }
    }
}

/// Version-specific transaction data access
pub enum TransactionVersionSpecific<'a> {
    V1(&'a TransactionV1),
    V4(&'a TransactionV4),
}

impl<'a> TransactionVersionSpecific<'a> {
    /// Get V4-specific fields (returns None for non-V4 transactions)
    pub fn v4_fields(&self) -> Option<V4Fields> {
        match self {
            TransactionVersionSpecific::V4(tx) => Some(V4Fields {
                version_group_id: tx.version_group_id(),
                expiry_height: tx.expiry_height(),
                value_balance_sapling: tx.value_balance_sapling(),
            }),
            _ => None,
        }
    }
}

/// V4-specific field access
pub struct V4Fields {
    pub version_group_id: u32,
    pub expiry_height: u32,
    pub value_balance_sapling: i64,
}

/// Transaction dispatcher - routes parsing to version-specific readers
pub struct TransactionDispatcher;

impl TransactionDispatcher {
    /// Parse a transaction of any supported version
    /// 
    /// This method peeks at the transaction version header and routes
    /// to the appropriate version-specific parser.
    pub fn parse(
        cursor: &mut Cursor<&[u8]>,
        context: &TransactionContext,
    ) -> Result<Transaction, ParseError> {
        // Peek at version to determine parser
        let version = peek_transaction_version(cursor)?;
        
        // Skip the header (version + overwinter flag) - already consumed by peek
        cursor.set_position(cursor.position() + 4);
        
        // Route to appropriate parser
        match version {
            1 => {
                let tx = TransactionV1Reader::parse(cursor, context)?;
                Ok(Transaction::V1(tx))
            }
            4 => {
                let tx = TransactionV4Reader::parse(cursor, context)?;
                Ok(Transaction::V4(tx))
            }
            2 | 3 => {
                // TODO: Implement V2 and V3 parsers
                Err(ParseError::UnsupportedVersion { version })
            }
            5 => {
                // TODO: Implement V5 parser
                Err(ParseError::UnsupportedVersion { version })
            }
            _ => Err(ParseError::UnsupportedVersion { version }),
        }
    }

    /// Parse a transaction and return only version information (for quick version detection)
    pub fn detect_version(cursor: &mut Cursor<&[u8]>) -> Result<u32, ParseError> {
        peek_transaction_version(cursor)
    }

    /// Check if a version is supported by this dispatcher
    pub fn is_version_supported(version: u32) -> bool {
        matches!(version, 1 | 4)
        // TODO: Add 2, 3, 5 when implemented
    }

    /// Get list of all supported versions
    pub fn supported_versions() -> Vec<u32> {
        vec![1, 4]
        // TODO: Add 2, 3, 5 when implemented
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::transaction::context::{BlockContext, ActivationHeights, TransactionContext};
    use zebra_chain::{block::Hash, parameters::Network};
    use std::io::Cursor;

    fn create_test_context() -> (BlockContext, [u8; 32], TransactionContext<'static>) {
        let block_context = BlockContext::new(
            500000, // High enough for all upgrades
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
    fn test_version_detection() {
        // Test V1 version detection
        let v1_header = [0x01, 0x00, 0x00, 0x00]; // Version 1, no overwinter
        let mut cursor = Cursor::new(&v1_header[..]);
        let version = TransactionDispatcher::detect_version(&mut cursor).unwrap();
        assert_eq!(version, 1);

        // Test V4 version detection  
        let v4_header = [0x04, 0x00, 0x00, 0x80]; // Version 4, with overwinter flag
        let mut cursor = Cursor::new(&v4_header[..]);
        let version = TransactionDispatcher::detect_version(&mut cursor).unwrap();
        assert_eq!(version, 4);
    }

    #[test]
    fn test_supported_versions() {
        assert!(TransactionDispatcher::is_version_supported(1));
        assert!(TransactionDispatcher::is_version_supported(4));
        assert!(!TransactionDispatcher::is_version_supported(2));
        assert!(!TransactionDispatcher::is_version_supported(3));
        assert!(!TransactionDispatcher::is_version_supported(5));
        assert!(!TransactionDispatcher::is_version_supported(99));
        
        let supported = TransactionDispatcher::supported_versions();
        assert_eq!(supported, vec![1, 4]);
    }

    #[test]
    fn test_transaction_enum_methods() {
        let (_block_context, txid, _tx_context) = create_test_context();
        
        // Create a V1 transaction for testing
        use crate::chain::transaction::versions::v1::{TransactionV1, RawTransactionV1};
        use crate::chain::transaction::fields::{TxIn, TxOut};

        let raw_v1 = RawTransactionV1 {
            transparent_inputs: vec![TxIn {
                prev_txid: [0; 32],
                prev_index: 0xFFFFFFFF,
                script: vec![1, 2, 3],
            }],
            transparent_outputs: vec![TxOut {
                value: 5000000000,
                script: vec![4, 5, 6],
            }],
            lock_time: 0,
        };
        
        let tx_v1 = TransactionV1::new(raw_v1, txid);
        let tx = Transaction::V1(tx_v1);

        assert_eq!(tx.version(), 1);
        assert_eq!(tx.txid(), &txid);
        assert!(tx.is_coinbase());
        assert_eq!(tx.input_count(), 1);
        assert_eq!(tx.output_count(), 1);
        assert_eq!(tx.lock_time(), 0);
        assert!(!tx.has_shielded_components());
        assert_eq!(tx.transparent_output_value().unwrap(), 5000000000);

        // Test version-specific access
        match tx.version_specific() {
            TransactionVersionSpecific::V1(_) => {
                // V1 should not have V4 fields
                assert!(tx.version_specific().v4_fields().is_none());
            }
            _ => panic!("Expected V1 transaction"),
        }
    }

    #[test]
    fn test_unsupported_version_error() {
        let (_block_context, _txid, tx_context) = create_test_context();
        
        // Test data with unsupported version 5
        let v5_header = [0x05, 0x00, 0x00, 0x80]; // Version 5
        let mut cursor = Cursor::new(&v5_header[..]);
        
        let result = TransactionDispatcher::parse(&mut cursor, &tx_context);
        assert!(matches!(result, Err(ParseError::UnsupportedVersion { version: 5 })));
    }
}