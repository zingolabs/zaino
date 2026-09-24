//! The total work of a chain up to a block.

use core::fmt;
use core::num::NonZeroU128;

use super::{SingleBlockWork, WorkOverflow};

/// The total work of a chain up to and including a block.
///
/// `Ord`. The `types::work` module documentation states the algebra and why
/// the three quantities are distinct.
///
/// Strictly positive. A validator that does not track the value, or a block
/// with no parent, is `Option<AbsoluteChainWork>`; absence is never a zero.
///
/// Recorded in 128 bits, which real chains do not approach.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AbsoluteChainWork(NonZeroU128);

/// Error when [`rollback`](AbsoluteChainWork::rollback) reaches or crosses
/// zero.
///
/// The result must stay strictly positive, because the chain still contains
/// genesis. Crossing that floor means the work being unwound was never part of
/// this total.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("unwinding a block's work would take the total to or below zero")]
pub struct WorkUnderflow;

/// Error when 32 big-endian bytes are not the byte form of a value of this type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ChainWorkBytesError {
    /// The high-order 128 bits are set, so the value does not fit the recorded width.
    #[error("chainwork does not fit 128 bits (high half {high:#034x})")]
    OverWidth {
        /// The non-zero high-order 128 bits.
        high: u128,
    },
    /// The bytes are all zero, which no chain's total work can be.
    #[error("chainwork is zero")]
    Zero,
}

impl AbsoluteChainWork {
    /// Create a total chain work value.
    ///
    /// Infallible: the strictly-positive bound travels in the argument type.
    pub const fn new(value: NonZeroU128) -> Self {
        Self(value)
    }

    /// Render as the 32 big-endian bytes the wire carries. Widening 128 bits to
    /// 256 cannot lose anything.
    pub fn to_be_bytes(&self) -> [u8; 32] {
        let mut bytes = [0u8; 32];
        bytes[16..].copy_from_slice(&self.0.get().to_be_bytes());
        bytes
    }

    /// Reads the 32 big-endian byte form back, the inverse of [`to_be_bytes`](Self::to_be_bytes).
    pub fn from_be_bytes(bytes: [u8; 32]) -> Result<Self, ChainWorkBytesError> {
        let (high, low) = bytes.split_at(16);
        let high = u128::from_be_bytes(high.try_into().expect("split_at(16) leaves 16 bytes"));
        if high != 0 {
            return Err(ChainWorkBytesError::OverWidth { high });
        }
        let low = u128::from_be_bytes(low.try_into().expect("split_at(16) leaves 16 bytes"));
        NonZeroU128::new(low)
            .map(Self)
            .ok_or(ChainWorkBytesError::Zero)
    }

    /// `genesis : W → C`. A chain of one block has that block's work.
    ///
    /// The only way a total comes into being other than by extending or
    /// unwinding another.
    pub fn genesis(work: SingleBlockWork) -> Self {
        Self(work.into_raw())
    }

    /// `accumulate : C × W → C`. Extend the chain by one block.
    pub fn accumulate(self, work: SingleBlockWork) -> Result<Self, WorkOverflow> {
        self.0
            .checked_add(work.into_raw().get())
            .map(Self)
            .ok_or(WorkOverflow)
    }

    /// `rollback : C × W → C`. Unwind one block, on reorg.
    pub fn rollback(self, work: SingleBlockWork) -> Result<Self, WorkUnderflow> {
        self.0
            .get()
            .checked_sub(work.into_raw().get())
            .and_then(NonZeroU128::new)
            .map(Self)
            .ok_or(WorkUnderflow)
    }
}

impl From<AbsoluteChainWork> for NonZeroU128 {
    fn from(work: AbsoluteChainWork) -> Self {
        work.0
    }
}

impl fmt::Debug for AbsoluteChainWork {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("AbsoluteChainWork")
            .field(&format_args!("{:#x}", self.0))
            .finish()
    }
}

impl fmt::Display for AbsoluteChainWork {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#x}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn work(value: u128) -> AbsoluteChainWork {
        AbsoluteChainWork::new(NonZeroU128::new(value).expect("test value must be nonzero"))
    }

    /// The byte form is the value's 16 big-endian bytes in the low half, and
    /// reading it back is the identity.
    #[test]
    fn be_bytes_round_trip() {
        let value = work(0x00de_ad00_beef);
        let bytes = value.to_be_bytes();

        assert_eq!(bytes[..16], [0u8; 16]);
        assert_eq!(bytes[16..], 0x00de_ad00_beefu128.to_be_bytes());
        assert_eq!(AbsoluteChainWork::from_be_bytes(bytes), Ok(value));
    }

    /// A set high half is refused, not truncated: a truncated value would be
    /// a lower total, which reorders chain selection.
    #[test]
    fn over_width_bytes_are_refused() {
        let mut bytes = [0u8; 32];
        bytes[0] = 1;

        assert_eq!(
            AbsoluteChainWork::from_be_bytes(bytes),
            Err(ChainWorkBytesError::OverWidth { high: 1 << 120 })
        );
    }

    /// All-zero bytes are not a value of the type.
    #[test]
    fn zero_bytes_are_refused() {
        assert_eq!(
            AbsoluteChainWork::from_be_bytes([0u8; 32]),
            Err(ChainWorkBytesError::Zero)
        );
    }

    #[test]
    fn ord_selects_the_heavier_chain() {
        assert!(work(200) > work(100));
    }

    fn block(value: u128) -> SingleBlockWork {
        SingleBlockWork::new(NonZeroU128::new(value).expect("test value must be nonzero"))
    }

    /// The genesis seed is the block's own work, counted exactly once.
    #[test]
    fn genesis_seed_is_the_own_block_work() {
        assert_eq!(AbsoluteChainWork::genesis(block(17)), work(17));
    }

    /// `rollback` inverts `accumulate`.
    #[test]
    fn rollback_inverts_accumulate() {
        let base = AbsoluteChainWork::genesis(block(1000));
        let delta = block(300);
        let extended = base.accumulate(delta).expect("no overflow");
        assert_eq!(extended.rollback(delta), Ok(base));
    }

    /// Accumulating gives a heavier chain — the fold feeds the ordering.
    #[test]
    fn accumulation_orders_chains_by_weight() {
        let light = AbsoluteChainWork::genesis(block(100));
        assert!(light.accumulate(block(1)).expect("no overflow") > light);
    }

    #[test]
    fn accumulate_overflow_is_refused() {
        let max = AbsoluteChainWork::new(NonZeroU128::MAX);
        assert_eq!(max.accumulate(block(1)), Err(WorkOverflow));
    }

    /// Rolling back to exactly zero is refused: a chain always has genesis.
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
