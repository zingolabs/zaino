//! Version 1 transaction implementation.
//!
//! V1 transactions are the simplest Zcash transactions, containing only:
//! - Transparent inputs
//! - Transparent outputs  
//! - Lock time
//!
//! They do not support:
//! - Overwinter flag
//! - Version group ID
//! - Expiry height
//! - Shielded components

use std::io::Cursor;
use crate::error::{ParseError, ValidationError};
use crate::chain::transaction::{
    context::{TransactionContext, TxId},
    reader::FieldReader,
    version_reader::TransactionVersionReader,
    fields::{TransparentInputs, TransparentOutputs, LockTime, TxIn, TxOut},
};

/// Raw V1 transaction data (before validation)
#[derive(Debug, Clone)]
pub struct RawTransactionV1 {
    pub transparent_inputs: Vec<TxIn>,
    pub transparent_outputs: Vec<TxOut>,
    pub lock_time: u32,
}

/// Final V1 transaction (validated and with convenience methods)
#[derive(Debug, Clone)]
pub struct TransactionV1 {
    raw: RawTransactionV1,
    txid: TxId,
}

impl TransactionV1 {
    /// Create a new V1 transaction
    pub fn new(raw: RawTransactionV1, txid: TxId) -> Self {
        Self { raw, txid }
    }

    /// Get the transaction ID
    pub fn txid(&self) -> &TxId {
        &self.txid
    }

    /// Get the number of transparent inputs
    pub fn input_count(&self) -> usize {
        self.raw.transparent_inputs.len()
    }

    /// Get the number of transparent outputs
    pub fn output_count(&self) -> usize {
        self.raw.transparent_outputs.len()
    }

    /// Get the lock time
    pub fn lock_time(&self) -> u32 {
        self.raw.lock_time
    }

    /// Check if this is a coinbase transaction
    pub fn is_coinbase(&self) -> bool {
        self.raw.transparent_inputs.len() == 1 && 
        self.raw.transparent_inputs[0].is_coinbase()
    }

    /// Get the transparent inputs
    pub fn transparent_inputs(&self) -> &[TxIn] {
        &self.raw.transparent_inputs
    }

    /// Get the transparent outputs
    pub fn transparent_outputs(&self) -> &[TxOut] {
        &self.raw.transparent_outputs
    }

    /// Get the total transparent output value
    pub fn transparent_output_value(&self) -> Result<i64, ValidationError> {
        self.raw.transparent_outputs
            .iter()
            .try_fold(0i64, |acc, output| {
                acc.checked_add(output.value)
                    .ok_or(ValidationError::ValueOverflow)
            })
    }

    /// Access to raw transaction data
    pub fn raw(&self) -> &RawTransactionV1 {
        &self.raw
    }

    /// Get the transaction version (always 1 for V1)
    pub fn version(&self) -> u32 {
        1
    }
}

/// V1 transaction reader
pub struct TransactionV1Reader;

impl TransactionVersionReader for TransactionV1Reader {
    type RawResult = RawTransactionV1;
    type FinalResult = TransactionV1;

    fn read_raw(
        cursor: &mut Cursor<&[u8]>, 
        context: &TransactionContext
    ) -> Result<Self::RawResult, ParseError> {
        let mut reader = FieldReader::new(cursor, context);

        // V1 field order: inputs → outputs → lock_time
        let transparent_inputs = reader.read_field::<TransparentInputs>(0)?;
        let transparent_outputs = reader.read_field::<TransparentOutputs>(1)?;
        let lock_time = reader.read_field::<LockTime>(2)?;

        Ok(RawTransactionV1 {
            transparent_inputs,
            transparent_outputs,
            lock_time,
        })
    }

    fn validate_and_construct(
        raw: Self::RawResult,
        context: &TransactionContext
    ) -> Result<Self::FinalResult, ValidationError> {
        // V1-specific validation
        
        // Must have at least one input or output
        if raw.transparent_inputs.is_empty() && raw.transparent_outputs.is_empty() {
            return Err(ValidationError::EmptyTransaction);
        }

        // V1 transactions cannot be overwinter
        if context.block_context.is_overwinter_active() && context.tx_index == 0 {
            // Allow coinbase transactions even in overwinter blocks
            // since they might still be V1
        } else if context.block_context.is_overwinter_active() {
            return Err(ValidationError::V1CannotBeOverwinter);
        }

        // Validate output values sum doesn't overflow
        let _total_output_value = raw.transparent_outputs
            .iter()
            .try_fold(0i64, |acc, output| {
                acc.checked_add(output.value)
                    .ok_or(ValidationError::ValueOverflow)
            })?;

        // Validate all output values are non-negative
        for (i, output) in raw.transparent_outputs.iter().enumerate() {
            if output.value < 0 {
                return Err(ValidationError::Generic {
                    message: format!("Output {} has negative value: {}", i, output.value),
                });
            }
        }

        // Construct final transaction
        Ok(TransactionV1::new(raw, context.txid.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::transaction::context::{BlockContext, ActivationHeights, TransactionContext};
    use zebra_chain::{block::Hash, parameters::Network};
    use std::io::Cursor;

    fn create_test_context_pre_overwinter() -> (BlockContext, TxId, TransactionContext<'static>) {
        let block_context = BlockContext::new(
            100000, // Before overwinter activation
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
    fn test_v1_transaction_creation() {
        let (_block_context, txid, _tx_context) = create_test_context_pre_overwinter();
        
        let raw = RawTransactionV1 {
            transparent_inputs: vec![TxIn {
                prev_txid: [0; 32],
                prev_index: 0xFFFFFFFF, // Coinbase
                script: vec![1, 2, 3],
            }],
            transparent_outputs: vec![TxOut {
                value: 5000000000, // 50 ZEC
                script: vec![4, 5, 6],
            }],
            lock_time: 0,
        };

        let tx = TransactionV1::new(raw, txid);
        
        assert_eq!(tx.version(), 1);
        assert_eq!(tx.input_count(), 1);
        assert_eq!(tx.output_count(), 1);
        assert_eq!(tx.lock_time(), 0);
        assert!(tx.is_coinbase());
        assert_eq!(tx.transparent_output_value().unwrap(), 5000000000);
    }

    #[test]
    fn test_v1_validation_empty_transaction() {
        let (_block_context, txid, tx_context) = create_test_context_pre_overwinter();
        
        let raw = RawTransactionV1 {
            transparent_inputs: vec![],
            transparent_outputs: vec![],
            lock_time: 0,
        };

        let result = TransactionV1Reader::validate_and_construct(raw, &tx_context);
        assert!(matches!(result, Err(ValidationError::EmptyTransaction)));
    }

    #[test] 
    fn test_v1_validation_negative_output() {
        let (_block_context, txid, tx_context) = create_test_context_pre_overwinter();
        
        let raw = RawTransactionV1 {
            transparent_inputs: vec![TxIn {
                prev_txid: [1; 32],
                prev_index: 0,
                script: vec![],
            }],
            transparent_outputs: vec![TxOut {
                value: -1000, // Negative value
                script: vec![],
            }],
            lock_time: 0,
        };

        let result = TransactionV1Reader::validate_and_construct(raw, &tx_context);
        assert!(matches!(result, Err(ValidationError::Generic { .. })));
    }
}