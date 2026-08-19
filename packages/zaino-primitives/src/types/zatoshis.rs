//! Zcash monetary amounts in zatoshis.

use core::fmt;

/// An unsigned zatoshi amount (balance, UTXO value).
///
/// 1 ZEC = 100_000_000 zatoshis. Maximum supply is 21M ZEC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Zatoshis(u64);

/// Maximum possible zatoshi value (21M ZEC).
const MAX_ZATOSHIS: u64 = 21_000_000 * 100_000_000;

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
}

impl From<Zatoshis> for u64 {
    fn from(z: Zatoshis) -> Self {
        z.0
    }
}

impl fmt::Display for Zatoshis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A signed zatoshi delta (balance change: positive = receive, negative = spend).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SignedZatoshis(i64);

impl SignedZatoshis {
    /// Create a signed zatoshi amount.
    pub fn new(amount: i64) -> Self {
        Self(amount)
    }

    /// Whether this is a spend (negative).
    pub fn is_spend(self) -> bool {
        self.0 < 0
    }

    /// Whether this is a receive (positive).
    pub fn is_receive(self) -> bool {
        self.0 > 0
    }
}

impl From<SignedZatoshis> for i64 {
    fn from(z: SignedZatoshis) -> Self {
        z.0
    }
}

impl fmt::Display for SignedZatoshis {
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

    #[test]
    fn signed_spend_receive() {
        assert!(SignedZatoshis::new(-100).is_spend());
        assert!(SignedZatoshis::new(100).is_receive());
        assert!(!SignedZatoshis::new(0).is_spend());
        assert!(!SignedZatoshis::new(0).is_receive());
    }
}
