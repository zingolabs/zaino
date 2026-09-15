//! Errors shared by the work folds.

/// Error when adding a block's work overflows the recorded width.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("adding a block's work overflowed the recorded width")]
pub struct WorkOverflow;

/// Error when unwinding a block's work reaches or crosses zero.
///
/// The result must stay strictly positive, because the chain still contains
/// genesis. Crossing that floor means the work being unwound was never part of
/// this total.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("unwinding a block's work would take the total to or below zero")]
pub struct WorkUnderflow;
