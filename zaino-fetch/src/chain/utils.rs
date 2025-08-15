//! Blockcache utility functionality.

use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use std::io::{self, Cursor, Read, Write};

use crate::chain::error::ParseError;

/// Unified parsing trait for hex data and cursor-based parsing.
/// 
/// This trait provides clean parsing by:
/// - Using cursors for stateful parsing within larger contexts (like blocks)
/// - Using hex strings/slices for standalone parsing (like individual transactions)
/// - Using associated types for context instead of Option parameters
/// - Allowing each implementation to specify its exact context requirements
/// - Eliminating runtime parameter validation
/// 
/// ## Implementation Guide
/// 
/// Implementors only need to implement `parse_from_cursor()`. The `parse_from_slice()`
/// method is provided automatically as a wrapper that:
/// 1. Creates a cursor from the input slice
/// 2. Calls `parse_from_cursor()` 
/// 3. Returns both the parsed object and remaining unparsed bytes
/// 
/// This design allows the same parsing logic to work in both contexts:
/// - When parsing within a block (cursor already positioned)
/// - When parsing standalone transactions from hex strings
pub trait ParseFromHex {
    /// The context type required for parsing this type
    type Context;
    
    /// Parse from a cursor with the appropriate context.
    /// 
    /// This is the main method implementors should provide.
    fn parse_from_cursor(
        cursor: &mut std::io::Cursor<&[u8]>,
        context: Self::Context,
    ) -> Result<Self, ParseError>
    where
        Self: Sized;
    
    /// Parse from a byte slice with the appropriate context.
    /// 
    /// Returns the remaining unparsed bytes and the parsed object.
    /// This method is automatically provided and wraps `parse_from_cursor()`.
    fn parse_from_slice(
        data: &[u8],
        context: Self::Context,
    ) -> Result<(&[u8], Self), ParseError>
    where
        Self: Sized,
        Self::Context: Clone,
    {
        let mut cursor = std::io::Cursor::new(data);
        let parsed = Self::parse_from_cursor(&mut cursor, context)?;
        let consumed = cursor.position() as usize;
        let remaining = &data[consumed..];
        Ok((remaining, parsed))
    }
}

/// Skips the next n bytes in cursor, returns error message given if eof is reached.
pub(crate) fn skip_bytes(
    cursor: &mut Cursor<&[u8]>,
    n: usize,
    error_msg: &str,
) -> Result<(), ParseError> {
    if cursor.get_ref().len() < (cursor.position() + n as u64) as usize {
        return Err(ParseError::InvalidData(error_msg.to_string()));
    }
    cursor.set_position(cursor.position() + n as u64);
    Ok(())
}

/// Reads the next n bytes from cursor into a `vec<u8>`, returns error message given if eof is reached.
pub(crate) fn read_bytes(
    cursor: &mut Cursor<&[u8]>,
    n: usize,
    error_msg: &str,
) -> Result<Vec<u8>, ParseError> {
    let mut buf = vec![0; n];
    cursor
        .read_exact(&mut buf)
        .map_err(|_| ParseError::InvalidData(error_msg.to_string()))?;
    Ok(buf)
}

/// Reads the next 8 bytes from cursor into a u64, returns error message given if eof is reached.
pub(crate) fn read_u64(cursor: &mut Cursor<&[u8]>, error_msg: &str) -> Result<u64, ParseError> {
    cursor
        .read_u64::<LittleEndian>()
        .map_err(ParseError::from)
        .map_err(|_| ParseError::InvalidData(error_msg.to_string()))
}

/// Reads the next 4 bytes from cursor into a u32, returns error message given if eof is reached.
pub(crate) fn read_u32(cursor: &mut Cursor<&[u8]>, error_msg: &str) -> Result<u32, ParseError> {
    cursor
        .read_u32::<LittleEndian>()
        .map_err(ParseError::from)
        .map_err(|_| ParseError::InvalidData(error_msg.to_string()))
}

/// Reads the next 8 bytes from cursor into an i64, returns error message given if eof is reached.
pub(crate) fn read_i64(cursor: &mut Cursor<&[u8]>, error_msg: &str) -> Result<i64, ParseError> {
    cursor
        .read_i64::<LittleEndian>()
        .map_err(ParseError::from)
        .map_err(|_| ParseError::InvalidData(error_msg.to_string()))
}

