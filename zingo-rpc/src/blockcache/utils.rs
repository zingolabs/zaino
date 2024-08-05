//! Blockcache utility functionality.

use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use std::io::{self, Cursor, Read, Write};

use crate::blockcache::error::ParseError;

/// Used for decoding zcash blocks from a bytestring.
pub trait ParseFromSlice {
    /// Reads data from a bytestring, consuming data read, and returns an instance of self along with the remaining data in the bytestring given.
    ///
    /// txid is givin as an input as this is taken from a get_block verbose=1 call.
    ///
    /// tx_version is used for deserializing sapling spends and outputs.
    fn parse_from_slice(
        data: &[u8],
        txid: Option<Vec<Vec<u8>>>,
        tx_version: Option<u32>,
    ) -> Result<(&[u8], Self), ParseError>
    where
        Self: Sized;
}

/// Skips the next n bytes in cursor, returns error message given if eof is reached.
pub fn skip_bytes(cursor: &mut Cursor<&[u8]>, n: usize, error_msg: &str) -> Result<(), ParseError> {
    if cursor.get_ref().len() < (cursor.position() + n as u64) as usize {
        return Err(ParseError::InvalidData(error_msg.to_string()));
    }
    cursor.set_position(cursor.position() + n as u64);
    Ok(())
}

/// Reads the next n bytes from cursor into a vec<u8>, returns error message given if eof is reached..
pub fn read_bytes(
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

/// Reads the next 8 bytes from cursor into a u64, returns error message given if eof is reached..
pub fn read_u64(cursor: &mut Cursor<&[u8]>, error_msg: &str) -> Result<u64, ParseError> {
    cursor
        .read_u64::<LittleEndian>()
        .map_err(ParseError::from)
        .map_err(|_| ParseError::InvalidData(error_msg.to_string()))
}

/// Reads the next 4 bytes from cursor into a u32, returns error message given if eof is reached..
pub fn read_u32(cursor: &mut Cursor<&[u8]>, error_msg: &str) -> Result<u32, ParseError> {
    cursor
        .read_u32::<LittleEndian>()
        .map_err(ParseError::from)
        .map_err(|_| ParseError::InvalidData(error_msg.to_string()))
}

/// Reads the next 4 bytes from cursor into an i32, returns error message given if eof is reached..
pub fn read_i32(cursor: &mut Cursor<&[u8]>, error_msg: &str) -> Result<i32, ParseError> {
    cursor
        .read_i32::<LittleEndian>()
        .map_err(ParseError::from)
        .map_err(|_| ParseError::InvalidData(error_msg.to_string()))
}

/// Reads the next byte from cursor into a bool, returns error message given if eof is reached..
pub fn read_bool(cursor: &mut Cursor<&[u8]>, error_msg: &str) -> Result<bool, ParseError> {
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
pub fn read_zcash_script_i64(cursor: &mut Cursor<&[u8]>) -> Result<i64, ParseError> {
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
pub struct CompactSize;

/// The maximum allowed value representable as a `[CompactSize]`
pub const MAX_COMPACT_SIZE: u32 = 0x02000000;

impl CompactSize {
    /// Reads an integer encoded in compact form.
    pub fn read<R: Read>(mut reader: R) -> io::Result<u64> {
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

    /// Reads an integer encoded in contact form and performs checked conversion
    /// to the target type.
    pub fn read_t<R: Read, T: TryFrom<u64>>(mut reader: R) -> io::Result<T> {
        let n = Self::read(&mut reader)?;
        <T>::try_from(n).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "CompactSize value exceeds range of target type.",
            )
        })
    }

    /// Writes the provided `usize` value to the provided Writer in compact form.
    pub fn write<W: Write>(mut writer: W, size: usize) -> io::Result<()> {
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

/// Takes a vec of big endian hex encoded txids and returns them as a vec of little endian raw bytes.
pub fn display_txids_to_server(txids: Vec<String>) -> Result<Vec<Vec<u8>>, ParseError> {
    txids
        .iter()
        .map(|txid| {
            txid.as_bytes()
                .chunks(2)
                .map(|chunk| {
                    let hex_pair = std::str::from_utf8(chunk).map_err(ParseError::from)?;
                    u8::from_str_radix(hex_pair, 16).map_err(ParseError::from)
                })
                .rev()
                .collect::<Result<Vec<u8>, _>>()
        })
        .collect::<Result<Vec<Vec<u8>>, _>>()
}
