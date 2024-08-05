//! Utility functions for Nym-Proxy

use crate::{
    blockcache::{
        error::ParseError,
        utils::{read_bytes, CompactSize},
    },
    nym::error::NymError,
};
use std::io::Cursor;

/// Reads a RPC method name from a Vec<u8> and returns this as a string along with the remaining data in the input.
fn read_nym_method(data: &[u8]) -> Result<(String, &[u8]), NymError> {
    let mut cursor = Cursor::new(data);
    let method_len = CompactSize::read(&mut cursor).map_err(ParseError::Io)? as usize;
    let method = String::from_utf8(read_bytes(&mut cursor, method_len, "failed to read")?)
        .map_err(ParseError::FromUtf8Error)?;
    Ok((method, &data[cursor.position() as usize..]))
}

/// Check the body of the request is the correct length.
fn check_nym_body(data: &[u8]) -> Result<&[u8], NymError> {
    let mut cursor = Cursor::new(data);
    let body_len = CompactSize::read(&mut cursor).map_err(ParseError::Io)? as usize;
    if &body_len != &data[cursor.position() as usize..].len() {
        return Err(NymError::ParseError(ParseError::InvalidData(
            "Incorrect request body size read.".to_string(),
        )));
    };
    Ok(&data[cursor.position() as usize..])
}

/// Extracts metadata from a NymRequest.
///
/// Returns [ID, Method, RequestData].
pub fn read_nym_request_data(data: &[u8]) -> Result<(u64, String, &[u8]), NymError> {
    let mut cursor = Cursor::new(data);
    let id = CompactSize::read(&mut cursor).map_err(ParseError::Io)?;
    let (method, data) = read_nym_method(&data[cursor.position() as usize..])?;
    let body = check_nym_body(data)?;
    Ok((id, method, body))
}
