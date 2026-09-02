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

/// Error when a signed zatoshi delta's magnitude exceeds the money supply.
///
/// A delta whose absolute value is larger than [`MAX_ZATOSHIS`] cannot be the
/// change in an aggregate transparent balance, which is what a checked delta
/// claims to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("signed zatoshi delta {got} exceeds supply magnitude {MAX_ZATOSHIS}")]
pub struct SignedZatoshisOverflow {
    /// The value that was rejected.
    pub got: i128,
}

impl SignedZatoshis {
    /// Create a signed zatoshi amount from an already-in-range `i64`.
    ///
    /// Does not bound its input: a raw delta can carry any `i64` the caller
    /// already holds (a pool value balance, a wire-supplied figure). Callers
    /// that derive a delta and need the money-supply invariant enforced use
    /// [`SignedZatoshis::try_from_i128`] instead.
    pub fn new(amount: i64) -> Self {
        Self(amount)
    }

    /// Create a signed zatoshi delta from a wide integer, enforcing that its
    /// magnitude fits the money supply.
    ///
    /// A delta is the change in an aggregate transparent balance over some
    /// range. An aggregate balance lives in `[0, MAX_ZATOSHIS]`, so its change
    /// lives in `[-MAX_ZATOSHIS, MAX_ZATOSHIS]`. A value outside that range is
    /// not a representable delta, so it is rejected rather than truncated: a
    /// derived figure that lands there signals corrupt input, not a large but
    /// legitimate balance change.
    ///
    /// Takes an `i128` because the caller accumulates gross flow in a wide
    /// integer before differencing it; the returned type then re-establishes
    /// the narrower invariant.
    pub fn try_from_i128(value: i128) -> Result<Self, SignedZatoshisOverflow> {
        i64::try_from(value)
            .ok()
            .filter(|inner| inner.unsigned_abs() <= MAX_ZATOSHIS)
            .map(Self)
            .ok_or(SignedZatoshisOverflow { got: value })
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

    #[test]
    fn try_from_i128_accepts_supply_magnitude() {
        let max = i128::from(MAX_ZATOSHIS);
        assert_eq!(
            SignedZatoshis::try_from_i128(max).map(i64::from),
            Ok(i64::try_from(MAX_ZATOSHIS).expect("supply fits in i64"))
        );
        assert_eq!(
            SignedZatoshis::try_from_i128(-max).map(i64::from),
            Ok(-i64::try_from(MAX_ZATOSHIS).expect("supply fits in i64"))
        );
    }

    #[test]
    fn try_from_i128_rejects_magnitude_past_supply() {
        let over = i128::from(MAX_ZATOSHIS) + 1;
        assert_eq!(
            SignedZatoshis::try_from_i128(over),
            Err(SignedZatoshisOverflow { got: over })
        );
        assert_eq!(
            SignedZatoshis::try_from_i128(-over),
            Err(SignedZatoshisOverflow { got: -over })
        );
    }

    #[test]
    fn try_from_i128_rejects_beyond_i64() {
        let huge = i128::from(i64::MAX) + 1;
        assert_eq!(
            SignedZatoshis::try_from_i128(huge),
            Err(SignedZatoshisOverflow { got: huge })
        );
    }
}
