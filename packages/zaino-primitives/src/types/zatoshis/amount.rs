//! An amount of ZEC counted in zatoshis, read as a balance or as a movement.

use core::fmt;

/// Maximum possible zatoshi value (21M ZEC).
pub(super) const MAX_ZATOSHIS: u64 = 21_000_000 * 100_000_000;

/// An unsigned zatoshi amount (balance, UTXO value).
///
/// 1 ZEC = 100_000_000 zatoshis. Maximum supply is 21M ZEC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Zatoshis(u64);

/// Error when a zatoshi amount exceeds the protocol maximum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("zatoshi amount {got} exceeds maximum {MAX_ZATOSHIS}")]
pub struct ZatoshisOverflow {
    /// The value that was rejected.
    pub got: u64,
}

impl Zatoshis {
    /// Zero zatoshis.
    pub const ZERO: Self = Self(0);

    /// The largest amount the protocol allows, the whole money supply.
    pub const MAX: Self = Self(MAX_ZATOSHIS);

    /// Create a zatoshi amount, validating against the protocol maximum.
    pub fn new(amount: u64) -> Result<Self, ZatoshisOverflow> {
        if amount > MAX_ZATOSHIS {
            Err(ZatoshisOverflow { got: amount })
        } else {
            Ok(Self(amount))
        }
    }

    /// Returns `Some(sum)` when the addition is representable (below
    /// MAX_ZATOSHIS), or `None` on arithmetic overflow, matching Rust
    /// primitive integer `checked_add` semantics.
    pub fn checked_add(self, other: Self) -> Option<Self> {
        let sum = self.0.checked_add(other.0)?;
        if sum > MAX_ZATOSHIS {
            return None;
        }
        Some(Self(sum))
    }

    /// Sums balances that coexist at one moment, or `None` if the total passes the supply.
    pub fn sum_balances(mut values: impl Iterator<Item = Zatoshis>) -> Option<Zatoshis> {
        values.try_fold(Zatoshis::ZERO, Zatoshis::checked_add)
    }
}

impl Zatoshis {
    /// Reads the amount as a plain integer, usable in constant context.
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

impl From<Zatoshis> for u64 {
    fn from(z: Zatoshis) -> Self {
        z.as_u64()
    }
}

impl fmt::Display for Zatoshis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero() {
        assert_eq!(u64::from(Zatoshis::ZERO), 0);
    }

    #[test]
    fn max_is_valid() {
        assert!(Zatoshis::new(MAX_ZATOSHIS).is_ok());
    }

    #[test]
    fn above_max_rejected() {
        assert!(Zatoshis::new(MAX_ZATOSHIS + 1).is_err());
    }

    #[test]
    fn checked_add_overflow() {
        let a = Zatoshis::new(MAX_ZATOSHIS).expect("valid");
        assert!(a.checked_add(Zatoshis::new(1).expect("valid")).is_none());
    }

    fn zatoshis(value: u64) -> Zatoshis {
        Zatoshis::new(value).expect("a valid amount")
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
}
