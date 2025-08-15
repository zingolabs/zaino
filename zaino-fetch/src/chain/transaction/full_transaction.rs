//! FullTransaction implementation using modern transaction parsing.
//!
//! This module provides the FullTransaction API built on top of the new
//! modular transaction parsing system with cursor-based parsing.

use std::io::{Cursor, Read};
use crate::{
    error::ParseError,
    utils::ParseFromHex,
    chain::types::{TxId, TxIdBytes, txid_bytes_to_canonical},
};
use super::{
    Transaction, TransactionDispatcher, BlockContext, TransactionContext,
    context::ActivationHeights, fields::{TxIn, TxOut}
};
use zebra_chain::{block::Hash, parameters::Network};

/// Full transaction with both raw data and parsed structure.
/// 
/// This struct combines the new Transaction enum with raw bytes and transaction ID,
/// providing a complete transaction representation for the Zaino indexer.
#[derive(Debug, Clone)]
pub struct FullTransaction {
    /// The parsed transaction using the new system
    transaction: Transaction,
    /// Raw transaction bytes
    raw_bytes: Vec<u8>,
    /// Transaction ID bytes
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

    /// Returns overwintered bool
    pub fn f_overwintered(&self) -> bool {
        match &self.transaction {
            Transaction::V1(_) => false, // V1 transactions are never overwintered
            Transaction::V4(_) => true,  // V4 transactions are always overwintered
        }
    }

    /// Returns the transaction version.
    pub fn version(&self) -> u32 {
        self.transaction.version()
    }

    /// Returns the transaction version group id.
    pub fn n_version_group_id(&self) -> Option<u32> {
        match &self.transaction {
            Transaction::V1(_) => None, // V1 transactions don't have version group ID
            Transaction::V4(tx) => Some(tx.version_group_id()),
        }
    }

    /// Returns the consensus branch id of the transaction.
    pub fn consensus_branch_id(&self) -> u32 {
        // TODO: This should be extracted from transaction parsing context
        // For now, return 0 as a placeholder
        0
    }

    /// Returns a vec of transparent inputs: (prev_txid, prev_index, script_sig).
    pub fn transparent_inputs(&self) -> Vec<(Vec<u8>, u32, Vec<u8>)> {
        match &self.transaction {
            Transaction::V1(tx) => tx.transparent_inputs().iter().map(|input| {
                (input.prev_txid.to_vec(), input.prev_index, input.script.clone())
            }).collect(),
            Transaction::V4(tx) => tx.transparent_inputs().iter().map(|input| {
                (input.prev_txid.to_vec(), input.prev_index, input.script.clone())
            }).collect(),
        }
    }

    /// Returns a vec of transparent outputs: (value, script_hash).
    pub fn transparent_outputs(&self) -> Vec<(u64, Vec<u8>)> {
        match &self.transaction {
            Transaction::V1(tx) => tx.transparent_outputs().iter().map(|output| {
                (output.value as u64, output.script.clone())
            }).collect(),
            Transaction::V4(tx) => tx.transparent_outputs().iter().map(|output| {
                (output.value as u64, output.script.clone())
            }).collect(),
        }
    }

    /// Returns sapling and orchard value balances for the transaction.
    ///
    /// Returned as (Option<valueBalanceSapling>, Option<valueBalanceOrchard>).
    pub fn value_balances(&self) -> (Option<i64>, Option<i64>) {
        match &self.transaction {
            Transaction::V1(_) => (None, None), // V1 has no shielded pools
            Transaction::V4(tx) => (Some(tx.value_balance_sapling()), None), // V4 has Sapling but no Orchard
        }
    }

    /// Returns a vec of sapling nullifiers for the transaction.
    pub fn shielded_spends(&self) -> Vec<Vec<u8>> {
        match &self.transaction {
            Transaction::V1(_) => Vec::new(), // V1 has no shielded spends
            Transaction::V4(tx) => {
                // TODO: Extract actual nullifiers from shielded spends
                // For now, return empty as the current V4 implementation stores placeholder data
                Vec::new()
            }
        }
    }

    /// Returns a vec of sapling outputs (cmu, ephemeral_key, enc_ciphertext) for the transaction.
    pub fn shielded_outputs(&self) -> Vec<(Vec<u8>, Vec<u8>, Vec<u8>)> {
        match &self.transaction {
            Transaction::V1(_) => Vec::new(), // V1 has no shielded outputs
            Transaction::V4(tx) => {
                // TODO: Extract actual output data from shielded outputs
                // For now, return empty as the current V4 implementation stores placeholder data
                Vec::new()
            }
        }
    }

    /// Returns None as joinsplits are not supported in Zaino.
    pub fn join_splits(&self) -> Option<()> {
        None
    }

    /// Returns a vec of orchard actions (nullifier, cmx, ephemeral_key, enc_ciphertext) for the transaction.
    #[allow(clippy::type_complexity)]
    pub fn orchard_actions(&self) -> Vec<(Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>)> {
        // V1 and V4 transactions don't have Orchard actions
        Vec::new()
    }

    /// Returns the orchard anchor of the transaction.
    ///
    /// If this is the Coinbase transaction then this returns the AuthDataRoot of the block.
    pub fn anchor_orchard(&self) -> Option<Vec<u8>> {
        // V1 and V4 transactions don't have Orchard anchors
        None
    }

    /// Returns the transaction as raw bytes.
    pub fn raw_bytes(&self) -> Vec<u8> {
        self.raw_bytes.clone()
    }

    /// Returns the TxId of the transaction.
    pub fn tx_id(&self) -> Vec<u8> {
        self.tx_id.clone()
    }

    /// Returns true if the transaction contains either sapling spends or outputs.
    pub(crate) fn has_shielded_elements(&self) -> bool {
        self.transaction.has_shielded_components()
    }

