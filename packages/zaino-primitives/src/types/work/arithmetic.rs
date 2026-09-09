//! The relations between the work quantities.
//!
//! An operation over two quantities belongs to neither type, so the three
//! relations live here. This is also the specification a new fold site
//! inherits.
//!
//! # The algebra
//!
//! Write `W` for [`SingleBlockWork`] and `C` for [`AbsoluteChainWork`]:
//!
//! ```text
//! W ∈ (0, 2^128)
//! C ∈ (0, 2^128)
//!
//! genesis    : W → C      a chain of one block
//! accumulate : C × W → C  extend the chain by one block
//! rollback   : C × W → C  unwind one block, on reorg
//! ```
//!
//! Those three and `C`'s ordering are the whole algebra. `C × C` is not
//! defined: no chain is the concatenation of two chains, so the sum of two
//! total chain works is not a quantity in this domain and no operation returns
//! one. See ADR-0013.
//!
//! Both adding relations are checked, and the subtracting one is checked
//! against zero. Neither bound is reachable on a real chain. They stay checked
//! so a corrupt input fails loud instead of wrapping into a small value that
//! would then sort as a light chain.

use core::num::NonZeroU128;

use super::{AbsoluteChainWork, SingleBlockWork};

/// Error when accumulating a block's work overflows the recorded width.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("accumulating a block's work overflowed the cumulative width")]
pub struct WorkOverflow;

/// Error when rolling back a block's work reaches or crosses zero.
///
/// A rollback unwinds a block this value once accumulated, so the result must
/// stay strictly positive: the chain still contains genesis. Crossing that
/// floor means the block work being unwound was never part of this
/// accumulation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("rolling back a block's work would take cumulative work to or below zero")]
pub struct WorkUnderflow;

impl AbsoluteChainWork {
    /// `genesis : W → C`. A chain of one block has that block's work.
    ///
    /// The only way a total chain work comes into being other than by
    /// extending or unwinding another.
    pub fn genesis(work: SingleBlockWork) -> Self {
        Self::new(work.into_raw())
    }

    /// `accumulate : C × W → C`. Extend the chain by one block.
    pub fn accumulate(self, work: SingleBlockWork) -> Result<Self, WorkOverflow> {
        self.into_raw()
            .checked_add(work.into_raw().get())
            .map(Self::new)
            .ok_or(WorkOverflow)
    }

    /// `rollback : C × W → C`. Unwind one block, on reorg.
    pub fn rollback(self, work: SingleBlockWork) -> Result<Self, WorkUnderflow> {
        self.into_raw()
            .get()
            .checked_sub(work.into_raw().get())
            .and_then(NonZeroU128::new)
            .map(Self::new)
            .ok_or(WorkUnderflow)
    }
}

#[cfg(test)]
mod tests {
    use core::num::NonZeroU128;

    use super::*;

    fn block(value: u128) -> SingleBlockWork {
        SingleBlockWork::try_new(value).expect("test value must be nonzero")
    }

    /// The genesis seed is the block's own work, counted exactly once.
    #[test]
    fn genesis_seed_is_the_own_block_work() {
        assert_eq!(
            NonZeroU128::from(AbsoluteChainWork::genesis(block(17))).get(),
            17u128
        );
    }

    /// `rollback` inverts `accumulate`.
    #[test]
    fn rollback_inverts_accumulate() {
        let base = AbsoluteChainWork::genesis(block(1000));
        let delta = block(300);
        let extended = base.accumulate(delta).expect("no overflow");
        assert_eq!(extended.rollback(delta), Ok(base));
    }

    /// Accumulating heavier blocks yields a heavier chain — the fold feeds a
    /// meaningful ordering.
    #[test]
    fn accumulation_orders_chains_by_weight() {
        let light = AbsoluteChainWork::genesis(block(100));
        let heavy = light.accumulate(block(1)).expect("no overflow");
        assert!(heavy > light);
    }

    #[test]
    fn accumulate_overflow_is_refused() {
        let max = AbsoluteChainWork::new(NonZeroU128::MAX);
        assert_eq!(max.accumulate(block(1)), Err(WorkOverflow));
    }

    /// Rolling back to exactly zero is refused: a chain always contains
    /// genesis.
    #[test]
    fn rollback_to_zero_is_refused() {
        let genesis = AbsoluteChainWork::genesis(block(42));
        assert_eq!(genesis.rollback(block(42)), Err(WorkUnderflow));
    }

    #[test]
    fn rollback_past_zero_is_refused() {
        let small = AbsoluteChainWork::genesis(block(1));
        assert_eq!(small.rollback(block(100)), Err(WorkUnderflow));
    }
}
