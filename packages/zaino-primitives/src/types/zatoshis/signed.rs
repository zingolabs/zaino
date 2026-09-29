//! A signed zatoshi value: a movement or a difference

use core::fmt;

use super::MAX_ZATOSHIS;

/// `-supply ..= supply` (positive = gained, negative = lost)
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SignedZatoshis(i64);

/// Magnitude past the money supply (corrupt input, never a real figure)
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("signed zatoshi value {got} exceeds supply magnitude {MAX_ZATOSHIS}")]
pub struct SignedZatoshisOverflow {
    pub got: i64,
}

impl SignedZatoshis {
    pub fn new(value: i64) -> Result<Self, SignedZatoshisOverflow> {
        if value.unsigned_abs() <= MAX_ZATOSHIS {
            Ok(Self(value))
        } else {
            Err(SignedZatoshisOverflow { got: value })
        }
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

    /// The supply magnitude either way is accepted; one past it (and `i64::MIN`) is refused,
    /// reporting the rejected value
    #[test]
    fn bounded_by_the_supply_in_both_directions() {
        let max = i64::try_from(MAX_ZATOSHIS).expect("supply fits in i64");
        assert_eq!(SignedZatoshis::new(max).map(i64::from), Ok(max));
        assert_eq!(SignedZatoshis::new(-max).map(i64::from), Ok(-max));
        for over in [max + 1, -(max + 1), i64::MIN] {
            assert_eq!(SignedZatoshis::new(over), Err(SignedZatoshisOverflow { got: over }));
        }
    }
}
