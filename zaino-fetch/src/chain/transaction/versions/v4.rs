//! Version 4 transaction implementation.
//!
//! V4 transactions add Sapling shielded components and Overwinter features:
//! - Overwinter flag (part of header)
//! - Version group ID
//! - Expiry height
//! - Transparent inputs/outputs (same as V1)
//! - Value balance (for Sapling)
//! - Shielded spends
//! - Shielded outputs
//! - Join split pubkey and signature (if present)
//! - Binding signature

use std::io::Cursor;
use crate::error::{ParseError, ValidationError};
use crate::chain::transaction::{
    context::{TransactionContext, TxId},
    reader::FieldReader,
    version_reader::TransactionVersionReader,
    fields::{
        VersionGroupId, TransparentInputs, TransparentOutputs, LockTime, ExpiryHeight,
        ValueBalanceSapling, ShieldedSpends, ShieldedOutputs, TxIn, TxOut
    },
};

/// Raw V4 transaction data (before validation)
#[derive(Debug, Clone)]
pub struct RawTransactionV4 {
    pub version_group_id: u32,
    pub transparent_inputs: Vec<TxIn>,
    pub transparent_outputs: Vec<TxOut>,
    pub lock_time: u32,
    pub expiry_height: u32,
    pub value_balance_sapling: i64,
    pub shielded_spends: Vec<u8>, // Placeholder - will be proper Spend type
    pub shielded_outputs: Vec<u8>, // Placeholder - will be proper Output type
    // TODO: Add join split data and binding signature when implementing full V4 support
}

/// Final V4 transaction (validated and with convenience methods)
#[derive(Debug, Clone)]
pub struct TransactionV4 {
    raw: RawTransactionV4,
    txid: TxId,
}

impl TransactionV4 {
    /// Create a new V4 transaction
    pub fn new(raw: RawTransactionV4, txid: TxId) -> Self {
        Self { raw, txid }
    }

    /// Get the transaction ID
    pub fn txid(&self) -> &TxId {
        &self.txid
    }

