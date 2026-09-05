//! The flow-sum quantity: an accumulation of zatoshi movements.

/// An accumulation of zatoshi movements.
///
/// A running total of amounts that *move* — every output paying an address,
/// every input spending its prior outputs — as distinct from a set of balances
/// that coexist. The same coins can move through an address many times, so this
/// total counts them each time and is **not** bounded by the money supply. It is
/// bounded only by machine representability, which is why it is a `u128` and not
/// a [`Zatoshis`](super::Zatoshis).
///
/// Two validated provenances lead in, and no unchecked one: a total *derived*
/// in the domain arrives through
/// [`try_accumulate`](ZatoshisFlowSum::try_accumulate), a checked fold of
/// amounts, and a total a source *delivers already summed* arrives through
/// [`from_summed`](ZatoshisFlowSum::from_summed), the boundary door. The inner
/// value is private, so a flow sum is always the sum of some movements, never
/// an arbitrary integer. Differencing two flow sums lands the result in a
/// [`SignedZatoshis`](super::SignedZatoshis) via
/// [`net`](ZatoshisFlowSum::net). The fold, the difference, and the algebra
/// that relates the quantities live in the `arithmetic` module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ZatoshisFlowSum(u128);

impl ZatoshisFlowSum {
    /// A flow sum of nothing.
    pub(super) const ZERO: Self = Self(0);

    /// Adopt a flow total delivered already summed by a source.
    ///
    /// This is the boundary door: a backend reports a lifetime flow total —
    /// such as an address's gross receipts — as a single `u64`, summed on its
    /// side. The flow sum's only invariant is machine representability, and a
    /// `u64` always fits the `u128` accumulator, so there is genuinely nothing
    /// to check and the door is honestly infallible. A total *derived* in the
    /// domain reaches the type through
    /// [`try_accumulate`](Self::try_accumulate) instead.
    pub fn from_summed(total: u64) -> Self {
        Self(u128::from(total))
    }

    /// Wrap a raw accumulator value.
    ///
    /// Module-internal: the arithmetic relations build a flow sum from the
    /// wide integer they fold into, and no external door exists.
    pub(super) const fn from_raw(raw: u128) -> Self {
        Self(raw)
    }

    /// The raw accumulated value, for the arithmetic relations to difference.
    pub(super) const fn into_raw(self) -> u128 {
        self.0
    }
}

impl From<ZatoshisFlowSum> for u128 {
    fn from(sum: ZatoshisFlowSum) -> Self {
        sum.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The difference guard fails loud, not silent, when a flow sum is too large
    /// to be a signed integer at all.
    ///
    /// This is the machine-representability check beneath the supply bound: a
    /// difference is refused for being unrepresentable before it is ever asked
    /// whether it fits the supply. It is unreachable with real amounts — a flow
    /// sum cannot approach `u128::MAX` — so it is constructed here through the
    /// module-internal raw door purely to prove the guard refuses rather than
    /// wraps to a plausible figure.
    #[test]
    fn a_difference_too_large_for_a_signed_integer_is_refused() {
        let unrepresentable = ZatoshisFlowSum::from_raw(u128::MAX);

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
