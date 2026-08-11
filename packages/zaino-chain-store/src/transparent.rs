//! Transparent address history, as the finalised range can report it.
//!
//! **Declared, not implemented.** Nothing here has an implementation and no
//! consumer is wired to it. The module states the boundary while the
//! capability is built, and is shaped to mirror
//! `zaino_chain_head::transparent`, so a consumer merging the two halves meets
//! one shape rather than two.
//!
//! The store contributes the *effects* it can see below the finalised
//! watermark. It never constructs a complete history: an address's full
//! record is the store's contribution merged with the recent window's, and
//! neither half alone is an answer.

use zaino_primitives::types::{BlockTxPosition, Height, Outpoint, TransactionId};

use crate::output::{StoredAddress, StoredTxOut};

/// What to report on, and over what range.
///
/// The range is not optional. The store answers for heights it holds and the
/// recent window answers for the rest; an unbounded query would overlap
/// whatever it is merged with and double-count the overlap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransparentHistoryQuery {
    /// The addresses to report on, as the store keys them.
    ///
    /// [`StoredAddress`] rather than a wallet-facing address type, because the
    /// store indexes outputs that have no address at all and a caller may
    /// legitimately ask about one.
    pub addresses: Vec<StoredAddress>,
    /// Lowest height to include.
    pub start: Height,
    /// Highest height to include.
    pub end: Height,
}

/// An output the store holds, and where it was created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocatedOutput {
    /// The outpoint that names it.
    pub outpoint: Outpoint,
    /// Its value and address key.
    pub output: StoredTxOut,
    /// Where the transaction creating it sits.
    pub position: BlockTxPosition,
    /// That transaction's identifier.
    pub txid: TransactionId,
}

/// An output the store has seen spent, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocatedSpend {
    /// The outpoint that was spent.
    pub outpoint: Outpoint,
    /// What it held, so a consumer can account for the value without a second
    /// lookup.
    pub output: StoredTxOut,
    /// Where the spending transaction sits.
    pub position: BlockTxPosition,
    /// That transaction's identifier.
    pub txid: TransactionId,
}

/// What the finalised range shows happening to a set of addresses.
///
/// Outputs and spends, kept apart rather than netted into a balance.
/// Deliberately not a balance or a delta: those are whole-chain answers, and
/// this is a contribution to one. A net figure also cannot be merged — a
/// consumer reconciling a spend whose output lies on the other side of the
/// seam needs the individual effects, not their sum.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct StoreAddressEffects {
    /// Outputs created within the queried range.
    pub outputs: Vec<LocatedOutput>,
    /// Spends the store observed within the queried range.
    ///
    /// Includes spends of outputs created outside the range: the store knows
    /// the output because it holds the whole finalised chain, not just the
    /// queried window.
    pub spends: Vec<LocatedSpend>,
}

impl StoreAddressEffects {
    /// The net value change these effects describe.
    ///
    /// A convenience for a consumer that has already merged both halves and
    /// wants a figure. Meaningless on one half alone, which is why it is a
    /// method here rather than a field: a field would invite reading it off an
    /// unmerged contribution.
    pub fn net_value(&self) -> i64 {
        let received: i64 = self
            .outputs
            .iter()
            .map(|o| u64::from(o.output.value) as i64)
            .sum();
        let spent: i64 = self
            .spends
            .iter()
            .map(|s| u64::from(s.output.value) as i64)
            .sum();
        received - spent
    }

    /// Whether the store observed nothing at all.
    pub fn is_empty(&self) -> bool {
        self.outputs.is_empty() && self.spends.is_empty()
    }
}
