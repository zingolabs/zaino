//! The flow-sum quantity: an accumulation of zatoshi movements.

use super::{SignedZatoshis, Zatoshis};

/// An accumulation of zatoshi movements.
///
/// A running total of amounts that *move* — every output paying an address,
/// every input spending its prior outputs — as distinct from a set of balances
/// that coexist. The same coins can move through an address many times, so this
/// total counts them each time and is **not** bounded by the money supply. It is
/// bounded only by `u128::MAX`, the ceiling of the `u128` that backs it, which
/// is why it is not a [`Zatoshis`](super::Zatoshis).
///
/// Two validated provenances lead in, and no unchecked one: a total *derived*
/// in the domain arrives through
/// [`try_accumulate`](ZatoshisFlowSum::try_accumulate), a checked fold of
/// amounts, and a total a source *delivers already summed* arrives through
/// [`from_summed`](ZatoshisFlowSum::from_summed), the boundary door. The inner
/// value is private, so a flow sum is always the sum of some movements, never
/// an arbitrary integer. Differencing two flow sums lands the result in a
/// [`SignedZatoshis`](super::SignedZatoshis) via
/// [`net`](ZatoshisFlowSum::net).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ZatoshisFlowSum(u128);

impl ZatoshisFlowSum {
    const ZERO: Self = Self(0);

    /// Adopt a flow total delivered already summed by a source.
    ///
    /// This is the boundary door: a backend reports a lifetime flow total —
    /// such as an address's gross receipts — as a single `u64`, summed on its
    /// side. The flow sum's only bound is `u128::MAX`, and a `u64` always fits
    /// the `u128` accumulator, so there is genuinely nothing
    /// to check and the door is honestly infallible. A total *derived* in the
    /// domain reaches the type through
    /// [`try_accumulate`](Self::try_accumulate) instead.
    pub fn from_summed(total: u64) -> Self {
        Self(u128::from(total))
    }

    /// Sums amounts as a flow, or `None` if the total would exceed `u128::MAX`.
    pub fn try_accumulate(mut values: impl Iterator<Item = Zatoshis>) -> Option<Self> {
        values.try_fold(Self::ZERO, Self::checked_add)
    }

    fn checked_add(self, amount: Zatoshis) -> Option<Self> {
        self.0.checked_add(u128::from(amount.as_u64())).map(Self)
    }

    /// Returns the received flow minus the spent one as a signed value, or `None` outside the supply.
    pub fn net(self, spent: Self) -> Option<SignedZatoshis> {
        let magnitude = i64::try_from(self.0.abs_diff(spent.0)).ok()?;
        let difference = if self.0 >= spent.0 {
            magnitude
        } else {
            -magnitude
        };
        SignedZatoshis::try_new(difference).ok()
    }
}

impl From<ZatoshisFlowSum> for u128 {
    fn from(sum: ZatoshisFlowSum) -> Self {
        sum.0
    }
}

#[cfg(any(test, doctest))]
mod tests {
    use super::super::MAX_ZATOSHIS;
    use super::*;

    /// No operator joins the quantity types: a flow sum plus a balance total is
    /// rejected by the compiler.
    ///
    /// ```compile_fail,E0369
    /// use zaino_primitives::types::{Zatoshis, ZatoshisFlowSum};
    ///
    /// let flow = ZatoshisFlowSum::try_accumulate(core::iter::empty()).expect("empty");
    /// let balance = Zatoshis::sum_balances(core::iter::empty()).expect("empty");
    /// let _ = flow + balance;
    /// ```
    #[allow(dead_code)]
    struct FlowSumPlusBalanceDoesNotAdd;

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
            .expect("well within u128::MAX");
        let spent = ZatoshisFlowSum::try_accumulate([60].map(zatoshis).into_iter())
            .expect("well within u128::MAX");

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
                .expect("gross flow is bounded only by u128::MAX");
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

    /// The difference guard fails loud, not silent, when a flow sum is too large
    /// to fit an `i64` at all.
    ///
    /// Unreachable with real amounts — a flow sum cannot approach `u128::MAX` —
    /// so the value is built on the private field, which only this module can
    /// reach, purely to prove the guard refuses rather than wraps.
    #[test]
    fn a_difference_too_large_for_a_signed_integer_is_refused() {
        let unrepresentable = ZatoshisFlowSum(u128::MAX);

        assert_eq!(unrepresentable.net(ZatoshisFlowSum::ZERO), None);
        assert_eq!(ZatoshisFlowSum::ZERO.net(unrepresentable), None);
    }

    /// The boundary door round-trips: a pre-summed total goes in as a `u64`
    /// and comes back out unchanged through the `u128` reader.
    #[test]
    fn from_summed_round_trips() {
        let total = 123_456_789_u64;

        assert_eq!(
            u128::from(ZatoshisFlowSum::from_summed(total)),
            u128::from(total)
        );
    }

    /// The boundary door admits any `u64`, including totals past the money
    /// supply — a flow counts the same coins each time they move, so a
    /// lifetime total past the supply is legitimate data, not corruption.
    #[test]
    fn from_summed_admits_totals_past_the_supply() {
        assert_eq!(
            u128::from(ZatoshisFlowSum::from_summed(u64::MAX)),
            u128::from(u64::MAX)
        );
    }
}
