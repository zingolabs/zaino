//! Node-operator queries relayed to the validator.
//!
//! Some node-operator RPCs (mining, peers, tx-out-set totals) are *not* indexed
//! by Zaino; they are relayed to the validator and returned as-is. The answer
//! is deliberately opaque — Zaino does not parse or model it, which is what
//! keeps these queries out of the indexed domain.

/// A node-operator query Zaino relays to the validator rather than answering
/// from an index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PassthroughQuery {
    /// `getmininginfo`.
    MiningInfo,
    /// `getpeerinfo`.
    PeerInfo,
    /// `gettxoutsetinfo`.
    TxOutSetInfo,
}

/// The validator's answer, relayed opaque. Held as the raw payload because
/// Zaino does not interpret it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PassthroughAnswer(pub String);
