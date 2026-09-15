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

impl SingleBlockWork {
    /// Wraps an already non-zero block work value.
    pub const fn new(value: NonZeroU128) -> Self {
        Self(value)
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

    const SAMPLE: NonZeroU128 = NonZeroU128::new(0x1f1f).expect("nonzero literal");

    #[test]
    fn nonzero_round_trips() {
        let work = SingleBlockWork::new(SAMPLE);
        assert_eq!(NonZeroU128::from(work), SAMPLE);
    }
}
