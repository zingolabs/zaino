//! Hold error types for the BlockCache and related functionality.

use crate::jsonrpc::error::JsonRpcConnectorError;

/// Parser Error Type.
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    /// Io Error.
    #[error("IO Error: {0}")]
    Io(#[from] std::io::Error),

    /// Invalid Data Error
    #[error("Invalid Data Error: {0}")]
    InvalidData(String),

    // /// Errors from the JsonRPC client.
    // #[error("JsonRPC Connector Error: {0}")]
    // JsonRpcError(#[from] JsonRpcConnectorError),
    /// UTF-8 conversion error.
    #[error("UTF-8 Error: {0}")]
    Utf8Error(#[from] std::str::Utf8Error),

    /// UTF-8 conversion error.
    #[error("UTF-8 Conversion Error: {0}")]
    FromUtf8Error(#[from] std::string::FromUtf8Error),

    /// Hexadecimal parsing error.
    #[error("Hex Parse Error: {0}")]
    ParseIntError(#[from] std::num::ParseIntError),

    /// Errors originating from prost decodings.
    #[error("Prost Decode Error: {0}")]
    ProstDecodeError(#[from] prost::DecodeError),
}

/// Parser Error Type.
#[derive(Debug, thiserror::Error)]
pub enum BlockCacheError {
    /// Serialization and deserialization error.
    #[error("Parser Error: {0}")]
    ParseError(#[from] ParseError),
    /// Errors from the JsonRPC client.
    #[error("JsonRPC Connector Error: {0}")]
    JsonRpcError(#[from] JsonRpcConnectorError),
}

/// Mempool Error struct.
#[derive(thiserror::Error, Debug)]
pub enum MempoolError {
    /// Errors from the JsonRPC client.
    #[error("JsonRPC Connector Error: {0}")]
    JsonRpcError(#[from] JsonRpcConnectorError),
}
