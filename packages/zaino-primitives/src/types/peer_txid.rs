//! Unmined transaction as peers name it

use zcash_protocol::TxId;

/// Txid + (v5+) auth digest (`MSG_WTX` inventory)
///
/// - `auth_digest` `None` = pre-v5 (its txid commits to every byte)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PeerTxId {
    pub txid: TxId,
    pub auth_digest: Option<[u8; 32]>,
}
