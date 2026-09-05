//! Arithmetic over the work quantity family.
//!
//! Cross-type operations are relations between quantities, not methods of a
//! single one, so they live here beside the types rather than on either. This
//! module is also where the allowed operations — the algebra — are written
//! down as the specification a new fold site inherits.
//!
//! # The algebra
//!
//! Write `W` for one block's expected work and `C` for cumulative work at a
//! block. Both are strictly positive:
//!
//! ```text
//! W ∈ (0, 2^128)      the expected work of one block
//! C ∈ (0, 2^128)      cumulative work: the fold of block works along a chain
//! ```
//!
//! Three relations are defined, and only these three:
//!
//! ```text
//! genesis    : W → C          the fold's seed — a chain of one block
//! accumulate : C × W → C      extend the chain by one block; refused on overflow
//! rollback   : C × W → C      unwind one block (reorg); refused at or below zero
//! ```
//!
//! Together with `C`'s derived ordering — chain selection, the operation the
//! fold exists to feed — that is the whole algebra. There is deliberately no
//! `C × C → C`: adding two cumulative values is meaningless, because no chain
//! is the concatenation of two chains. A sum of cumulative works is not a
//! quantity in this domain, so no operation returns one. See ADR-0013.

use super::{BlockWork, ChainWork};

/// Error when accumulating a block's work overflows the recorded width.
///
/// Unreachable on real chains — cumulative work is nowhere near `2^128` — but
/// the fold stays checked so a corrupt input fails loud rather than wrapping
/// into a small, wrongly ordered cumulative work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("accumulating a block's work overflowed the cumulative width")]
pub struct WorkOverflow;

/// Error when rolling back a block's work reaches or crosses zero.
///
/// A rollback unwinds a block that the cumulative value once accumulated, so
/// the result must stay strictly positive — the chain still contains at least
/// genesis. Reaching zero or below means the block work being unwound was
/// never part of this accumulation, and the fold refuses rather than invents
/// a chain with no blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("rolling back a block's work would take cumulative work to or below zero")]
pub struct WorkUnderflow;

impl ChainWork {
    /// Seed the fold at genesis.
    ///
    /// The `genesis` relation: `W → C`. A chain of one block has cumulative
    /// work equal to that block's own work — counted once, not accumulated
    /// onto anything. This is the only way a cumulative value comes into
    /// being other than by extending or unwinding another.
    pub fn genesis(work: BlockWork) -> Self {
        Self::from_raw(work.into_raw())
    }

    /// Extend this chain's cumulative work by one block's work.
    ///
    /// The `accumulate` relation: `C × W → C`. A checked add: overflow is
    /// unreachable on real chains, and refused rather than wrapped so a
    /// corrupt input cannot masquerade as a light chain.
    pub fn accumulate(self, work: BlockWork) -> Result<Self, WorkOverflow> {
        self.into_raw()
            .checked_add(work.into_raw().get())
            .map(Self::from_raw)
            .ok_or(WorkOverflow)
    }

    /// Unwind one block's work from this chain's cumulative work.
    ///
    /// The `rollback` relation: `C × W → C`, for reorgs. Refused if the
    /// result would reach or cross zero: the chain still contains genesis, so
    /// cumulative work stays strictly positive, and a rollback that violates
    /// that is unwinding work this value never accumulated.
    pub fn rollback(self, work: BlockWork) -> Result<Self, WorkUnderflow> {
        self.into_raw()
            .get()
            .checked_sub(work.into_raw().get())
            .and_then(core::num::NonZeroU128::new)
            .map(Self::from_raw)
            .ok_or(WorkUnderflow)
    }
}

#[cfg(test)]
mod tests {
    use core::num::NonZeroU128;

    use super::*;

    fn block(value: u128) -> BlockWork {
        BlockWork::try_new(value).expect("test value must be nonzero")
    }

    /// The genesis seed is the block's own work, counted exactly once.
    #[test]
    fn genesis_seed_is_the_own_block_work() {
        assert_eq!(
            NonZeroU128::from(ChainWork::genesis(block(17))).get(),
            17u128
        );
    }

    /// `rollback` inverts `accumulate`.
    #[test]
    fn rollback_inverts_accumulate() {
        let base = ChainWork::genesis(block(1000));
        let delta = block(300);
        let extended = base.accumulate(delta).expect("no overflow");
        assert_eq!(extended.rollback(delta), Ok(base));
    }

    /// Accumulating heavier blocks yields a heavier chain — the fold feeds a
    /// meaningful ordering.
    #[test]
    fn accumulation_orders_chains_by_weight() {
        let light = ChainWork::genesis(block(100));
        let heavy = light.accumulate(block(1)).expect("no overflow");
        assert!(heavy > light);
    }

    #[test]
    fn accumulate_overflow_is_refused() {
        let max = ChainWork::try_new(u128::MAX).expect("nonzero");
        assert_eq!(max.accumulate(block(1)), Err(WorkOverflow));
    }

    /// Rolling back to exactly zero is refused: a chain always contains
    /// genesis.
    #[test]
    fn rollback_to_zero_is_refused() {
        let genesis = ChainWork::genesis(block(42));
        assert_eq!(genesis.rollback(block(42)), Err(WorkUnderflow));
    }

    #[test]
    fn rollback_past_zero_is_refused() {
        let small = ChainWork::genesis(block(1));
        assert_eq!(small.rollback(block(100)), Err(WorkUnderflow));
    }
}
