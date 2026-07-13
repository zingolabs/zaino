//! Shared utility functionality for zaino-fetch.

use crate::chain::error::ParseError;

/// Used for decoding Zcash structures from a bytestring.
pub trait ParseFromSlice {
    /// Reads data from a bytestring, consuming data read, and returns an instance of self along with the remaining data in the bytestring given.
    ///
    /// `txid` is accepted for compatibility with callers that already fetched transaction ids from a verbose block RPC response.
    ///
    /// `tx_version` is retained for compatibility with the legacy parser API and should be `None` for Zebra-backed parsing.
    fn parse_from_slice(
        data: &[u8],
        txid: Option<Vec<Vec<u8>>>,
        tx_version: Option<u32>,
    ) -> Result<(&[u8], Self), ParseError>
    where
        Self: Sized;
}
