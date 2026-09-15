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
/// The value itself comes from a consensus implementation's
/// difficulty-to-work conversion, which proves it non-zero before
/// [`new`](Self::new) wraps it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct BlockWork(NonZeroU128);

impl BlockWork {
    /// Wraps an already non-zero block work value.
    pub const fn new(value: NonZeroU128) -> Self {
        Self(value)
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

    const SAMPLE: NonZeroU128 = NonZeroU128::new(0x1f1f).expect("nonzero literal");

    #[test]
    fn nonzero_round_trips() {
        let work = BlockWork::new(SAMPLE);
        assert_eq!(NonZeroU128::from(work), SAMPLE);
    }
}
