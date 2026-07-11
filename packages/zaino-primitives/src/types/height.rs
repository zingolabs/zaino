//! Block height on the Zcash chain.

use core::fmt;

/// Maximum valid block height (Zcash protocol limit, matches Zebra).
///
/// `2^31 - 1`. Heights above this are rejected at construction.
const MAX_HEIGHT: u32 = (1 << 31) - 1;

/// Block height.
///
/// Invariant: the inner value is `≤ MAX_HEIGHT` (`2^31 - 1`).
/// Enforced at construction; all arithmetic is checked.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Height(u32);

/// Error returned when a `u32` exceeds the protocol height limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("height {got} exceeds protocol maximum {MAX_HEIGHT}")]
pub struct HeightOverflow {
    /// The value that was rejected.
    pub got: u32,
}

impl Height {
    /// The genesis block.
    pub const GENESIS: Self = Self(0);

    /// Add a delta, returning `None` on overflow or protocol-limit violation.
    pub fn checked_add(self, delta: u32) -> Option<Self> {
        let sum = self.0.checked_add(delta)?;
        if sum > MAX_HEIGHT {
            return None;
        }
        Some(Self(sum))
    }

    /// Subtract a delta, returning `None` on underflow.
    pub fn checked_sub(self, delta: u32) -> Option<Self> {
        self.0.checked_sub(delta).map(Self)
    }

    /// Subtract, saturating at zero.
    pub fn saturating_sub(self, delta: u32) -> Self {
        Self(self.0.saturating_sub(delta))
    }

    /// Distance between two heights (absolute value).
    pub fn abs_diff(self, other: Self) -> u32 {
        self.0.abs_diff(other.0)
    }
}

impl TryFrom<u32> for Height {
    type Error = HeightOverflow;

    fn try_from(h: u32) -> Result<Self, Self::Error> {
        if h > MAX_HEIGHT {
            Err(HeightOverflow { got: h })
        } else {
            Ok(Self(h))
        }
    }
}

impl From<Height> for u32 {
    fn from(h: Height) -> Self {
        h.0
    }
}

impl From<Height> for u64 {
    fn from(h: Height) -> Self {
        u64::from(h.0)
    }
}

impl fmt::Debug for Height {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Height({})", self.0)
    }
}

impl fmt::Display for Height {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn genesis() {
        assert_eq!(u32::from(Height::GENESIS), 0);
    }

    #[test]
    fn valid_construction() {
        let h = Height::try_from(100).expect("valid height");
        assert_eq!(u32::from(h), 100);
    }

    #[test]
    fn max_is_valid() {
        assert!(Height::try_from(MAX_HEIGHT).is_ok());
    }

    #[test]
    fn above_max_rejected() {
        let err = Height::try_from(MAX_HEIGHT + 1).unwrap_err();
        assert_eq!(err.got, MAX_HEIGHT + 1);
    }

    #[test]
    fn checked_add_within_limit() {
        let h = Height::try_from(10).expect("valid");
        assert_eq!(u32::from(h.checked_add(5).expect("ok")), 15);
    }

    #[test]
    fn checked_add_overflow_returns_none() {
        let h = Height::try_from(MAX_HEIGHT).expect("valid");
        assert!(h.checked_add(1).is_none());
    }

    #[test]
    fn checked_sub_underflow_returns_none() {
        assert!(Height::GENESIS.checked_sub(1).is_none());
    }

    #[test]
    fn saturating_sub_floors_at_zero() {
        assert_eq!(Height::GENESIS.saturating_sub(100), Height::GENESIS);
    }

    #[test]
    fn abs_diff_commutative() {
        let a = Height::try_from(10).expect("valid");
        let b = Height::try_from(25).expect("valid");
        assert_eq!(a.abs_diff(b), 15);
        assert_eq!(b.abs_diff(a), 15);
    }

    #[test]
    fn ordering() {
        let a = Height::try_from(1).expect("valid");
        let b = Height::try_from(2).expect("valid");
        assert!(a < b);
    }

    #[test]
    fn into_u64() {
        let h = Height::try_from(42).expect("valid");
        assert_eq!(u64::from(h), 42u64);
    }
}
