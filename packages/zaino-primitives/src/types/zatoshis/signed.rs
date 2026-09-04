//! The signed zatoshi quantity: a movement or a difference.

use core::fmt;

use super::MAX_ZATOSHIS;

/// A signed zatoshi value, bounded by `-supply ..= supply`.
///
/// Positive is value gained, negative value lost. Two provenances share the
/// type: a directional movement parsed at a boundary — a single input or output
/// value, via [`try_new`](SignedZatoshis::try_new) — and the difference of two
/// totals derived in the domain, via
/// [`ZatoshisFlowSum::minus`](super::ZatoshisFlowSum::minus). Both are bounded by
/// the supply: a single amount cannot exceed it, and a change in an aggregate
/// balance (which lives in `[0, supply]`) cannot either.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SignedZatoshis(i64);

/// Error when a signed zatoshi value's magnitude exceeds the money supply.
///
/// A magnitude larger than the supply is neither a representable movement — a
/// single amount cannot exceed the supply — nor a representable balance change,
/// since an aggregate balance lives in `[0, supply]`. It signals corrupt input
/// rather than a legitimate figure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("signed zatoshi value {got} exceeds supply magnitude {MAX_ZATOSHIS}")]
pub struct SignedZatoshisOverflow {
    /// The value that was rejected.
    pub got: i128,
}

impl SignedZatoshis {
    /// Create a signed value from one read at a boundary, enforcing that its
    /// magnitude fits the money supply.
    ///
    /// This is the boundary door — the external-input validation step for a
    /// signed zatoshi parsed from a source or read from storage, typically a
    /// single directional movement. A single amount cannot exceed the supply, so
    /// a magnitude that does signals corrupt input and is rejected rather than
    /// truncated. A value *derived* inside the domain reaches the same bound
    /// through [`ZatoshisFlowSum::minus`](super::ZatoshisFlowSum::minus).
    pub fn try_new(value: i64) -> Result<Self, SignedZatoshisOverflow> {
        Self::try_from_i128(i128::from(value))
    }

    /// Create a signed value from a wide integer, enforcing that its magnitude
    /// fits the money supply.
    ///
    /// The difference of two flow totals is a change in an aggregate balance,
    /// which lives in `[-supply, supply]`; a result outside that range is not
    /// representable and is rejected rather than truncated. Takes an `i128`
    /// because the caller accumulates gross flow in a wide integer before
    /// subtracting; the returned type re-establishes the narrower bound.
    /// Module-internal: the derived-value door is
    /// [`ZatoshisFlowSum::minus`](super::ZatoshisFlowSum::minus), and the
    /// boundary door is [`try_new`](Self::try_new).
    pub(super) fn try_from_i128(value: i128) -> Result<Self, SignedZatoshisOverflow> {
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

    fn signed(value: i64) -> SignedZatoshis {
        SignedZatoshis::try_new(value).expect("within the supply")
    }

    #[test]
    fn spend_receive() {
        assert!(signed(-100).is_spend());
        assert!(signed(100).is_receive());
        assert!(!signed(0).is_spend());
        assert!(!signed(0).is_receive());
    }

    #[test]
    fn try_new_accepts_supply_magnitude() {
        let max = i64::try_from(MAX_ZATOSHIS).expect("supply fits in i64");
        assert_eq!(SignedZatoshis::try_new(max).map(i64::from), Ok(max));
        assert_eq!(SignedZatoshis::try_new(-max).map(i64::from), Ok(-max));
    }

    #[test]
    fn try_new_rejects_magnitude_past_supply() {
        let over = i64::try_from(MAX_ZATOSHIS).expect("supply fits in i64") + 1;
        assert!(SignedZatoshis::try_new(over).is_err());
        assert!(SignedZatoshis::try_new(-over).is_err());
        assert!(SignedZatoshis::try_new(i64::MIN).is_err());
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
