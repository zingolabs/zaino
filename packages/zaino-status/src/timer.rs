//! Scope timer whose histogram handle is resolved once per call site.

/// Records construction → drop into a histogram. Build one with [`timed!`](crate::timed).
///
/// - Drop, not a tail statement: covers `?`, `await` cancellation and panics, and a
///   read that fails slowly is the symptom worth keeping
pub struct Timer {
    histogram: &'static metrics::Histogram,
    started: std::time::Instant,
}

impl Timer {
    #[doc(hidden)]
    pub fn new(histogram: &'static metrics::Histogram) -> Self {
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

/// Times the enclosing scope, resolving the histogram once per call site.
///
/// ```ignore
/// let _timer = timed!(DB_READ_SECONDS, "op" => "block_hash");
/// ```
///
/// - `histogram!()` builds a `Key`, hashes the name and read-locks a registry shard on
///   every call, and a labelled one allocates a `Vec<Label>` too; the `OnceLock` pays
///   that once per site, leaving a load and the record
/// - Name and label values are `path`/`literal` by construction: one cached handle is
///   one series, so a runtime value on either axis would pin whichever arrived first
/// - Resolves against whatever recorder is installed when the site is *first* reached,
///   so install the recorder before serving starts (`zainod::run` does)
#[macro_export]
macro_rules! timed {
    ($name:path $(, $key:literal => $value:literal)* $(,)?) => {{
        static HISTOGRAM: ::std::sync::OnceLock<$crate::metrics::Histogram> =
            ::std::sync::OnceLock::new();
        $crate::Timer::new(
            HISTOGRAM.get_or_init(|| $crate::metrics::histogram!($name $(, $key => $value)*)),
        )
    }};
}

#[cfg(test)]
mod tests {
    use metrics_exporter_prometheus::PrometheusBuilder;

    const READ_SECONDS: &str = "zaino.test.read_seconds";

    /// - Each expansion owns its `OnceLock`, so two sites must not share a series —
    ///   the whole point of caching per site rather than per metric
    /// - Drop is what records, so the timer must be dropped before the render
    #[test]
    fn each_call_site_records_to_its_own_labelled_series() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            {
                let _read = crate::timed!(READ_SECONDS, "op" => "compact_chunk");
            }
            {
                let _read = crate::timed!(READ_SECONDS, "op" => "block_hash");
            }
        });

        let scrape = handle.render();
        for op in ["compact_chunk", "block_hash"] {
            assert!(
                scrape.contains(&format!("zaino_test_read_seconds_count{{op=\"{op}\"}} 1")),
                "`op={op}` did not get its own series with exactly one sample. \
                 Scrape was:\n{scrape}"
            );
        }
    }
}
