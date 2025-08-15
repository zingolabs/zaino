//! Version-specific transaction reader trait.

use std::io::Cursor;
use crate::error::{ParseError, ValidationError};
use super::context::TransactionContext;

/// Trait for version-specific transaction readers
/// 
/// Each transaction version implements this trait to define its specific
/// parsing and validation logic while providing a common API through
/// the default `parse` method.
pub trait TransactionVersionReader {
    /// Raw parsed transaction data (before validation)
    type RawResult;
    
    /// Final validated transaction type
    type FinalResult;
    
    /// Read raw transaction data from cursor
    /// 
    /// This method should use FieldReader to read fields in the correct
    /// order for this transaction version.
    fn read_raw(
        cursor: &mut Cursor<&[u8]>, 
        context: &TransactionContext
    ) -> Result<Self::RawResult, ParseError>;
    
    /// Validate raw data and construct final transaction
    /// 
    /// This method performs version-specific validation and constructs
    /// the final transaction type with any computed fields.
    fn validate_and_construct(
        raw: Self::RawResult,
        context: &TransactionContext
    ) -> Result<Self::FinalResult, ValidationError>;
    
    /// Parse transaction (default implementation)
    /// 
    /// This provides a common API for all transaction versions that
    /// orchestrates the read + validate + construct flow.
    fn parse(
        cursor: &mut Cursor<&[u8]>, 
        context: &TransactionContext
    ) -> Result<Self::FinalResult, ParseError> {
        // Read raw data
        let raw = Self::read_raw(cursor, context)?;
        
        // Validate and construct final transaction
        let final_tx = Self::validate_and_construct(raw, context)
            .map_err(ParseError::Validation)?;
        
        Ok(final_tx)
    }
}

/// Peek at transaction version without advancing cursor
pub fn peek_transaction_version(cursor: &mut Cursor<&[u8]>) -> Result<u32, ParseError> {
    use crate::utils::read_u32;
    
    let position_before = cursor.position();
    
    // Read header to extract version
    let header = read_u32(cursor, "transaction header")?;
    
    // Reset cursor position
    cursor.set_position(position_before);
    
    // Extract version (lower 31 bits)
    let version = header & 0x7FFFFFFF;
    
    Ok(version)
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::context::{BlockContext, TransactionContext};
    use std::io::Cursor;

    #[test]
    fn test_peek_transaction_version() {
        // Test data with version 4 transaction header
        let data = [0x04, 0x00, 0x00, 0x80]; // Version 4 with overwinter flag
        let mut cursor = Cursor::new(&data[..]);
        
        let version = peek_transaction_version(&mut cursor).unwrap();
        assert_eq!(version, 4);
        
        // Cursor should be back at original position
        assert_eq!(cursor.position(), 0);
    }

    #[test]
    fn test_peek_transaction_version_v1() {
        // Test data with version 1 transaction header (no overwinter flag)
        let data = [0x01, 0x00, 0x00, 0x00]; // Version 1
        let mut cursor = Cursor::new(&data[..]);
        
        let version = peek_transaction_version(&mut cursor).unwrap();
        assert_eq!(version, 1);
        
        // Cursor should be back at original position
        assert_eq!(cursor.position(), 0);
    }
}