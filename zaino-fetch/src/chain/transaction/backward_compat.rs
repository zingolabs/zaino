//! Backward compatibility layer for old FullTransaction API.
//!
//! This module provides a compatibility wrapper around the new Transaction
//! enum to maintain the old FullTransaction interface during migration.

use std::io::Cursor;
use crate::{
    error::ParseError,
    utils::ParseFromSlice,
};
use super::{
    Transaction, TransactionDispatcher, BlockContext, TransactionContext,
    context::ActivationHeights, fields::{TxIn, TxOut}
};
use zebra_chain::{block::Hash, parameters::Network};

/// Backward compatibility wrapper for the old FullTransaction interface.
/// 
/// This struct wraps the new Transaction enum and provides the methods
/// expected by existing code during the migration period.
#[derive(Debug, Clone)]
pub struct FullTransaction {
    /// The parsed transaction using the new system
    transaction: Transaction,
    /// Raw transaction bytes for compatibility
    raw_bytes: Vec<u8>,
    /// Transaction ID bytes for compatibility
    tx_id: Vec<u8>,
}

impl FullTransaction {
    /// Create a new FullTransaction from the modern Transaction type
    pub fn from_transaction(transaction: Transaction, raw_bytes: Vec<u8>) -> Self {
        let tx_id = transaction.txid().to_vec();
        Self {
            transaction,
            raw_bytes,
            tx_id,
        }
    }

    /// Get the underlying modern Transaction
    pub fn transaction(&self) -> &Transaction {
        &self.transaction
    }

    /// Check if transaction has shielded elements (for compatibility)
    pub fn has_shielded_elements(&self) -> bool {
        self.transaction.has_shielded_components()
    }

    /// Get transparent inputs (for compatibility)
    pub fn transparent_inputs(&self) -> Vec<TxIn> {
        match &self.transaction {
            Transaction::V1(tx) => tx.transparent_inputs().to_vec(),
            Transaction::V4(tx) => tx.transparent_inputs().to_vec(),
        }
    }

    /// Get transparent outputs (for compatibility)
    pub fn transparent_outputs(&self) -> Vec<TxOut> {
        match &self.transaction {
            Transaction::V1(tx) => tx.transparent_outputs().to_vec(),
            Transaction::V4(tx) => tx.transparent_outputs().to_vec(),
        }
    }

    /// Get transaction ID bytes (for compatibility)
    pub fn tx_id(&self) -> &[u8] {
        &self.tx_id
    }

    /// Get raw transaction bytes (for compatibility)
    pub fn raw_bytes(&self) -> &[u8] {
        &self.raw_bytes
    }

    /// Get transaction version
    pub fn version(&self) -> u32 {
        self.transaction.version()
    }

    /// Check if this is a coinbase transaction
    pub fn is_coinbase(&self) -> bool {
        self.transaction.is_coinbase()
    }

    /// Convert to compact transaction format (placeholder implementation)
    /// 
    /// NOTE: This is a simplified implementation for backward compatibility.
    /// The full implementation would need to handle all the protobuf
    /// conversion details from the original FullTransaction.
    pub fn to_compact(&self) -> Result<zaino_proto::proto::compact_formats::CompactTx, ParseError> {
        // Create a basic CompactTx for compatibility
        // This is a simplified implementation - a full implementation would
        // need to properly convert all shielded components, etc.
        
        use zaino_proto::proto::compact_formats::CompactTx;
        
        let mut compact_tx = CompactTx::default();
        compact_tx.hash = self.tx_id.clone();
        
        // Add basic transparent data
        // NOTE: This is incomplete - full implementation would need:
        // - Proper handling of shielded spends/outputs
        // - Action conversion for V5 transactions
        // - Fee calculation
        // - etc.
        
        Ok(compact_tx)
    }

