use crate::types::{DateTime, Utc};

/// Outbound port: the current time.
///
/// The domain depends on this rather than calling `Utc::now()` directly, so
/// services stay deterministic under test (see the `FixedClock` mock). The
/// binary wires a real system-clock adapter.
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}
