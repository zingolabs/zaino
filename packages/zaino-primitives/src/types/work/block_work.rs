//! The single-block quantity: the expected work of one block.

use core::fmt;
use core::num::NonZeroU128;

/// The expected work of one block, derived from its difficulty target.
///
/// Strictly positive: a valid difficulty target always yields non-zero work,
/// so zero is not a work value and is not representable.
///
/// This is *not* a chain-selection candidate — comparing single blocks by work
/// decides nothing, which is why the type carries no ordering. Its role is to
/// be folded into a [`ChainWork`](super::ChainWork) through the relations in
/// the `arithmetic` module: seeding at genesis, accumulating forward, rolling
/// back on reorg.
///
/// The value comes from the difficulty pipeline —
/// [`CompactDifficulty::to_work`](crate::types::CompactDifficulty::to_work) —
/// or, for a work integer computed elsewhere, through
/// [`try_new`](Self::try_new), which enforces only the strictly-positive
/// bound.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct BlockWork(NonZeroU128);

/// Error when a work value is zero.
///
/// Work is strictly positive: every valid difficulty target yields non-zero
/// work, and every chain — genesis included — has accumulated at least one
/// block's worth. A zero signals a value that was never work (an unset field,
/// a corrupt row), and is rejected rather than smuggled in as a smallest
/// element that would sort below every real chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("work is strictly positive; zero is not a work value")]
pub struct ZeroWork;

impl BlockWork {
    /// Create a block work value, rejecting zero.
    ///
    /// The boundary door for an already-computed work integer. A
    /// difficulty-to-work conversion never yields zero for a valid target, so
    /// a zero signals a value that is not work at all, and is refused rather
    /// than wrapped.
    pub fn try_new(value: u128) -> Result<Self, ZeroWork> {
        NonZeroU128::new(value).map(Self).ok_or(ZeroWork)
    }

    /// The raw value, for the arithmetic relations to fold.
    pub(super) const fn into_raw(self) -> NonZeroU128 {
        self.0
    }
}

impl From<BlockWork> for NonZeroU128 {
    fn from(work: BlockWork) -> Self {
        work.0
    }
}

impl From<NonZeroU128> for BlockWork {
    /// A non-zero integer already carries this type's whole invariant — work
    /// is strictly positive and nothing more — so the conversion is total.
    /// The difficulty pipeline lands its result here without a runtime check.
    fn from(value: NonZeroU128) -> Self {
        Self(value)
    }
}

impl fmt::Debug for BlockWork {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("BlockWork")
            .field(&format_args!("{:#x}", self.0))
            .finish()
    }
}

impl fmt::Display for BlockWork {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#x}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_is_rejected() {
        assert_eq!(BlockWork::try_new(0), Err(ZeroWork));
    }

    #[test]
    fn nonzero_round_trips() {
        let work = BlockWork::try_new(0x1f1f).expect("nonzero");
        assert_eq!(NonZeroU128::from(work).get(), 0x1f1f);
    }
}
