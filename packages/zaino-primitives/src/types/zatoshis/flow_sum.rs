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
/// The only way to obtain one is
/// [`try_accumulate`](ZatoshisFlowSum::try_accumulate): the inner value is
/// private, so a flow sum can only be the sum of some amounts, never an
/// arbitrary integer. Differencing two flow sums lands the result in a
/// [`ZatoshisDelta`](super::ZatoshisDelta) via
/// [`delta`](ZatoshisFlowSum::delta). Both operations, and the algebra that
/// relates the quantities, live in the `arithmetic` module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ZatoshisFlowSum(u128);

impl ZatoshisFlowSum {
    /// A flow sum of nothing.
    pub(super) const ZERO: Self = Self(0);

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
