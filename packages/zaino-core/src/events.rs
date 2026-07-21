//! Stream payloads for the tip and mempool subscriptions.

use zaino_primitives::types::TransactionHash;

use crate::refs::BlockId;

/// The best chain moved; carries the new tip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TipEvent {
    pub tip: BlockId,
}

/// A mempool delivery, tagged with the tip it was validated against (ADR-0001).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MempoolTx {
    pub txid: TransactionHash,
    pub validated_against: BlockId,
}
