//! Shared utility functionality for zaino-fetch.

use crate::legacy_parser::error::ParseError;

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

/// Rejects a `tx_version` argument for the Zebra-backed parsers, which take
/// none; `type_name` labels the caller in the error message.
pub(crate) fn reject_tx_version(
    tx_version: Option<u32>,
    type_name: &str,
) -> Result<(), ParseError> {
    if tx_version.is_some() {
        return Err(ParseError::InvalidData(format!(
            "tx_version must be None for {type_name}::parse_from_slice"
        )));
    }
    Ok(())
}

/// Deserializes one Zcash structure from the front of `data`, returning the
/// value together with the number of bytes consumed.
pub(crate) fn zcash_deserialize_consumed<T: zebra_chain::serialization::ZcashDeserialize>(
    data: &[u8],
) -> Result<(T, usize), ParseError> {
    let mut cursor = std::io::Cursor::new(data);
    let value = T::zcash_deserialize(&mut cursor)?;
    let consumed = usize::try_from(cursor.position())?;
    Ok((value, consumed))
}
