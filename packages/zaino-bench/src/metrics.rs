//! Scraping and parsing zainod's Prometheus endpoint.
//!
//! Measuring initial sync needs no new instrumentation: zainod already emits
//! everything required behind its `prometheus` feature. The harness reads that
//! endpoint from the outside, so a sync measurement never perturbs the run it is
//! measuring.

use std::collections::HashMap;
use std::time::Duration;

use crate::error::BenchError;

/// The metrics this harness reads, in the form `zaino-state` registers them.
///
/// Mirrors `zaino_state::metric_names`, which is `#[cfg(feature = "prometheus")]`
/// and would pull that feature into every workspace build if depended on
/// directly. The `metric_names_match_zaino_state` test below pins the two
/// together via a dev-dependency, so drift fails the build rather than the run.
pub(crate) mod names {
    pub(crate) const SYNC_FINALIZED_HEIGHT: &str = "zaino.sync.finalized_height";
    pub(crate) const SYNC_TARGET_HEIGHT: &str = "zaino.sync.target_height";
    pub(crate) const SYNC_LAG_BLOCKS: &str = "zaino.sync.lag_blocks";
    pub(crate) const SYNC_HAS_REACHED_TIP: &str = "zaino.sync.has_reached_tip";
    pub(crate) const SYNC_TRANSACTIONS_TOTAL: &str = "zaino.sync.transactions_total";
    pub(crate) const DB_TIP_HEIGHT: &str = "zaino.db.tip_height";
    pub(crate) const CHAIN_TIP_HEIGHT: &str = "zaino.chain.tip_height";
}

/// One scrape of the endpoint, parsed into unlabelled sample values.
#[derive(Debug, Default)]
pub(crate) struct Scrape {
    samples: HashMap<String, f64>,
}

impl Scrape {
    /// Reads a metric, or reports which one was missing.
    pub(crate) fn require(&self, name: &'static str) -> Result<f64, BenchError> {
        self.get(name).ok_or(BenchError::MissingMetric(name))
    }

    /// Reads a metric that may legitimately be absent — a gauge zainod has not
    /// set yet, such as `has_reached_tip` before the first sync iteration.
    pub(crate) fn get(&self, name: &str) -> Option<f64> {
        self.samples.get(&exposed_name(name)).copied()
    }

    /// Reads a height metric, rounded to the integer it represents.
    ///
    /// Heights cross the exposition format as `f64`; every value in play is far
    /// below 2^53, so the round-trip is exact.
    pub(crate) fn height(&self, name: &'static str) -> Result<u64, BenchError> {
        Ok(self.require(name)?.max(0.0) as u64)
    }

    /// Parses the Prometheus text exposition format.
    ///
    /// Only unlabelled samples are kept: every metric the harness reads is a
    /// process-wide gauge, and skipping labelled series avoids collapsing a
    /// per-method histogram into a single meaningless number.
    fn parse(body: &str) -> Self {
        let samples = body
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .filter_map(parse_sample)
            .collect();

        Self { samples }
    }
}

/// Polls `url` until it serves `required`, then returns that scrape.
///
/// Waiting rather than failing is deliberate: the operator starts the harness
/// *before* zainod so that t0 is the moment zainod begins, not the moment
/// someone got to a second terminal.
///
/// Readiness is the *metric*, not the endpoint. zainod binds its exporter
/// during startup, but the sync gauges are first set from inside the write
/// loop — after network adoption and the db open, and then only on a ten-second
/// throttle. So there is a window, unbounded on a cold start, where the endpoint
/// answers 200 with a body that does not mention `zaino.sync.*` yet. Returning
/// the first successful scrape lands in that window and fails the run before it
/// begins; waiting for the gauge is what "zainod has started syncing" actually
/// means.
pub(crate) async fn await_metric(
    client: &reqwest::Client,
    url: &str,
    required: &'static str,
    poll_interval: Duration,
) -> Scrape {
    loop {
        match scrape(client, url).await {
            Ok(scrape) if scrape.get(required).is_some() => return scrape,
            Ok(_) => {
                eprintln!("  {url} is up; waiting for `{required}` (zainod is still starting)");
                tokio::time::sleep(poll_interval).await;
            }
            Err(error) => {
                eprintln!("  waiting for {url}: {error}");
                tokio::time::sleep(poll_interval).await;
            }
        }
    }
}