    /// Get Orchard anchor (placeholder for compatibility)
    /// 
    /// NOTE: This returns None as V1/V4 transactions don't have Orchard data.
    /// This method exists for API compatibility.
    pub fn anchor_orchard(&self) -> Option<Vec<u8>> {
        // V1 and V4 transactions don't have Orchard components
        // This method exists for compatibility with code that expects it
        None
    }
}

impl ParseFromSlice for FullTransaction {
    fn parse_from_slice(
        data: &[u8],
        txid: Option<Vec<Vec<u8>>>,
        tx_version: Option<u32>,
    ) -> Result<(&[u8], Self), ParseError>
    where
        Self: Sized,
    {
        // Store original data length for calculating remaining
        let original_len = data.len();
        let mut cursor = Cursor::new(data);

        // Create a minimal block context for parsing
        // In a full implementation, this would use actual block context
        let block_context = BlockContext::new(
            1000000, // High height to ensure all upgrades are active
            Hash([0; 32]),
            Network::Mainnet,
            txid.clone().unwrap_or_default().into_iter().map(|id| {
                let mut array = [0u8; 32];
                array.copy_from_slice(&id[..32.min(id.len())]);
                array
            }).collect(),
            ActivationHeights::mainnet(),
            0,
        );

        // Extract transaction ID from the provided txid parameter
        // This mimics the old API where txid was passed separately
        let tx_id = if let Some(ref txids) = txid {
            if !txids.is_empty() {
                let mut array = [0u8; 32];
                array.copy_from_slice(&txids[0][..32.min(txids[0].len())]);
                array
            } else {
                [0u8; 32] // Fallback
            }
        } else {
            [0u8; 32] // Fallback
        };

        // Create transaction context
        let tx_context = TransactionContext::new(0, &tx_id, &block_context);

        // Parse using the new transaction system
        let transaction = TransactionDispatcher::parse(&mut cursor, &tx_context)?;

        // Calculate how much data was consumed
        let consumed = cursor.position() as usize;
        let remaining = &data[consumed..];

        // Store the raw bytes that were consumed
        let raw_bytes = data[..consumed].to_vec();

        // Create the compatibility wrapper
        let full_transaction = FullTransaction::from_transaction(transaction, raw_bytes);

        Ok((remaining, full_transaction))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::transaction::{
        versions::v1::{TransactionV1, RawTransactionV1},
        fields::{TxIn, TxOut},
    };

    #[test]
    fn test_full_transaction_compatibility() {
        // Create a test V1 transaction
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

        let txid = [1; 32];
        let tx_v1 = TransactionV1::new(raw_v1, txid);
        let transaction = Transaction::V1(tx_v1);

        // Create FullTransaction wrapper
        let raw_bytes = vec![1, 2, 3, 4]; // Mock raw bytes
        let full_tx = FullTransaction::from_transaction(transaction, raw_bytes);

        // Test compatibility methods
        assert_eq!(full_tx.version(), 1);
        assert!(full_tx.is_coinbase());
        assert!(!full_tx.has_shielded_elements());
        assert_eq!(full_tx.tx_id(), &txid);
        assert_eq!(full_tx.raw_bytes(), &[1, 2, 3, 4]);
        assert_eq!(full_tx.transparent_inputs().len(), 1);
        assert_eq!(full_tx.transparent_outputs().len(), 1);
        assert!(full_tx.anchor_orchard().is_none());
    }

    #[test]
    fn test_full_transaction_to_compact() {
        // Create a test transaction
        let raw_v1 = RawTransactionV1 {
            transparent_inputs: vec![],
            transparent_outputs: vec![],
            lock_time: 0,
        };

        let txid = [1; 32];
        let tx_v1 = TransactionV1::new(raw_v1, txid);
        let transaction = Transaction::V1(tx_v1);

        let full_tx = FullTransaction::from_transaction(transaction, vec![]);

        // Test to_compact method (basic functionality)
        let compact_result = full_tx.to_compact();
        assert!(compact_result.is_ok());
        
        let compact_tx = compact_result.unwrap();
        assert_eq!(compact_tx.hash, txid.to_vec());
    }
}