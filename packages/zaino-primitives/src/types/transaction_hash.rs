//! Transaction id

use core::{fmt, str::FromStr};

use super::display_order::{self, ParseHashError};

/// Internal byte order, display reversed (as [`super::BlockHash`])
///
/// - Pre-v5: SHA-256d of the bytes; v5+: [ZIP-244](https://zips.z.cash/zip-0244) id
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TransactionId([u8; 32]);

/// Unmined transaction as peers name it (`MSG_WTX` inventory): txid + (v5+) auth digest
///
/// - `auth_digest` `None` = pre-v5 (its txid commits to every byte)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PeerTxId {
    pub txid: TransactionId,
    pub auth_digest: Option<[u8; 32]>,
}

impl From<[u8; 32]> for TransactionId {
    fn from(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl From<TransactionId> for [u8; 32] {
    fn from(h: TransactionId) -> Self {
        h.0
    }
}

impl fmt::Debug for TransactionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        display_order::debug("TransactionId", &self.0, f)
    }
}

impl fmt::Display for TransactionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        display_order::write(&self.0, f)
    }
}

impl FromStr for TransactionId {
    type Err = ParseHashError;

    fn from_str(hex: &str) -> Result<Self, Self::Err> {
        display_order::parse(hex).map(Self)
    }
}
