//! Utility functions for wallet side nym code.

use zaino_nym::error::NymError;
use zaino_fetch::chain::{error::ParseError, utils::CompactSize};

/// Serialises gRPC request to a buffer.
pub async fn serialize_request<T: prost::Message>(
    request: &T,
) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    request.encode(&mut buf)?;
    Ok(buf)
}

/// Decodes gRPC request from a buffer
pub async fn deserialize_response<T: prost::Message + Default>(data: &[u8]) -> Result<T, NymError> {
    T::decode(data).map_err(|e| NymError::from(ParseError::from(e)))
}

/// Prepends an encoded tonic request with metadata required by the Nym server.
///
/// Encodes the request ID as a Zcash CompactSize [u64].
/// Encodes the RPC method String into a Vec<u8> prepended by a Zcash CompactSize indicating its length in bytes.
pub fn write_nym_request_data(id: u64, method: String, body: &[u8]) -> Result<Vec<u8>, NymError> {
    let method_bytes = method.into_bytes();
    let mut buffer = Vec::new();
    CompactSize::write(&mut buffer, id as usize).map_err(ParseError::Io)?;
    CompactSize::write(&mut buffer, method_bytes.len()).map_err(ParseError::Io)?;
    buffer.extend(method_bytes);
    CompactSize::write(&mut buffer, body.len()).map_err(ParseError::Io)?;
    buffer.extend(body);
    Ok(buffer)
}
