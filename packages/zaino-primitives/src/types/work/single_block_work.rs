//! The work one block is expected to take.

use core::fmt;
use core::num::NonZeroU128;

/// The work one block is expected to take, derived from its difficulty target.
///
/// Strictly positive. A valid difficulty target always yields non-zero work, so
/// zero is not a value of this quantity and cannot be represented.
///
/// Deliberately not `Ord`, and folded into
/// [`AbsoluteChainWork`](super::AbsoluteChainWork) rather than compared with
/// it. The module documentation explains both.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct SingleBlockWork(NonZeroU128);

/// Error when a work value is zero.
///
/// Zero is not a work value. It signals an integer that is not work at all — an
/// unset field, a corrupt row — so it is refused rather than accepted as a
/// smallest element that would sort below every real chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("work is strictly positive; zero is not a work value")]
pub struct ZeroWork;

impl SingleBlockWork {
    /// Create a block work value, rejecting zero.
    ///
    /// Takes a work integer that has already been computed elsewhere.
    pub fn try_new(value: u128) -> Result<Self, ZeroWork> {
        NonZeroU128::new(value).map(Self).ok_or(ZeroWork)
    }

    /// The raw value, for the relations that fold it into a chain total.
    pub(super) const fn into_raw(self) -> NonZeroU128 {
        self.0
    }
}

impl From<SingleBlockWork> for NonZeroU128 {
    fn from(work: SingleBlockWork) -> Self {
        work.0
    }
}

impl From<NonZeroU128> for SingleBlockWork {
    /// A non-zero integer carries this type's whole invariant, so the
    /// conversion is total.
    fn from(value: NonZeroU128) -> Self {
        Self(value)
    }
}

impl fmt::Debug for SingleBlockWork {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("SingleBlockWork")
            .field(&format_args!("{:#x}", self.0))
            .finish()
    }
}

impl fmt::Display for SingleBlockWork {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#x}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_is_rejected() {
        assert_eq!(SingleBlockWork::try_new(0), Err(ZeroWork));
    }

    #[test]
    fn nonzero_round_trips() {
        let work = SingleBlockWork::try_new(0x1f1f).expect("nonzero");
        assert_eq!(NonZeroU128::from(work).get(), 0x1f1f);
    }
}
