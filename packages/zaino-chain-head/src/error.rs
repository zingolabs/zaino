//! What can go wrong asking ChainHead a question.

use zaino_primitives::types::Height;

/// A ChainHead query could not be answered.
///
/// One variant, because one question can be asked wrongly. Every other query on
/// [`ChainHeadSnapshot`](crate::ChainHeadSnapshot) is a total function of a
/// graph the caller is already holding: a hash that is not retained, a height
/// outside the window and a transaction that appears nowhere are all *absence*,
/// reported as `None` or an empty collection, not failure.
///
/// `#[non_exhaustive]` because a snapshot implementation that does not hold its
/// whole graph in memory — one paging from a store, say — could fail to answer
/// where this one cannot, and should be able to say so without a breaking
/// change. Variants are added when such an implementation exists, not in
/// anticipation of one.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ChainHeadError {
    /// A range whose start is above its end.
    #[error("range start {start} is above range end {end}")]
    InvalidRange {
        /// The requested start.
        start: Height,
        /// The requested end.
        end: Height,
    },
}
