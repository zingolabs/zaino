//! Transparent address balance.

use super::{Zatoshis, ZatoshisFlowSum};

/// Balance information for a set of transparent addresses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressBalance {
    /// Total value currently held, in zatoshis.
    ///
    /// A sum of balances that coexist at one moment, so it is supply-bounded
    /// and lands in [`Zatoshis`].
    pub balance: Zatoshis,
    /// Lifetime gross receipts, in zatoshis.
    ///
    /// A flow total: every output ever paid to the addresses counts, so coins
    /// that cycle through are counted each time they arrive and the total is
    /// **not** supply-bounded. It lands in [`ZatoshisFlowSum`], not
    /// [`Zatoshis`].
    pub received: ZatoshisFlowSum,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `received` is a flow, not a balance: a lifetime total past the money
    /// supply is legitimate data, and the type admits it.
    #[test]
    fn received_past_the_supply_is_representable() {
        let balance = AddressBalance {
            balance: Zatoshis::ZERO,
            received: ZatoshisFlowSum::from_summed(u64::MAX),
        };

        assert_eq!(u128::from(balance.received), u128::from(u64::MAX));
    }
}
