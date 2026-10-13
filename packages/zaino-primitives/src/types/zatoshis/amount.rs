//! Unsigned zatoshi amount: a balance or a UTXO value

use core::fmt;

/// 21M ZEC
pub(super) const MAX_ZATOSHIS: u64 = 21_000_000 * 100_000_000;

/// `0..=MAX_ZATOSHIS` (1 ZEC = 100_000_000 zatoshis)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Zatoshis(u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("zatoshi amount {got} exceeds maximum {MAX_ZATOSHIS}")]
pub struct ZatoshisOverflow {
    pub(crate) got: u64,
}

impl Zatoshis {
    pub const ZERO: Self = Self(0);

    /// Whole money supply
    pub const MAX: Self = Self(MAX_ZATOSHIS);

    pub fn new(amount: u64) -> Result<Self, ZatoshisOverflow> {
        if amount > MAX_ZATOSHIS {
            Err(ZatoshisOverflow { got: amount })
        } else {
            Ok(Self(amount))
        }
    }

    /// `None` past `MAX_ZATOSHIS`
    pub fn checked_add(self, other: Self) -> Option<Self> {
        let sum = self.0.checked_add(other.0)?;
        if sum > MAX_ZATOSHIS {
            return None;
        }
        Some(Self(sum))
    }

    /// Balances coexisting at one moment; `None` past the supply
    pub fn sum_balances(mut values: impl Iterator<Item = Zatoshis>) -> Option<Zatoshis> {
        values.try_fold(Zatoshis::ZERO, Zatoshis::checked_add)
    }

    pub const fn as_u64(self) -> u64 {
        self.0
    }

    /// Total: the supply (2.1e15) is far below `i64::MAX`
    pub const fn as_i64(self) -> i64 {
        self.0 as i64
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

    /// A total built three ways (`new`, two halves added, two halves summed): exactly the supply
    /// = every coin in one set, one past it = double-counted inputs
    #[test]
    fn amounts_reach_the_supply_and_never_pass_it() {
        for (total, expected) in [
            (0, Some(Zatoshis(0))),
            (MAX_ZATOSHIS, Some(Zatoshis(MAX_ZATOSHIS))),
            (MAX_ZATOSHIS + 1, None),
        ] {
            let half = |n: u64| Zatoshis::new(n).expect("half the total within the supply");
            let (low, high) = (half(total / 2), half(total - total / 2));
            let built = (
                Zatoshis::new(total).ok(),
                low.checked_add(high),
                Zatoshis::sum_balances([low, high].into_iter()),
            );
            assert_eq!(built, (expected, expected, expected), "{total}");
        }
    }
}
