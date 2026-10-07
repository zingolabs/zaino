//! Block height on the Zcash chain

use core::fmt;

/// `2^31 - 1` (Zcash protocol limit, = Zebra's)
const MAX_HEIGHT: u32 = (1 << 31) - 1;

/// `<= MAX_HEIGHT`, enforced at construction; all arithmetic checked
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Height(u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("height {got} exceeds protocol maximum {MAX_HEIGHT}")]
pub struct HeightOverflow {
    pub got: u64,
}

impl Height {
    pub const GENESIS: Self = Self(0);

    /// `None` past the protocol maximum
    pub fn checked_add(self, delta: u32) -> Option<Self> {
        let sum = self.0.checked_add(delta)?;
        if sum > MAX_HEIGHT {
            return None;
        }
        Some(Self(sum))
    }

    /// `self + 1` (panics past the protocol maximum: no chain gets there)
    pub fn next(self) -> Self {
        self.checked_add(1).expect("height one past the protocol maximum")
    }

    pub fn checked_sub(self, delta: u32) -> Option<Self> {
        self.0.checked_sub(delta).map(Self)
    }

    pub fn saturating_sub(self, delta: u32) -> Self {
        Self(self.0.saturating_sub(delta))
    }

    /// `self` to `end`, both inclusive, ascending (empty when `end < self`)
    pub fn up_to(self, end: Height) -> impl Iterator<Item = Height> + Send + 'static {
        (self.0..=end.0).map(Self)
    }
}

impl TryFrom<u32> for Height {
    type Error = HeightOverflow;

    fn try_from(h: u32) -> Result<Self, Self::Error> {
        Self::try_from(u64::from(h))
    }
}

impl TryFrom<u64> for Height {
    type Error = HeightOverflow;

    fn try_from(h: u64) -> Result<Self, Self::Error> {
        match u32::try_from(h) {
            Ok(fits) if fits <= MAX_HEIGHT => Ok(Self(fits)),
            _ => Err(HeightOverflow { got: h }),
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

    /// Construction from `u32` and `u64` agrees up to `MAX_HEIGHT` and names the rejected value
    /// past it; arithmetic stays inside `0..=MAX_HEIGHT`
    #[test]
    fn heights_hold_to_the_protocol_maximum_from_either_width_and_arithmetic_stays_inside() {
        let max = MAX_HEIGHT;
        assert_eq!(u32::from(Height::GENESIS), 0);
        assert_eq!(Height::try_from(max).map(u32::from), Ok(max));
        assert_eq!(Height::try_from(u64::from(max)), Height::try_from(max));
        assert_eq!(Height::try_from(max + 1), Err(HeightOverflow { got: u64::from(max) + 1 }));
        let past_u32 = u64::from(u32::MAX) + 1;
        assert_eq!(Height::try_from(past_u32), Err(HeightOverflow { got: past_u32 }));

        let ten = Height::try_from(10u32).expect("valid");
        assert_eq!((u32::from(ten), u64::from(ten)), (10, 10));
        assert_eq!(ten.checked_add(5).map(u32::from), Some(15));
        assert_eq!(Height::try_from(max).expect("valid").checked_add(1), None);
        assert_eq!(Height::GENESIS.checked_sub(1), None);
        assert_eq!(Height::GENESIS.saturating_sub(100), Height::GENESIS);
        assert!(Height::GENESIS < ten);
        assert_eq!((format!("{ten}"), format!("{ten:?}")), ("10".into(), "Height(10)".into()));
    }
}