/// Reads the next 4 bytes from cursor into an i32, returns error message given if eof is reached.
pub(crate) fn read_i32(cursor: &mut Cursor<&[u8]>, error_msg: &str) -> Result<i32, ParseError> {
    cursor
        .read_i32::<LittleEndian>()
        .map_err(ParseError::from)
        .map_err(|_| ParseError::InvalidData(error_msg.to_string()))
}

/// Reads the next byte from cursor into a bool, returns error message given if eof is reached.
#[allow(dead_code)]
pub(crate) fn read_bool(cursor: &mut Cursor<&[u8]>, error_msg: &str) -> Result<bool, ParseError> {
    let byte = cursor
        .read_u8()
        .map_err(ParseError::from)
        .map_err(|_| ParseError::InvalidData(error_msg.to_string()))?;
    match byte {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(ParseError::InvalidData(error_msg.to_string())),
    }
}

/// read_zcash_script_int64 OP codes.
const OP_0: u8 = 0x00;
const OP_1_NEGATE: u8 = 0x4f;
const OP_1: u8 = 0x51;
const OP_16: u8 = 0x60;

/// Reads and interprets a Zcash (Bitcoin) custom compact integer encoding used for int64 numbers in scripts.
pub(crate) fn read_zcash_script_i64(cursor: &mut Cursor<&[u8]>) -> Result<i64, ParseError> {
    let first_byte = read_bytes(cursor, 1, "Error reading first byte in i64 script hash")?[0];

    match first_byte {
        OP_1_NEGATE => Ok(-1),
        OP_0 => Ok(0),
        OP_1..=OP_16 => Ok((u64::from(first_byte) - u64::from(OP_1 - 1)) as i64),
        _ => {
            let num_bytes =
                read_bytes(cursor, first_byte as usize, "Error reading i64 script hash")?;
            let number = num_bytes
                .iter()
                .rev()
                .fold(0, |acc, &byte| (acc << 8) | u64::from(byte));
            Ok(number as i64)
        }
    }
}

/// Zcash CompactSize implementation taken from LibRustZcash::zcash_encoding to simplify dependency tree.
///
/// Namespace for functions for compact encoding of integers.
///
/// This codec requires integers to be in the range `0x0..=0x02000000`, for compatibility
/// with Zcash consensus rules.
pub(crate) struct CompactSize;

/// The maximum allowed value representable as a `[CompactSize]`
pub(crate) const MAX_COMPACT_SIZE: u32 = 0x02000000;

impl CompactSize {
    /// Reads an integer encoded in compact form.
    pub(crate) fn read<R: Read>(mut reader: R) -> io::Result<u64> {
        let flag = reader.read_u8()?;
        let result = if flag < 253 {
            Ok(flag as u64)
        } else if flag == 253 {
            match reader.read_u16::<LittleEndian>()? {
                n if n < 253 => Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "non-canonical CompactSize",
                )),
                n => Ok(n as u64),
            }
        } else if flag == 254 {
            match reader.read_u32::<LittleEndian>()? {
                n if n < 0x10000 => Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "non-canonical CompactSize",
                )),
                n => Ok(n as u64),
            }
        } else {
            match reader.read_u64::<LittleEndian>()? {
                n if n < 0x100000000 => Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "non-canonical CompactSize",
                )),
                n => Ok(n),
            }
        }?;

        match result {
            s if s > <u64>::from(MAX_COMPACT_SIZE) => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "CompactSize too large",
            )),
            s => Ok(s),
        }
    }

    /// Reads an integer encoded in compact form and performs checked conversion
    /// to the target type.
    #[allow(dead_code)]
    pub(crate) fn read_t<R: Read, T: TryFrom<u64>>(mut reader: R) -> io::Result<T> {
        let n = Self::read(&mut reader)?;
        <T>::try_from(n).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "CompactSize value exceeds range of target type.",
            )
        })
    }

    /// Writes the provided `usize` value to the provided Writer in compact form.
    pub(crate) fn write<W: Write>(mut writer: W, size: usize) -> io::Result<()> {
        match size {
            s if s < 253 => writer.write_u8(s as u8),
            s if s <= 0xFFFF => {
                writer.write_u8(253)?;
                writer.write_u16::<LittleEndian>(s as u16)
            }
            s if s <= 0xFFFFFFFF => {
                writer.write_u8(254)?;
                writer.write_u32::<LittleEndian>(s as u32)
            }
            s => {
                writer.write_u8(255)?;
                writer.write_u64::<LittleEndian>(s as u64)
            }
        }
    }
}
