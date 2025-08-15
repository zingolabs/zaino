//! Block field reading infrastructure.

use std::io::Cursor;
use crate::error::ParseError;
use super::context::BlockParsingContext;

/// Size specification for block fields  
#[derive(Debug, Clone)]
pub enum BlockFieldSize {
    /// Fixed size field (e.g., 4 bytes for version)
    Fixed(usize),
    /// CompactSize-prefixed field
    CompactSize,
    /// Variable size that depends on content
    Variable,
}

/// Core trait for all block fields
pub trait BlockField: Sized {
    /// The type of value this field represents
    type Value;
    
    /// The size specification for this field
    const SIZE: BlockFieldSize;
    
    /// Read this field from the cursor
    fn read_from_cursor(cursor: &mut Cursor<&[u8]>) -> Result<Self::Value, ParseError>;
    
    /// Validate this field value in the given context
    /// Default implementation performs no validation
    fn validate(value: &Self::Value, context: &BlockParsingContext) -> Result<(), ParseError> {
        let _ = (value, context);
        Ok(())
    }
}

/// Helper for ergonomic block field reading with automatic position tracking
pub struct BlockFieldReader<'a> {
    cursor: &'a mut Cursor<&'a [u8]>,
    context: &'a BlockParsingContext,
    position: usize,
    fields_read: Vec<&'static str>,
}

impl<'a> BlockFieldReader<'a> {
    /// Create a new block field reader
    pub fn new(cursor: &'a mut Cursor<&'a [u8]>, context: &'a BlockParsingContext) -> Self {
        Self {
            cursor,
            context,
            position: 0,
            fields_read: Vec::new(),
        }
    }

    /// Read a field with position validation
    /// 
    /// The expected_position parameter enforces reading order - each block
    /// parser can specify exactly what position each field should be read at.
    pub fn read_field<F: BlockField>(&mut self, expected_position: usize) -> Result<F::Value, ParseError> {
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
        if let BlockFieldSize::Fixed(expected_size) = F::SIZE {
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

    /// Get the current cursor position in bytes
    pub fn cursor_position(&self) -> u64 {
        self.cursor.position()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::read_u32;
    use zebra_chain::parameters::Network;

    // Test field implementation
    struct TestVersionField;
    
    impl BlockField for TestVersionField {
        type Value = u32;
        const SIZE: BlockFieldSize = BlockFieldSize::Fixed(4);
        
        fn read_from_cursor(cursor: &mut Cursor<&[u8]>) -> Result<Self::Value, ParseError> {
            read_u32(cursor, "TestVersionField")
        }
        
        fn validate(value: &Self::Value, _context: &BlockParsingContext) -> Result<(), ParseError> {
            if *value < 4 {
                return Err(ParseError::InvalidData(
                    "Block version must be at least 4".to_string()
                ));
            }
            Ok(())
        }
    }

    #[test]
    fn test_block_field_reader_position_validation() {
        let data = [0x04, 0x00, 0x00, 0x00]; // Version 4 as little endian
        let mut cursor = Cursor::new(&data[..]);
        
        let context = BlockParsingContext::new(Network::Mainnet);
        let mut reader = BlockFieldReader::new(&mut cursor, &context);
        
        // Reading at position 0 should work
        let version = reader.read_field::<TestVersionField>(0).unwrap();
        assert_eq!(version, 4);
        
        // Trying to read at wrong position should fail
        let result = reader.read_field::<TestVersionField>(5);
        assert!(matches!(result, Err(ParseError::FieldOrderViolation { .. })));
    }

    #[test]
    fn test_block_field_validation() {
        let data = [0x03, 0x00, 0x00, 0x00]; // Version 3 (invalid)
        let mut cursor = Cursor::new(&data[..]);
        
        let context = BlockParsingContext::new(Network::Mainnet);
        let mut reader = BlockFieldReader::new(&mut cursor, &context);
        
        // Field validation should catch invalid version
        let result = reader.read_field::<TestVersionField>(0);
        assert!(matches!(result, Err(ParseError::InvalidData(_))));
    }

    #[test]
    fn test_block_field_size_validation() {
        let data = [0x04, 0x00]; // Only 2 bytes instead of 4
        let mut cursor = Cursor::new(&data[..]);
        
        let context = BlockParsingContext::new(Network::Mainnet);
        let mut reader = BlockFieldReader::new(&mut cursor, &context);
        
        // Should fail due to insufficient data
        let result = reader.read_field::<TestVersionField>(0);
        assert!(result.is_err());
    }
}