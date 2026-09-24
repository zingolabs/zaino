//! Errors raised by more than one work quantity.

/// Error when accumulating a block's work overflows the recorded width.
///
/// Raised by both folds: [`AbsoluteChainWork::accumulate`] and
/// [`RelativeChainWork::accumulate`].
///
/// [`AbsoluteChainWork::accumulate`]: super::AbsoluteChainWork::accumulate
/// [`RelativeChainWork::accumulate`]: super::RelativeChainWork::accumulate
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("accumulating a block's work overflowed the recorded width")]
pub struct WorkOverflow;
