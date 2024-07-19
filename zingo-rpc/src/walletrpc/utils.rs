//! Utility functions for wallet side nym code.

use crate::blockcache::utils::CompactSize;

use crate::blockcache::utils::ParseError;

/// Serialises gRPC request to a buffer.
pub async fn serialize_request<T: prost::Message>(
    request: &T,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut buf = Vec::new();
    request.encode(&mut buf)?;
    Ok(buf)
}

/// Decodes gRPC request from a buffer
pub async fn deserialize_response<T: prost::Message + Default>(
    data: &[u8],
) -> Result<T, ParseError> {
    T::decode(data).map_err(ParseError::from)
}

/// Prepends an encoded tonic request with metadata required by the Nym server.
///
/// Encodes the request ID as a Zcash CompactSize [u64].
/// Encodes the RPC method String into a Vec<u8> prepended by a Zcash CompactSize indicating its length in bytes.
pub fn write_nym_request_data(id: u64, method: String, body: &[u8]) -> Result<Vec<u8>, ParseError> {
    let method_bytes = method.into_bytes();
    let mut buffer = Vec::new();
    CompactSize::write(&mut buffer, id as usize)?;
    CompactSize::write(&mut buffer, method_bytes.len())?;
    buffer.extend(method_bytes);
    CompactSize::write(&mut buffer, body.len())?;
    buffer.extend(body);
    Ok(buffer)
}
