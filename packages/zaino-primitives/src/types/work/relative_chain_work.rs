//! Work accumulated over a run of blocks.

use core::fmt;

use super::{SingleBlockWork, WorkOverflow};

/// Work accumulated over a run of consecutive blocks.
///
/// A sum over a set of blocks, not an offset from a single one. [`ZERO`] is the
/// empty run, and each block folded in adds its own work. So a value says how
/// much work the run holds, and nothing about where the run begins.
///
/// Ordered, because picking the heaviest of several runs is what the ordering
/// is for. Two runs are only comparable when they begin at the same block, and
/// the type does not carry where it began, so it cannot check that. A caller
/// holding several runs is responsible for their sharing a start.
///
/// [`ZERO`]: Self::ZERO
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RelativeChainWork(u128);

impl RelativeChainWork {
    /// The empty run, which has accumulated nothing.
    pub const ZERO: Self = Self(0);

    /// `accumulate : R × W → R`. Extend the run by one block.
    pub fn accumulate(self, work: SingleBlockWork) -> Result<Self, WorkOverflow> {
        self.0
            .checked_add(work.into_raw().get())
            .map(Self)
            .ok_or(WorkOverflow)
    }
}

impl From<RelativeChainWork> for u128 {
    fn from(work: RelativeChainWork) -> Self {
        work.0
    }
}

impl fmt::Debug for RelativeChainWork {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("RelativeChainWork")
            .field(&format_args!("{:#x}", self.0))
            .finish()
    }
}

impl fmt::Display for RelativeChainWork {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#x}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use core::num::NonZeroU128;

    use super::*;

    fn block(value: u128) -> SingleBlockWork {
        SingleBlockWork::new(NonZeroU128::new(value).expect("test value must be nonzero"))
    }

    fn run(values: [u128; 2]) -> RelativeChainWork {
        values
            .into_iter()
            .fold(RelativeChainWork::ZERO, |run, value| {
                run.accumulate(block(value)).expect("no overflow")
            })
    }

    /// Zero is a value of the quantity, not an absence.
    #[test]
    fn zero_is_the_empty_run() {
        assert_eq!(u128::from(RelativeChainWork::ZERO), 0);
    }

    /// The fold needs no special case for its first block.
    #[test]
    fn the_run_totals_every_block_it_holds() {
        assert_eq!(u128::from(run([7, 11])), 18);
    }

    /// Extending a run outweighs the run it extends — what the ordering is for.
    #[test]
    fn a_longer_run_outweighs_the_run_it_extends() {
        let shorter = RelativeChainWork::ZERO
            .accumulate(block(100))
            .expect("no overflow");
        assert!(shorter.accumulate(block(1)).expect("no overflow") > shorter);
    }

    #[test]
    fn overflow_is_refused() {
        let max = RelativeChainWork::ZERO
            .accumulate(block(u128::MAX))
            .expect("no overflow");
        assert_eq!(max.accumulate(block(1)), Err(WorkOverflow));
    }
}
