//! Response parsing: JSON-RPC `serde_json::Value` → zaino-primitives types.
//!
//! Each function corresponds to one RPC method's response format as
//! returned by Zebra.

use zaino_primitives::types::{BlockHash, Height, Treestate};

/// Parse a `getblock(height, 0)` response — hex-encoded raw block bytes.
pub(crate) fn parse_raw_block(value: &serde_json::Value) -> Result<Vec<u8>, ParseError> {
    let hex_str = value
        .as_str()
        .ok_or_else(|| ParseError::unexpected("string", value))?;
    hex::decode(hex_str).map_err(|e| ParseError::Hex(e.to_string()))
}

/// Parse a `getbestblockhash` response — hex-encoded block hash.
pub(crate) fn parse_block_hash(value: &serde_json::Value) -> Result<BlockHash, ParseError> {
    let hex_str = value
        .as_str()
        .ok_or_else(|| ParseError::unexpected("string", value))?;
    let bytes = hex::decode(hex_str).map_err(|e| ParseError::Hex(e.to_string()))?;
    if bytes.len() != 32 {
        return Err(ParseError::WrongLength {
            expected: 32,
            got: bytes.len(),
        });
    }
    // RPC returns big-endian (display order), BlockHash stores little-endian.
    let mut le = [0u8; 32];
    le.copy_from_slice(&bytes);
    le.reverse();
    Ok(BlockHash::from(le))
}

/// Parse a `getblockcount` response — integer height.
pub(crate) fn parse_height(value: &serde_json::Value) -> Result<Height, ParseError> {
    let n = value
        .as_u64()
        .ok_or_else(|| ParseError::unexpected("u64", value))?;
    let h = u32::try_from(n).map_err(|_| ParseError::Overflow(n))?;
    Height::try_from(h).map_err(|e| ParseError::Height(e.to_string()))
}

/// Parse a `z_gettreestate` response.
pub(crate) fn parse_treestate(value: &serde_json::Value) -> Result<Treestate, ParseError> {
    let sapling = value
        .get("sapling")
        .and_then(|s| s.get("commitments"))
        .and_then(|c| c.get("finalState"))
        .and_then(|v| v.as_str())
        .map(hex::decode)
        .transpose()
        .map_err(|e| ParseError::Hex(e.to_string()))?;

    let orchard = value
        .get("orchard")
        .and_then(|s| s.get("commitments"))
        .and_then(|c| c.get("finalState"))
        .map(|v| {
            v.as_str()
                .ok_or_else(|| ParseError::unexpected("string", v))
                .and_then(|hex_str| {
                    hex::decode(hex_str).map_err(|e| ParseError::Hex(e.to_string()))
                })
        })
        .transpose()?;

    Ok(Treestate { sapling, orchard })
}

/// Errors from parsing RPC responses.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ParseError {
    /// Hex decoding failed.
    #[error("hex decode: {0}")]
    Hex(String),

    /// Unexpected JSON type.
    #[error("expected {expected}, got {got}")]
    UnexpectedType {
        /// What we expected.
        expected: &'static str,
        /// What we got (truncated).
        got: String,
    },

    /// Byte array wrong length.
    #[error("expected {expected} bytes, got {got}")]
    WrongLength {
        /// Expected length.
        expected: usize,
        /// Actual length.
        got: usize,
    },

    /// Value too large.
    #[error("value {0} overflows target type")]
    Overflow(u64),

    /// Height validation failed.
    #[error("invalid height: {0}")]
    Height(String),

    /// Block deserialization failed.
    #[error("deserialize: {0}")]
    Deserialize(String),
}

impl ParseError {
    fn unexpected(expected: &'static str, value: &serde_json::Value) -> Self {
        let got = format!("{value}").chars().take(64).collect();
        Self::UnexpectedType { expected, got }
    }
}
