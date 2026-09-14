use super::{SignedZatoshis, Zatoshis, ZatoshisFlowSum};

impl Zatoshis {
    /// Sums balances that coexist at one moment, or `None` if the total passes the supply.
    pub fn sum_balances(mut values: impl Iterator<Item = Zatoshis>) -> Option<Zatoshis> {
        values.try_fold(Zatoshis::ZERO, Zatoshis::checked_add)
    }
}

impl ZatoshisFlowSum {
    /// Sums amounts as a flow, or `None` if the total would exceed `u128::MAX`.
    pub fn try_accumulate(mut values: impl Iterator<Item = Zatoshis>) -> Option<Self> {
        values.try_fold(Self::ZERO, ZatoshisFlowSum::checked_add)
    }

    fn checked_add(self, amount: Zatoshis) -> Option<Self> {
        self.into_raw()
            .checked_add(u128::from(u64::from(amount)))
            .map(Self::from_raw)
    }

    /// Returns the received flow minus the spent one as a signed value, or `None` outside the supply.
    pub fn net(self, spent: Self) -> Option<SignedZatoshis> {
        let (received, spent) = (self.into_raw(), spent.into_raw());
        let magnitude = i64::try_from(received.abs_diff(spent)).ok()?;
        let difference = if received >= spent {
            magnitude
        } else {
            -magnitude
        };
        SignedZatoshis::try_new(difference).ok()
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

    /// `accumulate_balances` of nothing is a zero balance.
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
