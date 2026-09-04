use std::sync::Arc;

use relman_core::ports::{About, Clock};
use relman_core::types::AboutReport;

/// Answers the `about` query. Depends on the [`Clock`] driven port so the
/// reported instant is deterministic under test — the same seam relman uses
/// to stamp `## [x.y.z] - YYYY-MM-DD` changelog headers with "today".
pub struct AboutService {
    clock: Arc<dyn Clock>,
}

impl AboutService {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self { clock }
    }
}

impl About for AboutService {
    fn report(&self) -> AboutReport {
        AboutReport {
            version: env!("CARGO_PKG_VERSION"),
            now: self.clock.now(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use relman_core::mocks::FixedClock;
    use relman_core::mocks::fixtures::instant;

    #[test]
    fn report_carries_injected_clock() {
        let clock = Arc::new(FixedClock::new(instant()));
        let svc = AboutService::new(clock);

        let report = svc.report();

        assert_eq!(report.now, instant());
        assert_eq!(report.version, env!("CARGO_PKG_VERSION"));
    }
}
