//! Blockheight, impl taken from zebra-chain for consistancy.

use crate::primitives::error::SerializationError;
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    ops::{Add, Sub},
};
use thiserror::Error;

/// Error type alias to make working with generic errors easier.
///
/// Note: the 'static lifetime bound means that the *type* cannot have any
/// non-'static lifetimes, (e.g., when a type contains a borrow and is
/// parameterized by 'a), *not* that the object itself has 'static lifetime.
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// A wrapper type representing blockchain heights. Safe conversion from
/// various integer types, as well as addition and subtraction, are provided.
///
/// Taken from librustzcash::consensus.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BlockHeight(u32);

impl BlockHeight {
    /// Tries to convert a u32 to a BlockHeight.
    pub const fn from_u32(v: u32) -> BlockHeight {
        BlockHeight(v)
    }

    /// Subtracts the provided value from this height, returning `H0` if this would result in
    /// underflow of the wrapped `u32`.
    pub fn saturating_sub(self, v: u32) -> BlockHeight {
        BlockHeight(self.0.saturating_sub(v))
    }
}

impl fmt::Display for BlockHeight {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl From<u32> for BlockHeight {
    fn from(value: u32) -> Self {
        BlockHeight(value)
    }
}

impl From<BlockHeight> for u32 {
    fn from(value: BlockHeight) -> u32 {
        value.0
    }
}

impl TryFrom<u64> for BlockHeight {
    type Error = std::num::TryFromIntError;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        u32::try_from(value).map(BlockHeight)
    }
}

impl From<BlockHeight> for u64 {
    fn from(value: BlockHeight) -> u64 {
        value.0 as u64
    }
}

impl TryFrom<i32> for BlockHeight {
    type Error = std::num::TryFromIntError;

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        u32::try_from(value).map(BlockHeight)
    }
}

impl TryFrom<i64> for BlockHeight {
    type Error = std::num::TryFromIntError;

    fn try_from(value: i64) -> Result<Self, Self::Error> {
        u32::try_from(value).map(BlockHeight)
    }
}

impl From<BlockHeight> for i64 {
    fn from(value: BlockHeight) -> i64 {
        value.0 as i64
    }
}

impl Add<u32> for BlockHeight {
    type Output = Self;

    fn add(self, other: u32) -> Self {
        BlockHeight(self.0 + other)
    }
}

impl Add for BlockHeight {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        self + other.0
    }
}

impl Sub<u32> for BlockHeight {
    type Output = Self;

    fn sub(self, other: u32) -> Self {
        if other > self.0 {
            panic!("Subtraction resulted in negative block height.");
        }

        BlockHeight(self.0 - other)
    }
}

impl Sub for BlockHeight {
    type Output = Self;

    fn sub(self, other: Self) -> Self {
        self - other.0
    }
}

/// The length of the chain back to the genesis block.
///
/// Two [`Height`]s can't be added, but they can be *subtracted* to get their difference,
/// represented as an [`HeightDiff`]. This difference can then be added to or subtracted from a
/// [`Height`]. Note the similarity with `chrono::DateTime` and `chrono::Duration`.
///
/// # Invariants
///
/// Users should not construct block heights greater than `Height::MAX`.
///
/// # Consensus
///
/// There are multiple formats for serializing a height, so we don't implement
/// `ZcashSerialize` or `ZcashDeserialize` for `Height`.
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ChainHeight(pub u32);

/// Errors originating from ChainHeight calculations.
#[derive(Error, Debug)]
pub enum ChainHeightError {
    /// Height overflow error.
    #[error("The resulting height would overflow Height::MAX.")]
    Overflow,
    /// Height underflow error.
    #[error("The resulting height would underflow Height::MIN.")]
    Underflow,
}

impl std::str::FromStr for ChainHeight {
    type Err = SerializationError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.parse() {
            Ok(h) if (ChainHeight(h) <= ChainHeight::MAX) => Ok(ChainHeight(h)),
            Ok(_) => Err(SerializationError::Parse("Height exceeds maximum height")),
            Err(_) => Err(SerializationError::Parse("Height(u32) integer parse error")),
        }
    }
}

impl ChainHeight {
    /// The minimum [`Height`].
    ///
    /// Due to the underlying type, it is impossible to construct block heights
    /// less than [`Height::MIN`].
    ///
    /// Style note: Sometimes, [`Height::MIN`] is less readable than
    /// `Height(0)`. Use whichever makes sense in context.
    pub const MIN: ChainHeight = ChainHeight(0);

    /// The maximum [`Height`].
    ///
    /// Users should not construct block heights greater than [`Height::MAX`].
    ///
    /// The spec says *"Implementations MUST support block heights up to and
    /// including 2^31 − 1"*.
    ///
    /// Note that `u32::MAX / 2 == 2^31 - 1 == i32::MAX`.
    pub const MAX: ChainHeight = ChainHeight(u32::MAX / 2);

    /// The maximum [`Height`] as a [`u32`], for range patterns.
    ///
    /// `Height::MAX.0` can't be used in match range patterns, use this
    /// alias instead.
    pub const MAX_AS_U32: u32 = Self::MAX.0;

    /// The maximum expiration [`Height`] that is allowed in all transactions
    /// previous to Nu5 and in non-coinbase transactions from Nu5 activation
    /// height and above.
    pub const MAX_EXPIRY_HEIGHT: ChainHeight = ChainHeight(499_999_999);

