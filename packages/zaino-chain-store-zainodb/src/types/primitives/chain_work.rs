//! Cumulative proof-of-work for chain selection.
//!
//! `ChainWork` represents a strictly positive amount of proof-of-work.
//! The chain with the greatest cumulative `ChainWork` is the best chain.
//!
//! Zero is not representable — there is no valid block with zero cumulative
//! work, since even the genesis block contributes nonzero work.

use std::fmt;
use std::num::NonZeroU128;

/// A strictly positive amount of proof-of-work.
///
/// Used for cumulative chain work and single-block work contributions.
/// Implements [`Ord`] for chain selection (heaviest chain wins).
///
/// Zero is not representable: every block — including genesis — has
/// nonzero work, so cumulative chainwork is always positive.
///
/// # Construction
///
/// - **From a block header**: [`super::CompactDifficulty::to_work`]
///   produces a `ChainWork` from the block's compact difficulty, walking
///   through Zebra's validated conversion chain internally.
/// - **Arithmetic**: [`add`](Self::add) / [`sub`](Self::sub) on existing values.
/// - **Deserialization**: [`new`](Self::new) from a `NonZeroU128`. The
///   semantic invariant — that the value was originally produced through
///   valid proof-of-work arithmetic — is the responsibility of the caller
///   (typically the DB persistence layer, which trusts previously stored data).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChainWork(NonZeroU128);

/// Errors from [`ChainWork`] arithmetic.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ChainWorkError {
    /// Addition overflowed `u128`.
    #[error("chainwork addition overflowed u128")]
    Overflow,
    /// Subtraction would produce zero or underflow.
    #[error("chainwork subtraction would produce zero or underflow")]
    Underflow,
}

impl ChainWork {
    /// Construct a `ChainWork` from a `NonZeroU128`.
    ///
    /// The non-zero invariant is enforced by the type system. The semantic
    /// invariant — that this value was originally computed from valid
    /// proof-of-work — is the caller's responsibility.
    pub fn new(value: NonZeroU128) -> Self {
        Self(value)
    }

    /// Accumulate another block's work onto this running total.
    pub fn add(&self, other: &Self) -> Result<Self, ChainWorkError> {
        self.0
            .checked_add(other.0.get())
            .map(Self)
            .ok_or(ChainWorkError::Overflow)
    }

    /// Subtract work (used when rolling back blocks from a chain tip).
    ///
    /// Returns an error if `other >= self` (the result would be zero
    /// or negative).
    pub fn sub(&self, other: &Self) -> Result<Self, ChainWorkError> {
        self.0
            .get()
            .checked_sub(other.0.get())
            .and_then(NonZeroU128::new)
            .map(Self)
            .ok_or(ChainWorkError::Underflow)
    }

    /// Return the inner value.
    pub fn as_non_zero_u128(&self) -> NonZeroU128 {
        self.0
    }
}

impl fmt::Debug for ChainWork {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ChainWork")
            .field(&format_args!("{:#x}", self.0))
            .finish()
    }
}

impl fmt::Display for ChainWork {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#x}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cw(value: u128) -> ChainWork {
        ChainWork::new(NonZeroU128::new(value).expect("test value must be nonzero"))
    }

    #[test]
    fn add_is_commutative() {
        let a = cw(100);
        let b = cw(200);
        assert_eq!(a.add(&b), b.add(&a));
    }

    #[test]
    fn sub_inverts_add() {
        let base = cw(1000);
        let delta = cw(300);
        let sum = base.add(&delta).expect("no overflow");
        assert_eq!(sum.sub(&delta), Ok(base));
    }

    #[test]
    fn ord_selects_heavier_chain() {
        assert!(cw(200) > cw(100));
    }

    #[test]
    fn round_trip() {
        let original = NonZeroU128::new(0xdead_beef_cafe).expect("nonzero");
        let chain_work = ChainWork::new(original);
        assert_eq!(chain_work.as_non_zero_u128(), original);
    }

    #[test]
    fn sub_to_zero_is_error() {
        let a = cw(42);
        assert_eq!(a.sub(&a), Err(ChainWorkError::Underflow));
    }

    #[test]
    fn sub_underflow_is_error() {
        let small = cw(1);
        let big = cw(100);
        assert_eq!(small.sub(&big), Err(ChainWorkError::Underflow));
    }

    #[test]
    fn add_overflow_is_error() {
        let max = cw(u128::MAX);
        assert_eq!(max.add(&cw(1)), Err(ChainWorkError::Overflow));
    }
}
