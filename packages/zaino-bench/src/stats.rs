//! Summary statistics over a sample of durations.

/// Min / mean / max plus the tail percentiles.
///
/// Under load the mean understates what a client actually experiences, so every
/// latency line the harness prints carries p50/p95/p99 alongside it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Summary {
    pub(crate) min: f64,
    pub(crate) mean: f64,
    pub(crate) max: f64,
    pub(crate) p50: f64,
    pub(crate) p95: f64,
    pub(crate) p99: f64,
}

impl Summary {
    /// Summarises `samples`, or returns `None` when there is nothing to
    /// summarise (every connection failed, so there are no timings to report).
    pub(crate) fn new(samples: &[f64]) -> Option<Self> {
        if samples.is_empty() {
            return None;
        }

        let mut sorted = samples.to_vec();
        sorted.sort_by(f64::total_cmp);

        Some(Self {
            min: sorted[0],
            mean: sorted.iter().sum::<f64>() / sorted.len() as f64,
            max: sorted[sorted.len() - 1],
            p50: percentile(&sorted, 0.50),
            p95: percentile(&sorted, 0.95),
            p99: percentile(&sorted, 0.99),
        })
    }

    /// One aligned line: `<label> min .. mean .. max .. p50 .. p95 .. p99`.
    pub(crate) fn line(&self, label: &str) -> String {
        format!(
            "  {label:<26} min {:>8.3}  mean {:>8.3}  max {:>8.3}  \
             p50 {:>8.3}  p95 {:>8.3}  p99 {:>8.3}",
            self.min, self.mean, self.max, self.p50, self.p95, self.p99
        )
    }
}

/// Nearest-rank percentile over an already-sorted slice.
///
/// Nearest-rank (rather than an interpolating definition) so every reported
/// percentile is a timing some connection actually observed.
fn percentile(sorted: &[f64], fraction: f64) -> f64 {
    debug_assert!(!sorted.is_empty(), "percentile of an empty sample");
    let rank = (fraction * sorted.len() as f64).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;

    const ONE_TO_TEN: [f64; 10] = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0];

    #[test]
    fn summarises_a_known_sample() {
        let summary = Summary::new(&ONE_TO_TEN).expect("non-empty sample");
        assert_eq!(summary.min, 1.0);
        assert_eq!(summary.max, 10.0);
        assert_eq!(summary.mean, 5.5);
        assert_eq!(summary.p50, 5.0);
        assert_eq!(summary.p95, 10.0);
        assert_eq!(summary.p99, 10.0);
    }

    #[test]
    fn sorts_before_summarising() {
        let mut shuffled = ONE_TO_TEN;
        shuffled.reverse();
        assert_eq!(Summary::new(&shuffled), Summary::new(&ONE_TO_TEN));
    }

    #[test]
    fn a_single_sample_is_every_percentile() {
        let summary = Summary::new(&[4.0]).expect("non-empty sample");
        assert_eq!(summary.min, 4.0);
        assert_eq!(summary.max, 4.0);
        assert_eq!(summary.p99, 4.0);
    }

    #[test]
    fn an_empty_sample_has_no_summary() {
        assert!(Summary::new(&[]).is_none());
    }
}
