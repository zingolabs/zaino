//! Wire <-> domain conversion, owned by the adapter.
//!
//! Both directions live here: `*_from_hex` is the fallible external-input
//! validation (wire -> domain), and `to_hex` / `spend_status_to_wire` are the
//! domain -> wire renderings. No domain crate depends on any wire schema.

use zaino_core::{SpendStatus, TransactionId};

use crate::error::RpcError;

fn hex_val(c: u8) -> Result<u8, RpcError> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(RpcError::InvalidParams(format!(
            "invalid hex digit: {c:#x}"
        ))),
    }
}

/// Decode a hex string to bytes (wire -> domain input validation).
pub(crate) fn bytes_from_hex(s: &str) -> Result<Vec<u8>, RpcError> {
    let bytes = s.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return Err(RpcError::InvalidParams("odd-length hex".into()));
    }
    bytes
        .chunks_exact(2)
        .map(|pair| Ok((hex_val(pair[0])? << 4) | hex_val(pair[1])?))
        .collect()
}

/// Decode a 32-byte transaction id (wire -> domain input validation).
pub(crate) fn txid_from_hex(s: &str) -> Result<TransactionId, RpcError> {
    let arr: [u8; 32] = bytes_from_hex(s)?
        .try_into()
        .map_err(|_| RpcError::InvalidParams("txid must be 32 bytes".into()))?;
    Ok(TransactionId::from(arr))
}

/// Lowercase hex (domain -> wire).
pub(crate) fn to_hex(bytes: [u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Render a spend status as a wire string (domain -> wire). Exhaustive by
/// design — a new `SpendStatus` variant should force a decision here.
pub(crate) fn spend_status_to_wire(status: SpendStatus) -> String {
    match status {
        SpendStatus::Unspent => "unspent".to_string(),
        SpendStatus::Spent { by } => format!("spent:{}", to_hex(by.into())),
        SpendStatus::SpentSpenderUnknown => "spent".to_string(),
        SpendStatus::NoSuchOutput => "none".to_string(),
    }
}
