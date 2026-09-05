//! Arithmetic over the zatoshi quantity family.
//!
//! Cross-type operations are relations between quantities, not methods of a
//! single one, so they live here beside the types rather than on any of them.
//! This module is also where the allowed operations — the algebra — are written
//! down as the specification a new summation site inherits.
//!
//! # The algebra
//!
//! Write `A` for a held amount, `F` for a flow sum, `D` for a signed value,
//! and `S` for the money supply. The quantities occupy nested ranges:
//!
//! ```text
//! A ∈ [0, S]          an amount held
//! F ∈ [0, ∞)          a sum of movements  (machine-bounded, not supply-bounded)
//! D ∈ [−S, S]         a signed value (a movement or a difference)
//! ```
//!
//! Three relations are defined, and only these three:
//!
//! ```text
//! accumulate          : [A] → F   Σ aᵢ, counting each movement
//! accumulate_balances : [A] → A?  Σ aᵢ, of balances that coexist
//!                                 (supply-capped; closed — lands back in A)
//! net                 : F × F → D received − spent, landing in [−S, S] or refused
//! ```
//!
//! `accumulate` carries amounts into the unbounded flow sum: a total of
//! movements is not a balance, so the supply cap does not apply to it, and it
//! fails only if the machine integer overflows. `accumulate_balances` is the
//! second accumulate — the same fold shape with a different landing: balances
//! that coexist at one moment cannot total more than the coins that exist, so
//! the sum is itself in `[0, S]` and `A` is closed under it; the fold lands
//! back in [`Zatoshis`] rather than a new type, refusing a total past the
//! supply. `net` subtracts a spent flow from a received one and admits the
//! result only as a signed value: a balance change lives in `[−S, S]`, so a
//! result outside it is refused. That bound is a property of a balance change,
//! which the two sums are only when they are the received and spent flow of
//! one balance — `net`'s contract. A difference of unrelated flow sums is not
//! a balance change and is deliberately not offered. See ADR-0013.

use super::{SignedZatoshis, Zatoshis, ZatoshisFlowSum};

impl Zatoshis {
    /// Sum a set of coexisting balances.
    ///
    /// The `accumulate_balances` relation: `[A] → A?`. Folds the balances with
    /// the supply-capped [`checked_add`](Self::checked_add), returning `None`
    /// if the running total passes the supply; an empty sequence sums to
    /// [`ZERO`](Self::ZERO).
    ///
    /// Contract: the operands are balances that coexist at one moment. Coins
    /// that coexist cannot total more than the coins that exist, so under that
    /// contract a total past the supply is not a large number — it is evidence
    /// the inputs overlap or double-count (corruption), hence `None`, failing
    /// loud rather than wrapping.
    ///
    /// This is the second accumulate of the algebra: the same fold shape as
    /// [`ZatoshisFlowSum::try_accumulate`], with a different landing and
    /// bound. Because a sum of coexisting balances stays within `[0, supply]`,
    /// [`Zatoshis`] is closed under it — so this is deliberately an operation
    /// returning [`Zatoshis`], not a new quantity type.
    pub fn sum_balances(mut values: impl Iterator<Item = Zatoshis>) -> Option<Zatoshis> {
        values.try_fold(Zatoshis::ZERO, Zatoshis::checked_add)
    }
}

impl ZatoshisFlowSum {
    /// Sum a sequence of amounts as flow.
    ///
    /// The `accumulate` relation: `[A] → F`. Folds the amounts into the
    /// unbounded flow sum with a checked add, returning `None` only if the
    /// running total overflows the machine integer.
    ///
    /// That overflow is unreachable in practice — each amount is a
    /// supply-bounded [`Zatoshis`] and the count is a collection length, so the
    /// total cannot approach a `u128` — but the add stays checked so a future
    /// change fails loud rather than wrapping silently.
    pub fn try_accumulate(mut values: impl Iterator<Item = Zatoshis>) -> Option<Self> {
        values.try_fold(Self::ZERO, ZatoshisFlowSum::checked_add)
    }

    /// Add one amount to a flow sum, or `None` on machine overflow.
    ///
    /// The incremental step of [`try_accumulate`](Self::try_accumulate).
    fn checked_add(self, amount: Zatoshis) -> Option<Self> {
        self.into_raw()
            .checked_add(u128::from(u64::from(amount)))
            .map(Self::from_raw)
    }

