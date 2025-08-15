//! Transaction field reading infrastructure.

use std::io::Cursor;
use crate::error::ParseError;
use super::context::TransactionContext;

/// Size specification for transaction fields
#[derive(Debug, Clone)]
pub enum FieldSize {
    /// Fixed size field (e.g., 4 bytes for u32)
    Fixed(usize),
    /// CompactSize-prefixed field
    CompactSize,
    /// Array with CompactSize count prefix
    CompactSizeArray,
    /// Variable size that depends on context
    Variable,
}

/// Core trait for all transaction fields
pub trait TransactionField: Sized {
    /// The type of value this field represents
    type Value;
    
    /// The size specification for this field
    const SIZE: FieldSize;
    
    /// Read this field from the cursor
    fn read_from_cursor(cursor: &mut Cursor<&[u8]>) -> Result<Self::Value, ParseError>;
    
    /// Validate this field value in the given context
    /// Default implementation performs no validation
    fn validate(value: &Self::Value, context: &TransactionContext) -> Result<(), ParseError> {
        let _ = (value, context);
        Ok(())
    }
}

/// Helper for ergonomic field reading with automatic position tracking
pub struct FieldReader<'a> {
    cursor: &'a mut Cursor<&'a [u8]>,
    context: &'a TransactionContext<'a>,
    position: usize,
    fields_read: Vec<&'static str>,
}

impl<'a> FieldReader<'a> {
    /// Create a new field reader
    pub fn new(cursor: &'a mut Cursor<&'a [u8]>, context: &'a TransactionContext<'a>) -> Self {
        Self {
            cursor,
            context,
            position: 0,
            fields_read: Vec::new(),
        }
    }

    /// Read a field with position validation
    /// 
    /// The expected_position parameter enforces reading order - each version
    /// can specify exactly what position each field should be read at.
    pub fn read_field<F: TransactionField>(&mut self, expected_position: usize) -> Result<F::Value, ParseError> {
        // Validate reading order
        if self.position != expected_position {
            return Err(ParseError::FieldOrderViolation {
                expected: expected_position,
                actual: self.position,
                field: std::any::type_name::<F>(),
                fields_read: self.fields_read.clone(),
            });
        }

        // Store position before reading for size validation
        let position_before = self.cursor.position();
        
        // Read the field value
        let value = F::read_from_cursor(self.cursor)?;
        
        // Validate expected size was consumed (for fixed-size fields)
        let bytes_consumed = self.cursor.position() - position_before;
        if let FieldSize::Fixed(expected_size) = F::SIZE {
            if bytes_consumed != expected_size as u64 {
                return Err(ParseError::FieldSizeMismatch {
                    field: std::any::type_name::<F>(),
                    expected: expected_size,
                    actual: bytes_consumed as usize,
                });
            }
        }

        // Perform field-specific validation
        F::validate(&value, self.context)?;

        // Update tracking
        self.position += 1;
        self.fields_read.push(std::any::type_name::<F>());

        Ok(value)
    }

    /// Get the current position (for debugging)
    pub fn position(&self) -> usize {
        self.position
    }

    /// Get the list of fields read so far (for debugging)
    pub fn fields_read(&self) -> &[&'static str] {
        &self.fields_read
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::context::{BlockContext, TransactionContext};
    use std::io::Cursor;
    use zebra_chain::{block::Hash, parameters::Network};

    // Test field implementation
    struct TestU32Field;
    
    impl TransactionField for TestU32Field {
        type Value = u32;
        const SIZE: FieldSize = FieldSize::Fixed(4);
        
        fn read_from_cursor(cursor: &mut Cursor<&[u8]>) -> Result<Self::Value, ParseError> {
            use crate::utils::read_u32;
            read_u32(cursor, "TestU32Field")
        }
    }

    #[test]
    fn test_field_reader_position_validation() {
        let data = [0x01, 0x02, 0x03, 0x04]; // 4 bytes for u32
        let mut cursor = Cursor::new(&data[..]);
        
        let block_context = BlockContext::test_context();
        let txid = [1; 32];
        let tx_context = TransactionContext::new(0, &txid, &block_context);
        
        let mut reader = FieldReader::new(&mut cursor, &tx_context);
        
        // Reading at position 0 should work
        let value = reader.read_field::<TestU32Field>(0).unwrap();
        assert_eq!(value, 0x04030201); // Little endian
        
        // Trying to read at wrong position should fail
        let result = reader.read_field::<TestU32Field>(5);
        assert!(matches!(result, Err(ParseError::FieldOrderViolation { .. })));
    }
}