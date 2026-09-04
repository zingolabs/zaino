//! The signed balance-change quantity.

use core::fmt;

use super::MAX_ZATOSHIS;

/// A signed zatoshi delta (balance change: positive = receive, negative = spend).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ZatoshisDelta(i64);

/// Error when a signed zatoshi delta's magnitude exceeds the money supply.
///
/// A delta whose absolute value is larger than [`MAX_ZATOSHIS`] cannot be the
/// change in an aggregate transparent balance, which is what a checked delta
/// claims to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("signed zatoshi delta {got} exceeds supply magnitude {MAX_ZATOSHIS}")]
pub struct ZatoshisDeltaOverflow {
    /// The value that was rejected.
    pub got: i128,
}

impl ZatoshisDelta {
    /// Create a delta from a value read at a boundary, enforcing that its
    /// magnitude fits the money supply.
    ///
    /// A delta is the change in an aggregate balance, which lives in
    /// `[0, MAX_ZATOSHIS]`, so its change lives in
    /// `[-MAX_ZATOSHIS, MAX_ZATOSHIS]`. A value outside that range is not a
    /// representable delta and is rejected rather than truncated: a figure off
    /// the wire or disk that lands there signals corrupt input, not a large but
    /// legitimate balance change.
    ///
    /// This is the boundary door — the external-input validation step for a
    /// delta parsed from a source or read from storage. A delta *derived* inside
    /// the domain reaches the same invariant through
    /// [`ZatoshisFlowSum::delta`](super::ZatoshisFlowSum::delta).
    pub fn try_new(value: i64) -> Result<Self, ZatoshisDeltaOverflow> {
        Self::try_from_i128(i128::from(value))
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
    pub fn try_from_i128(value: i128) -> Result<Self, ZatoshisDeltaOverflow> {
        i64::try_from(value)
            .ok()
            .filter(|inner| inner.unsigned_abs() <= MAX_ZATOSHIS)
            .map(Self)
            .ok_or(ZatoshisDeltaOverflow { got: value })
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

impl From<ZatoshisDelta> for i64 {
    fn from(z: ZatoshisDelta) -> Self {
        z.0
    }
}

impl fmt::Display for ZatoshisDelta {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delta(value: i64) -> ZatoshisDelta {
        ZatoshisDelta::try_new(value).expect("within the supply")
    }

    #[test]
    fn spend_receive() {
        assert!(delta(-100).is_spend());
        assert!(delta(100).is_receive());
        assert!(!delta(0).is_spend());
        assert!(!delta(0).is_receive());
    }

    #[test]
    fn try_new_accepts_supply_magnitude() {
        let max = i64::try_from(MAX_ZATOSHIS).expect("supply fits in i64");
        assert_eq!(ZatoshisDelta::try_new(max).map(i64::from), Ok(max));
        assert_eq!(ZatoshisDelta::try_new(-max).map(i64::from), Ok(-max));
    }

    #[test]
    fn try_new_rejects_magnitude_past_supply() {
        let over = i64::try_from(MAX_ZATOSHIS).expect("supply fits in i64") + 1;
        assert!(ZatoshisDelta::try_new(over).is_err());
        assert!(ZatoshisDelta::try_new(-over).is_err());
        assert!(ZatoshisDelta::try_new(i64::MIN).is_err());
    }

    #[test]
    fn try_from_i128_accepts_supply_magnitude() {
        let max = i128::from(MAX_ZATOSHIS);
        assert_eq!(
            ZatoshisDelta::try_from_i128(max).map(i64::from),
            Ok(i64::try_from(MAX_ZATOSHIS).expect("supply fits in i64"))
        );
        assert_eq!(
            ZatoshisDelta::try_from_i128(-max).map(i64::from),
            Ok(-i64::try_from(MAX_ZATOSHIS).expect("supply fits in i64"))
        );
    }

    #[test]
    fn try_from_i128_rejects_magnitude_past_supply() {
        let over = i128::from(MAX_ZATOSHIS) + 1;
        assert_eq!(
            ZatoshisDelta::try_from_i128(over),
            Err(ZatoshisDeltaOverflow { got: over })
        );
        assert_eq!(
            ZatoshisDelta::try_from_i128(-over),
            Err(ZatoshisDeltaOverflow { got: -over })
        );
    }

    #[test]
    fn try_from_i128_rejects_beyond_i64() {
        let huge = i128::from(i64::MAX) + 1;
        assert_eq!(
            ZatoshisDelta::try_from_i128(huge),
            Err(ZatoshisDeltaOverflow { got: huge })
        );
    }
}