    /// Get the version group ID
    pub fn version_group_id(&self) -> u32 {
        self.raw.version_group_id
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

    /// Get the expiry height
    pub fn expiry_height(&self) -> u32 {
        self.raw.expiry_height
    }

    /// Get the Sapling value balance
    pub fn value_balance_sapling(&self) -> i64 {
        self.raw.value_balance_sapling
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

    /// Check if transaction has any shielded components
    pub fn has_shielded_components(&self) -> bool {
        !self.raw.shielded_spends.is_empty() || !self.raw.shielded_outputs.is_empty()
    }

    /// Access to raw transaction data
    pub fn raw(&self) -> &RawTransactionV4 {
        &self.raw
    }

    /// Get the transaction version (always 4 for V4)
    pub fn version(&self) -> u32 {
        4
    }
}

/// V4 transaction reader
pub struct TransactionV4Reader;

impl TransactionVersionReader for TransactionV4Reader {
    type RawResult = RawTransactionV4;
    type FinalResult = TransactionV4;

    fn read_raw(
        cursor: &mut Cursor<&[u8]>, 
        context: &TransactionContext
    ) -> Result<Self::RawResult, ParseError> {
        let mut reader = FieldReader::new(cursor, context);

        // V4 field order: version_group_id → inputs → outputs → lock_time → 
        // expiry_height → value_balance → shielded_spends → shielded_outputs
        let version_group_id = reader.read_field::<VersionGroupId>(0)?;
        let transparent_inputs = reader.read_field::<TransparentInputs>(1)?;
        let transparent_outputs = reader.read_field::<TransparentOutputs>(2)?;
        let lock_time = reader.read_field::<LockTime>(3)?;
        let expiry_height = reader.read_field::<ExpiryHeight>(4)?;
        let value_balance_sapling = reader.read_field::<ValueBalanceSapling>(5)?;
        let shielded_spends = reader.read_field::<ShieldedSpends>(6)?;
        let shielded_outputs = reader.read_field::<ShieldedOutputs>(7)?;

        Ok(RawTransactionV4 {
            version_group_id,
            transparent_inputs,
            transparent_outputs,
            lock_time,
            expiry_height,
            value_balance_sapling,
            shielded_spends,
            shielded_outputs,
        })
    }

    fn validate_and_construct(
        raw: Self::RawResult,
        context: &TransactionContext
    ) -> Result<Self::FinalResult, ValidationError> {
        // V4-specific validation

        // Must have at least one input or output (transparent or shielded)
        if raw.transparent_inputs.is_empty() && 
           raw.transparent_outputs.is_empty() &&
           raw.shielded_spends.is_empty() &&
           raw.shielded_outputs.is_empty() {
            return Err(ValidationError::EmptyTransaction);
        }

        // V4 transactions must be overwinter compatible if overwinter is active
        if context.block_context.is_overwinter_active() {
            // Version group ID should have been validated by the VersionGroupId field
            // Additional overwinter-specific validation can go here
        }

        // Sapling must be active for V4 transactions
        if !context.block_context.is_sapling_active() {
            return Err(ValidationError::Generic {
                message: "V4 transactions require Sapling activation".to_string(),
            });
        }

        // Expiry height validation (should be greater than current height if non-zero)
        if raw.expiry_height > 0 && raw.expiry_height <= context.block_context.height {
            return Err(ValidationError::Generic {
                message: format!(
                    "Transaction expired: expiry height {} <= current height {}",
                    raw.expiry_height,
                    context.block_context.height
                ),
            });
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

        // Value balance validation: if there are no shielded components,
        // value balance should be zero
        if raw.shielded_spends.is_empty() && raw.shielded_outputs.is_empty() {
            if raw.value_balance_sapling != 0 {
                return Err(ValidationError::Generic {
                    message: format!(
                        "Non-zero value balance {} with no shielded components",
                        raw.value_balance_sapling
                    ),
                });
            }
        }

        // Construct final transaction
        Ok(TransactionV4::new(raw, context.txid.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::transaction::context::{BlockContext, ActivationHeights, TransactionContext};
    use zebra_chain::{block::Hash, parameters::Network};
    use std::io::Cursor;

    fn create_test_context_sapling_active() -> (BlockContext, TxId, TransactionContext<'static>) {
        let block_context = BlockContext::new(
            500000, // After sapling activation
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
    fn test_v4_transaction_creation() {
        let (_block_context, txid, _tx_context) = create_test_context_sapling_active();
        
        let raw = RawTransactionV4 {
            version_group_id: 0x892F2085, // Sapling version group ID
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
            expiry_height: 500010, // Future height
            value_balance_sapling: 0,
            shielded_spends: vec![],
            shielded_outputs: vec![],
        };

        let tx = TransactionV4::new(raw, txid);
        
        assert_eq!(tx.version(), 4);
        assert_eq!(tx.version_group_id(), 0x892F2085);
        assert_eq!(tx.input_count(), 1);
        assert_eq!(tx.output_count(), 1);
        assert_eq!(tx.lock_time(), 0);
        assert_eq!(tx.expiry_height(), 500010);
        assert_eq!(tx.value_balance_sapling(), 0);
        assert!(tx.is_coinbase());
        assert!(!tx.has_shielded_components());
        assert_eq!(tx.transparent_output_value().unwrap(), 5000000000);
    }

    #[test]
    fn test_v4_validation_empty_transaction() {
        let (_block_context, txid, tx_context) = create_test_context_sapling_active();
        
        let raw = RawTransactionV4 {
            version_group_id: 0x892F2085,
            transparent_inputs: vec![],
            transparent_outputs: vec![],
            lock_time: 0,
            expiry_height: 0,
            value_balance_sapling: 0,
            shielded_spends: vec![],
            shielded_outputs: vec![],
        };

        let result = TransactionV4Reader::validate_and_construct(raw, &tx_context);
        assert!(matches!(result, Err(ValidationError::EmptyTransaction)));
    }

    #[test]
    fn test_v4_validation_expired_transaction() {
        let (_block_context, txid, tx_context) = create_test_context_sapling_active();
        
        let raw = RawTransactionV4 {
            version_group_id: 0x892F2085,
            transparent_inputs: vec![TxIn {
                prev_txid: [1; 32],
                prev_index: 0,
                script: vec![],
            }],
            transparent_outputs: vec![],
            lock_time: 0,
            expiry_height: 400000, // Past height (before current 500000)
            value_balance_sapling: 0,
            shielded_spends: vec![],
            shielded_outputs: vec![],
        };

        let result = TransactionV4Reader::validate_and_construct(raw, &tx_context);
        assert!(matches!(result, Err(ValidationError::Generic { .. })));
    }

    #[test]
    fn test_v4_validation_invalid_value_balance() {
        let (_block_context, txid, tx_context) = create_test_context_sapling_active();
        
        let raw = RawTransactionV4 {
            version_group_id: 0x892F2085,
            transparent_inputs: vec![TxIn {
                prev_txid: [1; 32],
                prev_index: 0,
                script: vec![],
            }],
            transparent_outputs: vec![TxOut {
                value: 1000,
                script: vec![],
            }],
            lock_time: 0,
            expiry_height: 0,
            value_balance_sapling: 1000, // Non-zero with no shielded components
            shielded_spends: vec![],
            shielded_outputs: vec![],
        };

        let result = TransactionV4Reader::validate_and_construct(raw, &tx_context);
        assert!(matches!(result, Err(ValidationError::Generic { .. })));
    }
}