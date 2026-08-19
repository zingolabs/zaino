//! Value types. One module per type; invariants enforced at construction.

mod about;

pub use about::AboutReport;

// Re-exported so downstream crates get a single, consistent chrono surface.
pub use chrono::{DateTime, Utc};
