//! Scope timer recording into a histogram.

/// Records construction → drop into `histogram`.
///
/// - Drop, not a tail statement: covers `?`, cancellation and panics (a slow failing read = the
///   symptom worth keeping)
pub(crate) struct Timer {
    histogram: metrics::Histogram,
    started: std::time::Instant,
}

impl Timer {
    pub(crate) fn start(histogram: metrics::Histogram) -> Self {
        Self {
            histogram,
            started: std::time::Instant::now(),
        }
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        self.histogram.record(self.started.elapsed().as_secs_f64());
    }
}
