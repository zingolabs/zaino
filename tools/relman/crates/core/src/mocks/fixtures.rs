//! Small constructors for use in tests, keeping test bodies terse.

use crate::types::{DateTime, Utc};

/// A fixed, arbitrary instant (2023-11-14T22:13:20Z) for deterministic tests.
pub fn instant() -> DateTime<Utc> {
    DateTime::from_timestamp(1_700_000_000, 0).expect("valid timestamp")
}
