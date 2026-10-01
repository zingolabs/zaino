//! Pre-composed index sets.
//!
//! Each module defines a set-wide context, the `ProvideContext`
//! projections for its indexes, and the set itself as a type through
//! [`index_set!`](macro@crate::index_set).

pub mod compact_blocks;
pub mod current_zaino;
pub mod transparent_history;
