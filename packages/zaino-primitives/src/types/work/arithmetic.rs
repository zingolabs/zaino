//! Arithmetic over the work quantity family.
//!
//! Cross-type operations are relations between quantities, not methods of a
//! single one, so they live here beside the types rather than on either. This
//! module is also where the allowed operations — the algebra — are written
//! down as the specification a new fold site inherits.
//!
//! # The algebra
//!
//! Write `W` for one block's expected work, `C` for cumulative work at a
//! block, and `R` for work accumulated since an anchor. `W` and `C` are
//! strictly positive; `R` admits zero:
//!
//! ```text
//! W ∈ (0, 2^128)      the expected work of one block
//! C ∈ (0, 2^128)      cumulative work: the fold of block works along a chain
//! R ∈ [0, 2^128)      relative work: the fold of block works since an anchor
//! ```
//!
//! Six relations are defined, and only these six:
//!
//! ```text
//! genesis    : W → C          the fold's seed — a chain of one block
//! accumulate : C × W → C      extend the chain by one block; refused on overflow
//! rollback   : C × W → C      unwind one block (reorg); refused at or below zero
//! accumulate : R × W → R      extend the branch by one block; refused on overflow
//! extend     : C × R → C      absolute work at a branch tip: anchor ⊕ relative; refused on overflow
//! since      : C × C → R      work above an anchor: self − anchor; refused below the anchor
//! ```
//!
//! Together with `C`'s derived ordering — chain selection, the operation the
//! fold exists to feed — that is the whole algebra. `C × C` exists in exactly
//! one legal form, `since`, and its result is *relative*, never absolute.
//! Adding two absolute values remains unexpressible: no chain is the
//! concatenation of two chains, so a sum of cumulative works is not a
//! quantity in this domain and no operation returns one. See ADR-0013.

use super::{BlockWork, ChainWork, RelativeWork};

/// Error when a work addition overflows the recorded width.
///
/// Returned by every adding relation — `accumulate` on either fold, and
/// `extend`. Unreachable on real chains — cumulative work is nowhere near
/// `2^128` — but the fold stays checked so a corrupt input fails loud rather
/// than wrapping into a small, wrongly ordered cumulative work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("adding a work value overflowed the recorded width")]
pub struct WorkOverflow;

/// Error when a work subtraction goes below its result's floor.
///
/// Two relations subtract, each with its own floor. A `rollback` unwinds a
/// block that the cumulative value once accumulated, so the result must stay
/// strictly positive — the chain still contains at least genesis. A `since`
/// measures work above an anchor, so the anchor may equal the value (the
/// branch has accumulated nothing) but never exceed it. Going below either
/// floor means the operands were never related as the relation assumes, and
/// the fold refuses rather than invents a quantity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("subtracting work would take the result below its quantity's floor")]
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

    /// Combine an anchor's absolute work with a branch's relative work.
    ///
    /// The `extend` relation: `C × R → C`. The finalised tip carries absolute
    /// cumulative work; a non-finalised branch carries the work accumulated
    /// since that anchor; their combination is the absolute cumulative work
    /// at the branch tip. Extending by [`RelativeWork::ZERO`] is the
    /// identity. Checked: overflow is refused, as everywhere in the fold.
    pub fn extend(self, relative: RelativeWork) -> Result<Self, WorkOverflow> {
        self.into_raw()
            .checked_add(u128::from(relative))
            .map(Self::from_raw)
            .ok_or(WorkOverflow)
    }

    /// Measure this chain's work above an anchor.
    ///
    /// The `since` relation: `C × C → R` — the one legal combination of two
    /// cumulative values, and its result is relative, never absolute. An
    /// anchor equal to `self` yields [`RelativeWork::ZERO`]: the branch has
    /// accumulated nothing. An anchor above `self` is refused — it does not
    /// lie below this value on any chain.
    pub fn since(self, anchor: ChainWork) -> Result<RelativeWork, WorkUnderflow> {
        self.into_raw()
            .get()
            .checked_sub(anchor.into_raw().get())
            .map(RelativeWork::new)
            .ok_or(WorkUnderflow)
    }
}

impl RelativeWork {
    /// Extend this branch's relative work by one block's work.
    ///
    /// The `accumulate` relation on the relative quantity: `R × W → R`. The
    /// same checked fold as [`ChainWork::accumulate`], seeded at
    /// [`RelativeWork::ZERO`] — the anchor — instead of at genesis.
    pub fn accumulate(self, work: BlockWork) -> Result<Self, WorkOverflow> {
        u128::from(self)
            .checked_add(work.into_raw().get())
            .map(Self::new)
            .ok_or(WorkOverflow)
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

    /// The round-trip law: extending an anchor by the work since it recovers
    /// the tip.
    #[test]
    fn extend_inverts_since() {
        let anchor = ChainWork::genesis(block(1000));
        let tip = anchor.accumulate(block(300)).expect("no overflow");
        let relative = tip.since(anchor).expect("anchor at or below tip");
        assert_eq!(anchor.extend(relative), Ok(tip));
    }

    /// A tip that is its own anchor has accumulated nothing.
    #[test]
    fn since_of_equal_values_is_zero() {
        let tip = ChainWork::genesis(block(77));
        assert_eq!(tip.since(tip), Ok(RelativeWork::ZERO));
    }

    #[test]
    fn since_with_anchor_above_is_refused() {
        let low = ChainWork::genesis(block(10));
        let high = low.accumulate(block(5)).expect("no overflow");
        assert_eq!(low.since(high), Err(WorkUnderflow));
    }

    /// Extending by zero is the identity: the branch adds nothing.
    #[test]
    fn extend_with_zero_is_identity() {
        let anchor = ChainWork::genesis(block(123));
        assert_eq!(anchor.extend(RelativeWork::ZERO), Ok(anchor));
    }

    #[test]
    fn extend_overflow_is_refused() {
        let max = ChainWork::try_new(u128::MAX).expect("nonzero");
        assert_eq!(max.extend(RelativeWork::new(1)), Err(WorkOverflow));
    }

    /// The relative fold seeds at zero — a branch tip sitting at its anchor.
    #[test]
    fn relative_accumulate_from_zero() {
        let one = RelativeWork::ZERO
            .accumulate(block(7))
            .expect("no overflow");
        assert_eq!(u128::from(one), 7);
    }

    #[test]
    fn relative_accumulate_overflow_is_refused() {
        assert_eq!(
            RelativeWork::new(u128::MAX).accumulate(block(1)),
            Err(WorkOverflow)
        );
    }
}
