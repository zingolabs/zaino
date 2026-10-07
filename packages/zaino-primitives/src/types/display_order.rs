//! 32-byte hashes in RPC display order (byte-reversed hex): [`BlockHash`](super::BlockHash),
//! [`TransactionId`](super::TransactionId)

use core::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ParseHashError {
    #[error("{0} hex digits, not 64")]
    Length(usize),
    #[error("not hex")]
    NotHex,
}

/// Display-order hex → internal-order bytes
pub(crate) fn parse(hex: &str) -> Result<[u8; 32], ParseHashError> {
    if hex.len() != 64 {
        return Err(ParseHashError::Length(hex.len()));
    }
    if !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ParseHashError::NotHex);
    }
    let mut bytes = [0u8; 32];
    for (at, byte) in bytes.iter_mut().rev().enumerate() {
        *byte =
            u8::from_str_radix(&hex[at * 2..at * 2 + 2], 16).map_err(|_| ParseHashError::NotHex)?;
    }
    Ok(bytes)
}

/// Internal-order bytes → display-order hex
pub(crate) fn write(bytes: &[u8; 32], f: &mut fmt::Formatter<'_>) -> fmt::Result {
    for &byte in bytes.iter().rev() {
        write!(f, "{byte:02x}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{BlockHash, TransactionId};

    #[test]
    fn display_order_round_trips_and_rejects_bad_input() {
        let display = "2c7c50c5b6ed3a223ec575e024891141c64d2a6469a8db045d53229e06aa7e7e";
        let hash: BlockHash = display.parse().expect("64 hex digits");
        assert_eq!(<[u8; 32]>::from(hash)[31], 0x2c, "first display byte = last internal");
        assert_eq!(hash.to_string(), display);
        let txid: TransactionId = display.parse().expect("64 hex digits");
        assert_eq!((txid.to_string(), <[u8; 32]>::from(txid)), (display.into(), hash.into()));

        assert_eq!(display[2..].parse::<BlockHash>(), Err(ParseHashError::Length(62)));
        assert_eq!(display.replace('c', "g").parse::<BlockHash>(), Err(ParseHashError::NotHex));
        assert_eq!("é".repeat(32).parse::<TransactionId>(), Err(ParseHashError::NotHex));
    }
}
