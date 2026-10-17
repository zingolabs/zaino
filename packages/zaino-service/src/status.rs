//! Read-result statuses.

use zaino_primitives::types::{Height, TransactionId};

/// Where a snapshot places a transaction. Chain state only — mempool presence
/// is observed through its own stream, not reported here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxStatus {
    Mined(Height),
    Orphaned,
    Unknown,
}

/// Whether the pinned view considers a transparent output spent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpendStatus {
    Unspent,
    Spent {
        by: TransactionId,
    },
    /// Known spent, spender unresolved — caller retries (ZcashFoundation/zebra#10806).
    SpentSpenderUnknown,
    /// No in-view transaction created this outpoint.
    NoSuchOutput,
}