/// Fetches and parses one scrape.
pub(crate) async fn scrape(client: &reqwest::Client, url: &str) -> Result<Scrape, BenchError> {
    let scrape_error = |source| BenchError::Scrape {
        url: url.to_string(),
        source,
    };

    let body = client
        .get(url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(scrape_error)?
        .text()
        .await
        .map_err(scrape_error)?;

    Ok(Scrape::parse(&body))
}

/// Translates a registered metric name into the name it is exposed under.
///
/// `metrics-exporter-prometheus` sanitises names to the Prometheus character
/// set, so the dotted name `zaino-state` registers reaches the wire as
/// `zaino_sync_finalized_height`.
fn exposed_name(registered: &str) -> String {
    registered.replace(['.', '-'], "_")
}

/// Splits `name value` / `name{labels} value`, keeping only unlabelled samples.
fn parse_sample(line: &str) -> Option<(String, f64)> {
    let (name, value) = line.rsplit_once(char::is_whitespace)?;
    let name = name.trim();
    if name.contains('{') {
        return None;
    }
    Some((name.to_string(), value.trim().parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
# HELP zaino_sync_finalized_height Highest finalised block written to Zaino's db.
# TYPE zaino_sync_finalized_height gauge
zaino_sync_finalized_height 3200000
# TYPE zaino_sync_target_height gauge
zaino_sync_target_height 3390744
zaino_sync_has_reached_tip 0
zaino_db_tip_height 3200000
zaino_chain_tip_height 3390744
# TYPE zaino_grpc_request_duration_seconds histogram
zaino_grpc_request_duration_seconds{method=\"get_block_range\",quantile=\"0.5\"} 0.012

";

    #[test]
    fn parses_unlabelled_gauges() {
        let scrape = Scrape::parse(SAMPLE);
        assert_eq!(
            scrape.height(names::SYNC_FINALIZED_HEIGHT).ok(),
            Some(3200000)
        );
        assert_eq!(scrape.height(names::SYNC_TARGET_HEIGHT).ok(), Some(3390744));
        assert_eq!(scrape.get(names::SYNC_HAS_REACHED_TIP), Some(0.0));
    }

    #[test]
    fn skips_labelled_series() {
        let scrape = Scrape::parse(SAMPLE);
        assert_eq!(scrape.get("zaino.grpc.request_duration_seconds"), None);
    }

    #[test]
    fn a_missing_metric_names_itself() {
        let error = Scrape::default()
            .require(names::SYNC_FINALIZED_HEIGHT)
            .expect_err("empty scrape has no metrics");
        assert!(
            error.to_string().contains(names::SYNC_FINALIZED_HEIGHT),
            "error should name the missing metric: {error}"
        );
    }

    /// The scrape zainod serves between binding its exporter and the write
    /// loop's first gauge set: a healthy 200 that does not carry `zaino.sync.*`
    /// yet. `await_metric` must keep waiting on this, not accept it — accepting
    /// it is what failed a run before it started.
    #[test]
    fn a_started_exporter_without_the_sync_gauges_is_not_ready() {
        const STARTING_UP: &str = "\
# TYPE zaino_grpc_request_duration_seconds histogram
zaino_grpc_request_duration_seconds{method=\"get_block_range\",quantile=\"0.5\"} 0.012
";
        let scrape = Scrape::parse(STARTING_UP);
        assert!(
            scrape.get(names::SYNC_FINALIZED_HEIGHT).is_none(),
            "the readiness predicate must treat this scrape as not-yet-ready"
        );
        assert!(
            Scrape::parse(SAMPLE)
                .get(names::SYNC_FINALIZED_HEIGHT)
                .is_some(),
            "and must treat a scrape carrying the gauge as ready"
        );
    }

    #[test]
    fn dotted_names_reach_the_wire_underscored() {
        assert_eq!(
            exposed_name(names::SYNC_FINALIZED_HEIGHT),
            "zaino_sync_finalized_height"
        );
    }

    /// Pins this crate's copy of the metric names to the ones `zaino-state`
    /// actually emits, so a rename there fails the build rather than silently
    /// producing a harness that waits forever for a metric nobody publishes.
    ///
    /// Pins names, not emission — `lag_blocks`, `has_reached_tip` and `db_tip_height` were
    /// retired upstream, and `Sample` still reads all three into fields that can only be
    /// `None`. Removing those fields is the reconciliation this test cannot express
    #[test]
    fn metric_names_match_zaino_state() {
        use zaino_state::metric_names as upstream;

        assert_eq!(
            names::SYNC_FINALIZED_HEIGHT,
            upstream::SYNC_FINALIZED_HEIGHT
        );
        assert_eq!(names::SYNC_TARGET_HEIGHT, upstream::SYNC_TARGET_HEIGHT);
        assert_eq!(
            names::SYNC_TRANSACTIONS_TOTAL,
            upstream::SYNC_TRANSACTIONS_TOTAL
        );
        assert_eq!(names::CHAIN_TIP_HEIGHT, upstream::CHAIN_TIP_HEIGHT);
    }
}