    /// The net balance change of a received flow minus a spent flow.
    ///
    /// The `net` relation: `F × F → D`. Computes `self - spent` and admits the
    /// result only as a [`SignedZatoshis`] (±supply), returning `None` otherwise.
    ///
    /// Contract: `self` is the received flow and `spent` the spent flow of the
    /// *same* balance over the *same* range. Only then is the difference that
    /// balance's change, which an aggregate balance — living in `[0, supply]` —
    /// keeps within `[-supply, supply]`. So `None` means the two flows do not
    /// describe a coherent balance (partial or corrupt data), not merely a large
    /// number. A difference of unrelated flow sums is not a balance change, is
    /// bounded only by the machine, and is deliberately not offered: there is no
    /// generic subtraction nor `impl Sub` that would return an unbounded result.
    pub fn net(self, spent: Self) -> Option<SignedZatoshis> {
        let (received, spent) = (self.into_raw(), spent.into_raw());
        let magnitude = i128::try_from(received.abs_diff(spent)).ok()?;
        let difference = if received >= spent {
            magnitude
        } else {
            -magnitude
        };
        SignedZatoshis::try_from_i128(difference).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::super::MAX_ZATOSHIS;
    use super::*;

    fn zatoshis(value: u64) -> Zatoshis {
        Zatoshis::new(value).expect("a valid amount")
    }

    /// `accumulate` of nothing is a flow sum of zero, which differences to a
    /// zero signed value rather than being absent.
    #[test]
    fn accumulate_of_nothing_is_zero() {
        let empty = ZatoshisFlowSum::try_accumulate(core::iter::empty())
            .expect("an empty sum does not overflow");

        assert_eq!(empty, ZatoshisFlowSum::ZERO);
        assert_eq!(empty.net(ZatoshisFlowSum::ZERO).map(i64::from), Some(0));
    }

    /// `accumulate` sums its amounts.
    #[test]
    fn accumulate_sums_the_amounts() {
        let received = ZatoshisFlowSum::try_accumulate([100, 50, 30].map(zatoshis).into_iter())
            .expect("well within the machine bound");
        let spent = ZatoshisFlowSum::try_accumulate([60].map(zatoshis).into_iter())
            .expect("well within the machine bound");

        assert_eq!(received.net(spent).map(i64::from), Some(120));
    }

    /// Differencing a smaller sum from a larger is a receive; the reverse a
    /// spend.
    #[test]
    fn difference_is_signed_by_direction() {
        let more = ZatoshisFlowSum::try_accumulate([70].map(zatoshis).into_iter()).expect("valid");
        let less = ZatoshisFlowSum::try_accumulate([10].map(zatoshis).into_iter()).expect("valid");

        let receive = more.net(less).expect("within the supply");
        assert!(receive.is_receive());
        assert_eq!(i64::from(receive), 60);

        let spend = less.net(more).expect("within the supply");
        assert!(spend.is_spend());
        assert_eq!(i64::from(spend), -60);
    }

    /// A flow sum is not bounded by the supply: several supply-sized amounts
    /// accumulate past it, and the total is a legitimate flow sum.
    #[test]
    fn flow_sum_exceeds_the_supply() {
        let gross =
            ZatoshisFlowSum::try_accumulate([MAX_ZATOSHIS, MAX_ZATOSHIS].map(zatoshis).into_iter())
                .expect("gross flow is only machine-bounded");
        let one = ZatoshisFlowSum::try_accumulate([MAX_ZATOSHIS].map(zatoshis).into_iter())
            .expect("valid");

        // Twice the supply less once the supply nets to exactly the supply.
        assert_eq!(
            gross.net(one).map(i64::from),
            Some(i64::try_from(MAX_ZATOSHIS).expect("the supply fits in an i64"))
        );
    }

    /// A difference whose magnitude exceeds the supply is not a representable
    /// signed value, so it is refused rather than truncated.
    #[test]
    fn difference_past_the_supply_is_refused() {
        let two_supplies =
            ZatoshisFlowSum::try_accumulate([MAX_ZATOSHIS, MAX_ZATOSHIS].map(zatoshis).into_iter())
                .expect("valid flow sum");

        assert_eq!(two_supplies.net(ZatoshisFlowSum::ZERO), None);
        assert_eq!(ZatoshisFlowSum::ZERO.net(two_supplies), None);
    }

    /// `accumulate_balances` of nothing is zero held.
    #[test]
    fn sum_balances_of_nothing_is_zero() {
        assert_eq!(
            Zatoshis::sum_balances(core::iter::empty()),
            Some(Zatoshis::ZERO)
        );
    }

    /// `accumulate_balances` sums balances within the supply.
    #[test]
    fn sum_balances_within_the_supply_sums() {
        let total = Zatoshis::sum_balances([100, 50, 30].map(zatoshis).into_iter());

        assert_eq!(total.map(u64::from), Some(180));
    }

    /// A set of balances totalling exactly the supply is the extreme legitimate
    /// case — all coins in the summed set — and is admitted.
    #[test]
    fn sum_balances_at_the_supply_is_allowed() {
        let half = MAX_ZATOSHIS / 2;
        let total = Zatoshis::sum_balances([half, MAX_ZATOSHIS - half].map(zatoshis).into_iter());

        assert_eq!(total.map(u64::from), Some(MAX_ZATOSHIS));
    }

    /// Coexisting balances cannot total past the supply, so such a total is
    /// evidence of overlapping or double-counted inputs and is refused.
    #[test]
    fn sum_balances_past_the_supply_is_refused() {
        assert_eq!(
            Zatoshis::sum_balances([MAX_ZATOSHIS, 1].map(zatoshis).into_iter()),
            None
        );
    }

    /// A difference at exactly the supply is still a representable signed value.
    #[test]
    fn difference_at_the_supply_is_allowed() {
        let one_supply = ZatoshisFlowSum::try_accumulate([MAX_ZATOSHIS].map(zatoshis).into_iter())
            .expect("valid flow sum");

        assert_eq!(
            one_supply.net(ZatoshisFlowSum::ZERO).map(i64::from),
            Some(i64::try_from(MAX_ZATOSHIS).expect("the supply fits in an i64"))
        );
    }
}
