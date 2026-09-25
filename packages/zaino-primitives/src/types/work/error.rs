//! Errors raised by more than one work quantity.

/// Error when accumulating a block's work overflows the recorded width.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("accumulating a block's work overflowed the recorded width")]
pub struct WorkOverflow;
