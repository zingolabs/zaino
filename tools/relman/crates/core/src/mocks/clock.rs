use crate::ports::Clock;
use crate::types::{DateTime, Utc};

/// A [`Clock`] that always returns the same instant. Makes time-dependent
/// service logic deterministic under test.
pub struct FixedClock {
    at: DateTime<Utc>,
}

impl FixedClock {
    pub fn new(at: DateTime<Utc>) -> Self {
        Self { at }
    }
}

impl Clock for FixedClock {
    fn now(&self) -> DateTime<Utc> {
        self.at
    }
}
