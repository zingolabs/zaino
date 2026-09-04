//! Transparent address history, as the finalised range can report it.
//!
//! These are the port and effect types; `zaino-chain-store-zainodb` implements
//! [`TransparentHistoryIndex`](crate::TransparentHistoryIndex) against them.
//! The module is shaped to mirror `zaino_chain_head::transparent`, so a
//! consumer merging the two halves meets one shape rather than two.
//!
//! The store contributes the *effects* it can see below the finalised
//! watermark. It never constructs a complete history: an address's full
//! record is the store's contribution merged with the recent window's, and
//! neither half alone is an answer.

use zaino_primitives::types::{
    BlockTxPosition, Height, Outpoint, TransactionId, Zatoshis, ZatoshisDelta,
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
    /// The net value change these effects describe, or `None` if it is not a
    /// representable delta.
    ///
    /// A convenience for a consumer that has already merged both halves and
    /// wants a figure. Meaningless on one half alone, which is why it is a
    /// method here rather than a field: a field would invite reading it off an
    /// unmerged contribution.
    ///
    /// # What is bounded and what is not
    ///
    /// The gross sides — total received and total spent — are *flow*, not a
    /// balance: every output paying the address and every input spending its
    /// prior outputs across the range is counted, and the same coins can move
    /// through the address many times. Gross flow is therefore not bounded by
    /// the money supply, so the sides are accumulated in a wide integer with
    /// [`checked_add`](i128::checked_add), which fails loud only on machine
    /// overflow. Each element is a supply-bounded [`Zatoshis`] and the count is
    /// a `Vec` length, so that failure is unreachable in practice; it stays
    /// checked so a future change fails loud rather than wrapping silently.
    ///
    /// The *net* — received minus spent — is the change in the addresses'
    /// aggregate balance over the range. An aggregate balance lives in
    /// `[0, supply]`, so its change lives in `[-supply, +supply]`. That is the
    /// real invariant, and [`ZatoshisDelta::try_from_i128`] enforces it: a net
    /// whose magnitude exceeds the supply is not a representable delta and
    /// yields `None` rather than a truncated figure.
    ///
    /// A [`ZatoshisDelta`] rather than a bare `i64`, because this is the
    /// domain's signed-delta quantity and a raw integer invites the arithmetic
    /// that produced it to be redone somewhere else with different rules.
    pub fn net_value(&self) -> Option<ZatoshisDelta> {
        let received = total(self.outputs.iter().map(|output| output.output.value))?;
        let spent = total(self.spends.iter().map(|spend| spend.output.value))?;

        ZatoshisDelta::try_from_i128(received - spent).ok()
    }

    /// Whether the store observed nothing at all.
    pub fn is_empty(&self) -> bool {
        self.outputs.is_empty() && self.spends.is_empty()
    }
}

/// Sums gross flow into a wide integer, failing loud only on machine overflow.
///
/// Accumulates in `i128` via [`checked_add`](i128::checked_add): gross flow is
/// not bounded by the money supply, so the supply cap does not belong on the
/// running total. Each element is a supply-bounded [`Zatoshis`] and the count
/// is a `Vec` length, so the `None` branch is unreachable in practice; it stays
/// checked so a future change fails loud rather than wrapping silently.
fn total(mut values: impl Iterator<Item = Zatoshis>) -> Option<i128> {
    values.try_fold(0i128, |sum, value| {
        sum.checked_add(i128::from(u64::from(value)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use zaino_primitives::types::ScriptType;

    /// The largest amount the protocol allows, so two of them on one side
    /// exceed the supply magnitude a net delta may hold.
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
            Some(ZatoshisDelta::try_new(0).expect("zero is in range"))
        );
    }

    /// Gross flow past the money supply is a legitimate answer, not corruption.
    ///
    /// `outputs`/`spends` are cumulative movements, so the same coins can pass
    /// through an address enough times to sum past the supply on either side.
    /// Here each gross side is twice the supply, yet they net to a
    /// within-supply delta, which is reported rather than refused.
    #[test]
    fn gross_flow_past_the_supply_is_allowed() {
        let net = effects(&[MAX, MAX], &[MAX])
            .net_value()
            .expect("gross flow is not supply-bounded");

        assert_eq!(
            i64::from(net),
            i64::try_from(MAX).expect("the supply fits in an i64")
        );
    }

    /// A net whose magnitude exceeds the supply is refused rather than wrapped.
    ///
    /// The net is a change in aggregate balance, which lives in
    /// `[-supply, +supply]`. A received/spent pairing that lands outside it is
    /// not a representable delta, so it fails loud instead of saturating to a
    /// plausible figure.
    #[test]
    fn a_net_past_the_supply_is_refused() {
        assert_eq!(effects(&[MAX, MAX], &[]).net_value(), None);
        assert_eq!(effects(&[], &[MAX, MAX]).net_value(), None);
    }

    /// A net at exactly the supply is still an answer.
    ///
    /// The bound is inclusive, so the largest representable delta is not
    /// mistaken for corruption.
    #[test]
    fn a_net_at_the_supply_is_allowed() {
        let net = effects(&[MAX], &[])
            .net_value()
            .expect("exactly the maximum");

        assert_eq!(
            i64::from(net),
            i64::try_from(MAX).expect("the supply fits in an i64")
        );
    }
}