    /// Check if this is a coinbase transaction
    pub fn is_coinbase(&self) -> bool {
        self.transaction.is_coinbase()
    }

    /// Converts a zcash full transaction into a compact transaction.
    pub fn to_compact(self, index: u64) -> Result<zaino_proto::proto::compact_formats::CompactTx, ParseError> {
        use zaino_proto::proto::compact_formats::{
            CompactTx, CompactSaplingSpend, CompactSaplingOutput, CompactOrchardAction
        };
        
        let hash = self.tx_id.clone();
        
        // NOTE: LightWalletD currently does not return a fee and is not currently priority here. 
        // Please open an Issue or PR at the Zingo-Indexer github (https://github.com/zingolabs/zingo-indexer) 
        // if you require this functionality.
        let fee = 0;
        
        // For V1 transactions, all shielded components are empty
        let (spends, outputs) = match &self.transaction {
            Transaction::V1(_) => {
                (Vec::new(), Vec::new())
            }
            Transaction::V4(tx) => {
                // TODO: When we implement proper shielded spend/output parsing,
                // extract the actual data here. For now, return empty as the current
                // V4 implementation uses placeholder Vec<u8> data.
                let spends = Vec::new(); // Would extract nullifiers from tx.raw().shielded_spends
                let outputs = Vec::new(); // Would extract cmu, ephemeral_key, ciphertext from tx.raw().shielded_outputs
                (spends, outputs)
            }
        };
        
        // Orchard actions are empty for V1 and V4 transactions
        let actions = Vec::new();
        
        Ok(CompactTx {
            index,
            hash,
            fee,
            spends,
            outputs,
            actions,
        })
    }
}

/// Context for parsing FullTransaction
#[derive(Debug, Clone)]
pub struct FullTransactionContext {
    /// Transaction ID from external source (like RPC response)
    pub txid: TxIdBytes,
    /// Block context for validation
    pub block_context: Option<BlockContext>,
}

impl FullTransactionContext {
    /// Create a new context with just the transaction ID
    pub fn new(txid: TxIdBytes) -> Self {
        Self {
            txid,
            block_context: None,
        }
    }
    
    /// Create context with block information
    pub fn with_block_context(txid: TxIdBytes, block_context: BlockContext) -> Self {
        Self {
            txid,
            block_context: Some(block_context),
        }
    }
}

impl ParseFromHex for FullTransaction {
    type Context = FullTransactionContext;
    
    fn parse_from_cursor(
        cursor: &mut Cursor<&[u8]>,
        context: Self::Context,
    ) -> Result<Self, ParseError> {
        // Get starting position to calculate consumed bytes
        let start_position = cursor.position();

        // Create block context for parsing (or use provided one)
        let block_context = context.block_context.unwrap_or_else(|| {
            BlockContext::minimal_for_parsing()
        });

        // Convert txid to canonical format
        let txid = txid_bytes_to_canonical(context.txid.clone())?;

        // Create transaction context
        let tx_context = TransactionContext::new(0, &txid, &block_context);

        // Parse using the new transaction system
        let transaction = TransactionDispatcher::parse(cursor, &tx_context)?;

        // Calculate consumed bytes
        let end_position = cursor.position();
        let consumed_bytes = (end_position - start_position) as usize;
        
        // Get raw bytes from the original data
        cursor.set_position(start_position);
        let mut raw_bytes = vec![0u8; consumed_bytes];
        cursor.read_exact(&mut raw_bytes)
            .map_err(|e| ParseError::InvalidData(format!("Failed to read raw bytes: {}", e)))?;
        
        // Create FullTransaction
        let full_transaction = FullTransaction::from_transaction(transaction, raw_bytes);
        
        Ok(full_transaction)
    }
}

/// Temporary compatibility for old API - will be removed
impl FullTransaction {
    /// Parse from slice using old API - DEPRECATED
    pub fn parse_from_slice(
        data: &[u8],
        txid: Option<Vec<Vec<u8>>>,
        _tx_version: Option<u32>,
    ) -> Result<(&[u8], Self), ParseError> {
        let txid_bytes = txid
            .and_then(|mut txids| txids.pop())
            .ok_or_else(|| ParseError::InvalidData("txid required".to_string()))?;
        
        let context = FullTransactionContext::new(txid_bytes);
        let (remaining, transaction) = Self::parse_from_slice(data, context)?;
        
        Ok((remaining, transaction))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::transaction::{
        versions::v1::{TransactionV1, RawTransactionV1},
        Transaction,
    };

    #[test]
    fn test_full_transaction_creation() {
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

        // Create FullTransaction
        let raw_bytes = vec![1, 2, 3, 4]; // Mock raw bytes
        let full_tx = FullTransaction::from_transaction(transaction, raw_bytes);

        // Test methods
        assert_eq!(full_tx.version(), 1);
        assert!(full_tx.is_coinbase());
        assert!(!full_tx.has_shielded_elements());
        assert_eq!(full_tx.tx_id(), &txid);
        assert_eq!(full_tx.raw_bytes(), &[1, 2, 3, 4]);
        assert_eq!(full_tx.transparent_inputs().len(), 1);
        assert_eq!(full_tx.transparent_outputs().len(), 1);
        assert!(full_tx.anchor_orchard().is_none());
        assert!(!full_tx.f_overwintered());
        assert!(full_tx.n_version_group_id().is_none());
    }

    #[test]
    fn test_full_transaction_parsing_context() {
        let txid_bytes = vec![1u8; 32];
        let context = FullTransactionContext::new(txid_bytes.clone());
        
        assert_eq!(context.txid, txid_bytes);
        assert!(context.block_context.is_none());
    }
}