    /// Returns the next [`Height`].
    ///
    /// # Panics
    ///
    /// - If the current height is at its maximum.
    pub fn next(self) -> Result<Self, ChainHeightError> {
        (self + 1).ok_or(ChainHeightError::Overflow)
    }

    /// Returns the previous [`Height`].
    ///
    /// # Panics
    ///
    /// - If the current height is at its minimum.
    pub fn previous(self) -> Result<Self, ChainHeightError> {
        (self - 1).ok_or(ChainHeightError::Underflow)
    }

    /// Returns `true` if the [`Height`] is at its minimum.
    pub fn is_min(self) -> bool {
        self == Self::MIN
    }

    /// Returns the value as a `usize`.
    pub fn as_usize(self) -> usize {
        self.0.try_into().expect("fits in usize")
    }
}

impl From<ChainHeight> for BlockHeight {
    fn from(height: ChainHeight) -> Self {
        BlockHeight::from_u32(height.0)
    }
}

impl TryFrom<BlockHeight> for ChainHeight {
    type Error = &'static str;

    /// Checks that the `height` is within the valid [`Height`] range.
    fn try_from(height: BlockHeight) -> Result<Self, Self::Error> {
        Self::try_from(u32::from(height))
    }
}

/// A difference between two [`Height`]s, possibly negative.
///
/// This can represent the difference between any height values,
/// even if they are outside the valid height range (for example, in buggy RPC code).
pub type HeightDiff = i64;

// We don't implement TryFrom<u64>, because it causes type inference issues for integer constants.
// Instead, use 1u64.try_into_height().

impl TryFrom<u32> for ChainHeight {
    type Error = &'static str;

    /// Checks that the `height` is within the valid [`Height`] range.
    fn try_from(height: u32) -> Result<Self, Self::Error> {
        // Check the bounds.
        //
        // Clippy warns that `height >= Height::MIN.0` is always true.
        assert_eq!(ChainHeight::MIN.0, 0);

        if height <= ChainHeight::MAX.0 {
            Ok(ChainHeight(height))
        } else {
            Err("heights must be less than or equal to Height::MAX")
        }
    }
}

/// Convenience trait for converting a type into a valid Zcash [`Height`].
pub trait TryIntoHeight {
    /// The error type returned by [`Height`] conversion failures.
    type Error;

    /// Convert `self` to a `Height`, if possible.
    fn try_into_height(&self) -> Result<ChainHeight, Self::Error>;
}

impl TryIntoHeight for u64 {
    type Error = BoxError;

    fn try_into_height(&self) -> Result<ChainHeight, Self::Error> {
        u32::try_from(*self)?.try_into().map_err(Into::into)
    }
}

impl TryIntoHeight for usize {
    type Error = BoxError;

    fn try_into_height(&self) -> Result<ChainHeight, Self::Error> {
        u32::try_from(*self)?.try_into().map_err(Into::into)
    }
}

impl TryIntoHeight for str {
    type Error = BoxError;

    fn try_into_height(&self) -> Result<ChainHeight, Self::Error> {
        self.parse().map_err(Into::into)
    }
}

impl TryIntoHeight for String {
    type Error = BoxError;

    fn try_into_height(&self) -> Result<ChainHeight, Self::Error> {
        self.as_str().try_into_height()
    }
}

impl TryIntoHeight for i32 {
    type Error = BoxError;

    fn try_into_height(&self) -> Result<ChainHeight, Self::Error> {
        u32::try_from(*self)?.try_into().map_err(Into::into)
    }
}

// We don't implement Add<u32> or Sub<u32>, because they cause type inference issues for integer constants.

impl Sub<ChainHeight> for ChainHeight {
    type Output = HeightDiff;

    /// Subtract two heights, returning the result, which can be negative.
    /// Since [`HeightDiff`] is `i64` and [`Height`] is `u32`, the result is always correct.
    fn sub(self, rhs: ChainHeight) -> Self::Output {
        // All these conversions are exact, and the subtraction can't overflow or underflow.
        let lhs = HeightDiff::from(self.0);
        let rhs = HeightDiff::from(rhs.0);

        lhs - rhs
    }
}

impl Sub<HeightDiff> for ChainHeight {
    type Output = Option<Self>;

    /// Subtract a height difference from a height, returning `None` if the resulting height is
    /// outside the valid `Height` range (this also checks the result is non-negative).
    fn sub(self, rhs: HeightDiff) -> Option<Self> {
        // We need to convert the height to [`i64`] so we can subtract negative [`HeightDiff`]s.
        let lhs = HeightDiff::from(self.0);
        let res = lhs - rhs;

        // Check the bounds.
        let res = u32::try_from(res).ok()?;
        ChainHeight::try_from(res).ok()
    }
}

impl Add<HeightDiff> for ChainHeight {
    type Output = Option<ChainHeight>;

    /// Add a height difference to a height, returning `None` if the resulting height is outside
    /// the valid `Height` range (this also checks the result is non-negative).
    fn add(self, rhs: HeightDiff) -> Option<ChainHeight> {
        // We need to convert the height to [`i64`] so we can add negative [`HeightDiff`]s.
        let lhs = i64::from(self.0);
        let res = lhs + rhs;

        // Check the bounds.
        let res = u32::try_from(res).ok()?;
        ChainHeight::try_from(res).ok()
    }
}
