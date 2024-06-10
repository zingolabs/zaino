//! Blockcache utility functionality.

use byteorder::{LittleEndian, ReadBytesExt};
use std::io::{Cursor, Read};

/// Parser Error Type.
#[derive(Debug)]
pub enum ParseError {
    Io(std::io::Error),
    InvalidData(String),
}

impl From<std::io::Error> for ParseError {
    fn from(err: std::io::Error) -> ParseError {
        ParseError::Io(err)
    }
}

pub trait ParseFromSlice {
    fn parse_from_slice(data: &[u8], txid: Option<Vec<u8>>) -> Result<(&[u8], Self), ParseError>
    where
        Self: Sized;
}

pub fn skip_bytes(cursor: &mut Cursor<&[u8]>, n: usize, error_msg: &str) -> Result<(), ParseError> {
    if cursor.get_ref().len() < (cursor.position() + n as u64) as usize {
        return Err(ParseError::InvalidData(error_msg.to_string()));
    }
    cursor.set_position(cursor.position() + n as u64);
    Ok(())
}

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

pub fn read_u64(cursor: &mut Cursor<&[u8]>, error_msg: &str) -> Result<u64, ParseError> {
    cursor
        .read_u64::<LittleEndian>()
        .map_err(ParseError::from)
        .map_err(|_| ParseError::InvalidData(error_msg.to_string()))
}

pub fn read_u32(cursor: &mut Cursor<&[u8]>, error_msg: &str) -> Result<u32, ParseError> {
    cursor
        .read_u32::<LittleEndian>()
        .map_err(ParseError::from)
        .map_err(|_| ParseError::InvalidData(error_msg.to_string()))
}

pub fn read_i32(cursor: &mut Cursor<&[u8]>, error_msg: &str) -> Result<i32, ParseError> {
    cursor
        .read_i32::<LittleEndian>()
        .map_err(ParseError::from)
        .map_err(|_| ParseError::InvalidData(error_msg.to_string()))
}

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
