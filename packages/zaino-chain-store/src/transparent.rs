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

use zaino_primitives::types::{
    BlockTxPosition, Height, Outpoint, SignedZatoshis, TransactionId, Zatoshis,
};

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
    /// The net value change these effects describe, or `None` if either side
    /// sums past the money supply.
    ///
    /// A convenience for a consumer that has already merged both halves and
    /// wants a figure. Meaningless on one half alone, which is why it is a
    /// method here rather than a field: a field would invite reading it off an
    /// unmerged contribution.
    ///
    /// # Why it is typed and fallible
    ///
    /// A [`SignedZatoshis`] rather than a bare `i64`, because this is the
    /// domain's signed-delta quantity and a raw integer invites the arithmetic
    /// that produced it to be redone somewhere else with different rules.
    ///
    /// `None` rather than a wrapped total, because the individual values come
    /// off disk. Each is bounded by the supply on its own, so no single one can
    /// overflow the accumulator — but a corrupt or adversarial effect set can
    /// hold enough of them to sum past it, and an unchecked `sum::<i64>()`
    /// would report that as a plausible figure rather than as the corruption it
    /// is. Checking each addition is what makes the bound the type claims
    /// actually hold across the fold.
    ///
    /// The subtraction needs no check: both sides are bounded by the supply,
    /// which is three orders of magnitude below `i64::MAX`, so their difference
    /// is always representable.
    pub fn net_value(&self) -> Option<SignedZatoshis> {
        let received = total(self.outputs.iter().map(|output| output.output.value))?;
        let spent = total(self.spends.iter().map(|spend| spend.output.value))?;

        Some(SignedZatoshis::new(signed(received) - signed(spent)))
    }

    /// Whether the store observed nothing at all.
    pub fn is_empty(&self) -> bool {
        self.outputs.is_empty() && self.spends.is_empty()
    }
}

/// Sums amounts, refusing a total the supply cannot hold.
///
/// `Zatoshis::checked_add` bounds every step, so the running total is a valid
/// amount at each addition rather than only at the end.
fn total(mut values: impl Iterator<Item = Zatoshis>) -> Option<Zatoshis> {
    values.try_fold(Zatoshis::ZERO, Zatoshis::checked_add)
}

/// An amount as a signed integer.
///
/// Infallible in practice — every [`Zatoshis`] is bounded by the supply, which
/// is far below `i64::MAX` — but written as a checked conversion rather than an
/// `as` cast so the bound is enforced rather than assumed. `as` would wrap
/// silently if that ever stopped being true.
fn signed(amount: Zatoshis) -> i64 {
    i64::try_from(u64::from(amount)).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zaino_primitives::types::ScriptType;

    /// The largest amount the protocol allows, so two of them overflow a sum.
    const MAX: u64 = 21_000_000 * 100_000_000;

    fn amount(value: u64) -> StoredTxOut {
        StoredTxOut::new(
            Zatoshis::new(value).expect("a valid amount"),
            StoredAddress {
                hash: [0u8; 20],
                script_type: ScriptType::P2PKH,
            },
        )
    }

    fn position() -> BlockTxPosition {
        BlockTxPosition {
            height: Height::try_from(1u32).expect("a valid height"),
            tx_index: 0,
        }
    }

    fn outpoint() -> Outpoint {
        Outpoint {
            txid: TransactionId::from([0u8; 32]),
            index: 0,
        }
    }

    fn effects(outputs: &[u64], spends: &[u64]) -> StoreAddressEffects {
        StoreAddressEffects {
            outputs: outputs
                .iter()
                .map(|value| LocatedOutput {
                    outpoint: outpoint(),
                    output: amount(*value),
                    position: position(),
                    txid: TransactionId::from([0u8; 32]),
                })
                .collect(),
            spends: spends
                .iter()
                .map(|value| LocatedSpend {
                    outpoint: outpoint(),
                    output: amount(*value),
                    position: position(),
                    txid: TransactionId::from([0u8; 32]),
                })
                .collect(),
        }
    }

    /// Receiving more than was spent is a positive delta.
    #[test]
    fn more_received_than_spent_is_a_receive() {
        let net = effects(&[100, 50], &[30])
            .net_value()
            .expect("well within the supply");

        assert_eq!(i64::from(net), 120);
        assert!(net.is_receive());
    }

    /// Spending more than was received is a negative delta.
    ///
    /// Legitimate on one half alone: the store sees spends of outputs created
    /// outside the queried range, so its contribution can be net negative.
    #[test]
    fn more_spent_than_received_is_a_spend() {
        let net = effects(&[10], &[70])
            .net_value()
            .expect("well within the supply");

        assert_eq!(i64::from(net), -60);
        assert!(net.is_spend());
    }

    /// Nothing observed is a zero delta, not an absent one.
    #[test]
    fn no_effects_net_to_zero() {
        assert_eq!(
            StoreAddressEffects::default().net_value(),
            Some(SignedZatoshis::new(0))
        );
    }

    /// A total past the money supply is refused rather than wrapped.
    ///
    /// No single amount can do this — `Zatoshis` bounds each one — but a
    /// corrupt or adversarial effect set can hold enough of them that the sum
    /// does. The previous `sum::<i64>()` would have reported a plausible
    /// figure instead of refusing.
    #[test]
    fn a_total_past_the_supply_is_refused() {
        assert_eq!(effects(&[MAX, MAX], &[]).net_value(), None);
        assert_eq!(effects(&[], &[MAX, MAX]).net_value(), None);
    }

    /// A total at exactly the supply is still an answer.
    ///
    /// The bound is inclusive, so the largest representable set is not
    /// mistaken for corruption.
    #[test]
    fn a_total_at_the_supply_is_allowed() {
        let net = effects(&[MAX], &[])
            .net_value()
            .expect("exactly the maximum");

        assert_eq!(
            i64::from(net),
            i64::try_from(MAX).expect("the supply fits in an i64")
        );
    }
}
