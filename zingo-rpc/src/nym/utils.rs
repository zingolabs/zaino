//! Utility functions for Nym-Proxy

use std::io::Cursor;
use zcash_encoding::CompactSize;

use crate::blockcache::utils::{read_bytes, ParseError};

/// Reads a RPC method name from a Vec<u8> and returns this as a string along with the remaining data in the input.
fn read_nym_method(data: &[u8]) -> Result<(String, &[u8]), ParseError> {
    let mut cursor = Cursor::new(data);
    let method_len = CompactSize::read(&mut cursor)? as usize;
    let method = String::from_utf8(read_bytes(&mut cursor, method_len, "failed to read")?)?;
    Ok((method, &data[cursor.position() as usize..]))
}

/// Extracts metadata from a NymRequest.
pub fn read_nym_request_data(data: &[u8]) -> Result<(u64, String, &[u8]), ParseError> {
    let mut cursor = Cursor::new(data);
    let id = CompactSize::read(&mut cursor)?;
    let (method, data) = read_nym_method(data)?;
    Ok((id, method, data))
}
