//! A transparent outpoint consumed, at the location that consumed it.

use super::{Height, Outpoint, OutputIndex, TransactionId};

/// One transparent outpoint spent, with where and by what.
///
/// Keyed by the outpoint rather than by an address, which is what lets a tier
/// holding no history report it: an input names the outpoint it consumes, so
/// recognising the spend needs nothing but the outpoint, while naming the
/// address it paid would need the output that created it.
///
/// Carries more than "it was spent" because a balance change has a location: a
/// delta is reported at the height and transaction that caused it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransparentSpend {
    /// The outpoint consumed.
    pub outpoint: Outpoint,
    /// The transaction that consumed it.
    pub by: TransactionId,
    /// Position of the consuming input within that transaction.
    pub input_index: OutputIndex,
    /// Block height that mined the consuming transaction.
    pub height: Height,
    /// Position of the consuming transaction within its block.
    ///
    /// The spend is local data — the tier that reports it holds (or has located)
    /// the spending transaction — so this is the real block position, never a
    /// placeholder. It is the same `txindex` zcashd keys its ordering on, so a
    /// caller can break same-height ties by block position.
    pub block_index: u32,
}
