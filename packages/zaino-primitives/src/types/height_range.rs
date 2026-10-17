//! An inclusive span of heights.

use crate::types::Height;

/// `[start, end]` — an inclusive height range.
///
/// Both bounds are heights on the chain, so a range never names a height
/// that cannot exist; whether the blocks in it are *held* is the concern of
/// whoever serves it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeightRange {
    /// The lowest height in the range.
    pub start: Height,
    /// The highest height in the range.
    pub end: Height,
}
