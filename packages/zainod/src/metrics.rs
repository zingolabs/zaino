//! Prometheus recorder + `/metrics` scrape listener

use std::net::SocketAddr;

use metrics_exporter_prometheus::{Matcher, PrometheusBuilder};

// Names owned by the emitting crates, so `describe_*` cannot drift from emission
use zaino_chain_head_service::metric_names::*;
use zaino_rpc::metric_names::*;
use zaino_serve::metric_names::*;
use zaino_state::mempool_metric_names::*;
use zaino_state::metric_names::*;
use zaino_status::metric_names::*;

use crate::error::IndexerError;

const BUILD_INFO: &str = "zainod.build_info";

/// Supervisor restarts; named here (zainod = emitter & registrar)
const RESTARTS_TOTAL: &str = "zainod.restarts_total";

/// Per-block timings; sub-ms floor (0.4ms warm vs 5ms cold read must not share a bucket)
const PER_BLOCK_SECONDS: &[f64] = &[
    0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Serving, outbound calls, batch commit (whole-batch fsync → tens of seconds in range)
const COARSE_SECONDS: &[f64] = &[
    0.001, 0.005, 0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 300.0,
];

/// Accumulator rebuild, client-held streams
const LONG_SECONDS: &[f64] = &[
    0.01, 0.1, 1.0, 5.0, 15.0, 60.0, 300.0, 900.0, 1800.0, 3600.0, 10800.0, 43200.0,
];

/// Integer ladder, dense at small ints (1 = routine, past the NFS window = incident)
const REORG_DEPTHS: &[f64] = &[1.0, 2.0, 3.0, 4.0, 5.0, 7.0, 10.0, 20.0, 50.0, 100.0];

const BATCH_BLOCK_COUNTS: &[f64] = &[1.0, 2.0, 4.0, 8.0, 16.0, 32.0, 64.0, 128.0, 256.0, 512.0];

/// Bucket ladder per emitted histogram; the `# HELP` comes from the emitting crate
///
/// - `Matcher::Full`, never `Suffix`: overlapping matchers resolve lexicographically
/// - A histogram missing here would ship as a summary, so [`init`] refuses to start
const BUCKETS: &[(&str, &[f64])] = &[
    (SYNC_BLOCK_FETCH_SECONDS, PER_BLOCK_SECONDS),
    (SYNC_TREESTATE_FETCH_SECONDS, PER_BLOCK_SECONDS),
    (SYNC_BLOCK_ASSEMBLE_SECONDS, PER_BLOCK_SECONDS),
    (DB_READ_SECONDS, PER_BLOCK_SECONDS),
    (SYNC_BATCH_WRITE_SECONDS, COARSE_SECONDS),
    (SYNC_FSYNC_SECONDS, COARSE_SECONDS),
    (DB_VALIDATION_SECONDS, COARSE_SECONDS),
    (GRPC_REQUEST_DURATION_SECONDS, COARSE_SECONDS),
    (JSONRPC_REQUEST_DURATION_SECONDS, COARSE_SECONDS),
    (RPC_OUTBOUND_DURATION_SECONDS, COARSE_SECONDS),
    (MEMPOOL_POLL_SECONDS, COARSE_SECONDS),
    (SYNC_ACCUMULATOR_SECONDS, LONG_SECONDS),
    (CHAIN_HEAD_REORG_DEPTH, REORG_DEPTHS),
    (SYNC_BATCH_BLOCKS, BATCH_BLOCK_COUNTS),
];

/// Every counter the workspace emits, with its `# HELP`, from the crate that emits it
const COUNTERS: &[&[(&str, &str)]] = &[
    zaino_state::metric_names::store::COUNTERS,
    zaino_state::metric_names::COUNTERS,
    zaino_serve::metric_names::COUNTERS,
    zaino_rpc::metric_names::COUNTERS,
    zaino_chain_head_service::metric_names::COUNTERS,
    &[(
        RESTARTS_TOTAL,
        "Times the supervisor has restarted the indexer",
    )],
];

const GAUGES: &[&[(&str, &str)]] = &[
    zaino_state::metric_names::store::GAUGES,
    zaino_state::metric_names::GAUGES,
    zaino_state::mempool_metric_names::GAUGES,
    &[(
        BUILD_INFO,
        "Static build metadata; always 1, version in a label",
    )],
];

const HISTOGRAMS: &[&[(&str, &str)]] = &[
    zaino_state::metric_names::store::HISTOGRAMS,
    zaino_state::mempool_metric_names::HISTOGRAMS,
    zaino_serve::metric_names::HISTOGRAMS,
    zaino_rpc::metric_names::HISTOGRAMS,
    zaino_chain_head_service::metric_names::HISTOGRAMS,
];

/// Flatten one of the per-crate tables above
fn all(
    tables: &'static [&'static [(&'static str, &'static str)]],
) -> impl Iterator<Item = (&'static str, &'static str)> {
    tables.iter().flat_map(|table| table.iter().copied())
}

fn buckets(metric: &str) -> Option<&'static [f64]> {
    BUCKETS
        .iter()
        .find(|(name, _)| *name == metric)
        .map(|(_, buckets)| *buckets)
}

/// - Call once, before any `metrics::*!()` (earlier calls silently no-op)
/// - Listener lives in [`crate::admin`]: only it wants a runtime of its own
pub fn init(endpoint: SocketAddr) -> Result<(), IndexerError> {
    let mut builder = PrometheusBuilder::new().with_http_listener(endpoint);
    for (metric, _) in all(HISTOGRAMS) {
        let buckets = buckets(metric).ok_or_else(|| {
            IndexerError::MetricsError(format!(
                "`{metric}` has no bucket ladder in BUCKETS, so it would scrape as a summary"
            ))
        })?;
        builder = builder
            .set_buckets_for_metric(Matcher::Full(metric.to_string()), buckets)
            .map_err(|e| {
                IndexerError::MetricsError(format!("bucket bounds for `{metric}`: {e}"))
            })?;
    }
    builder.install().map_err(|e| {
        IndexerError::MetricsError(format!("Failed to install metrics recorder: {e}"))
    })?;

    describe_metrics();
    initialise_counters();
    metrics::gauge!(BUILD_INFO, "version" => env!("CARGO_PKG_VERSION")).set(1.0);
    spawn_process_collector();

    tracing::info!(%endpoint, "Prometheus metrics endpoint started");
    Ok(())
}

/// - Recorder installed outside the supervisor loop → nothing else separates a
///   crash loop from a healthy run
pub fn record_restart() {
    metrics::counter!(RESTARTS_TOTAL).increment(1);
}

/// - Block time / process CPU separates CPU-bound from disk-bound from waiting
/// - Timer, not scrape-time: the exporter's listener has no scrape hook
fn spawn_process_collector() {
    let collector = metrics_process::Collector::default();
    collector.describe();

    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(10));
        loop {
            tick.tick().await;
            collector.collect();
        }
    });
}

fn describe_metrics() {
    for (metric, help) in all(HISTOGRAMS) {
        metrics::describe_histogram!(metric, help);
    }
    for (metric, help) in all(COUNTERS) {
        metrics::describe_counter!(metric, help);
    }
    for (metric, help) in all(GAUGES) {
        metrics::describe_gauge!(metric, help);
    }

    // Legend ships in the scrape; a dashboard-side copy of it drifts, silently
    for (metric, help, values) in LEGENDS {
        let legend: Vec<String> = values
            .iter()
            .enumerate()
            .map(|(discriminant, name)| format!("{discriminant}={name}"))
            .collect();
        metrics::describe_gauge!(*metric, format!("{help}: {}", legend.join(", ")));
    }
}

/// Gauges publishing a raw discriminant; the legend is what makes one readable
const LEGENDS: &[(&str, &str, &[&str])] = &[
    (STATUS, "Component state, by name", &STATUS_VALUES),
    (
        MEMPOOL_COMPLETENESS,
        "Published mempool set completeness",
        &MEMPOOL_COMPLETENESS_VALUES,
    ),
];

/// - Absent ≠ zero: an unseeded series reads as "this build does not report it"
/// - Per-block counters seed themselves when the store builds its cached handles
/// - No height gauges (0 = a false height), no `method` families ([`UNSEEDABLE_COUNTERS`])
fn initialise_counters() {
    zaino_state::seed_block_counters();

    for outcome in ["ok", "error"] {
        metrics::counter!(SYNC_ITERATIONS_TOTAL, SYNC_OUTCOME => outcome).increment(0);
    }
    for name in [
        DB_ON_DEMAND_VALIDATIONS_TOTAL,
        DB_CORRUPT_ROWS_TOTAL,
        CHAIN_HEAD_REORG_TOTAL,
        RESTARTS_TOTAL,
    ] {
        metrics::counter!(name).increment(0);
    }
}

/// Labels unknown until emission; a partial seed is a different series, not a placeholder
///
/// - `rate()` over an absent series yields no data, so alerts need `or vector(0)`
#[cfg(test)]
const UNSEEDABLE_COUNTERS: &[&str] = &[
    GRPC_ERRORS_TOTAL,
    JSONRPC_ERRORS_TOTAL,
    RPC_OUTBOUND_REQUESTS_TOTAL,
];

#[cfg(test)]
mod tests {
    use metrics_exporter_prometheus::PrometheusBuilder;

    use super::*;

    /// - Shares `init`'s bucket registration; its own ladder would pass while the
    ///   shipped binary rendered summaries
    fn scrape(body: impl FnOnce()) -> String {
        let mut builder = PrometheusBuilder::new();
        for (metric, ladder) in BUCKETS {
            builder = builder
                .set_buckets_for_metric(Matcher::Full((*metric).to_string()), ladder)
                .expect("bucket bounds are non-empty and finite");
        }
        let recorder = builder.build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            describe_metrics();
            initialise_counters();
            body();
        });
        handle.render()
    }

    /// - 0 = a false height, absent = honest until something measures one
    #[test]
    fn height_gauges_are_absent_until_measured() {
        let scrape = scrape(|| {});
        assert!(
            !scrape.contains("zaino_sync_finalized_height "),
            "a finalized height was published before any block was indexed, which \
             reads as a tip at genesis. Scrape was:\n{scrape}"
        );
    }

    /// - Unbucketed → the exporter renders a summary: rolling-window quantiles, not
    ///   aggregatable, and the series still appears
    #[test]
    fn every_histogram_scrapes_on_its_own_bucket_ladder() {
        let scrape = scrape(|| {
            for (metric, ladder) in BUCKETS {
                // In the first bucket → the `le` assert exercises the ladder, not +Inf
                metrics::histogram!(*metric).record(ladder[0]);
            }
        });

        for (metric, ladder) in BUCKETS {
            // Overlapping matchers resolve by lexicographic accident; `Matcher::Full` prevents it
            let lowest = format!(
                "{}_bucket{{le=\"{}\"}}",
                metric.replace('.', "_"),
                ladder[0]
            );
            assert!(
                scrape.contains(&lowest),
                "`{metric}` did not get its configured ladder; expected `{lowest}`. \
                 Scrape was:\n{scrape}"
            );
        }
    }

    /// - Missing ladder = a summary at runtime; stale ladder = a decision about a
    ///   histogram nothing emits. `init` refuses to start on the first, not the second
    #[test]
    fn every_emitted_histogram_has_a_bucket_ladder_and_no_ladder_is_stale() {
        let mut emitted: Vec<&str> = all(HISTOGRAMS).map(|(name, _)| name).collect();
        let mut laddered: Vec<&str> = BUCKETS.iter().map(|(name, _)| *name).collect();
        emitted.sort_unstable();
        laddered.sort_unstable();
        assert_eq!(
            laddered, emitted,
            "BUCKETS and the emitted histograms disagree"
        );
    }

    /// - Every counter is seeded or named unseedable, never neither
    #[test]
    fn every_counter_is_seeded_or_named_unseedable() {
        let scrape = scrape(|| {});
        for (metric, _) in all(COUNTERS) {
            // A sample line, not `contains`: `# HELP` carries the name too, so a
            // substring match reads an unsampled counter as seeded. Labelled series
            // render as `name{..} 0`, unlabelled as `name 0`
            let rendered = metric.replace('.', "_");
            let seeded = scrape.lines().any(|line| {
                line.starts_with(&format!("{rendered} "))
                    || line.starts_with(&format!("{rendered}{{"))
            });
            assert_eq!(
                seeded,
                !UNSEEDABLE_COUNTERS.contains(&metric),
                "`{metric}` is {} a fresh scrape but {} UNSEEDABLE_COUNTERS. \
                 Scrape was:\n{scrape}",
                if seeded { "in" } else { "absent from" },
                if UNSEEDABLE_COUNTERS.contains(&metric) {
                    "listed in"
                } else {
                    "absent from"
                },
            );
        }
    }

    /// - Gauges publish a raw int; the legend is the only thing making it readable
    /// - Variant added without extending the list = permanently unlabelled state
    #[test]
    fn discriminant_legends_are_rendered_into_help_text() {
        // `# HELP` is emitted only for sampled metrics, and neither is pre-created
        let scrape = scrape(|| {
            metrics::gauge!(STATUS, STATUS_COMPONENT => "test").set(0.0);
            metrics::gauge!(MEMPOOL_COMPLETENESS).set(0.0);
        });
        for (metric, _, values) in LEGENDS {
            let rendered = metric.replace('.', "_");
            let help = scrape
                .lines()
                .find(|line| line.starts_with(&format!("# HELP {rendered} ")))
                .unwrap_or_else(|| panic!("`{metric}` has no HELP line. Scrape was:\n{scrape}"));
            for (discriminant, name) in values.iter().enumerate() {
                assert!(
                    help.contains(&format!("{discriminant}={name}")),
                    "`{metric}` help omits `{discriminant}={name}`, so that state is \
                     unreadable in a dashboard. Help line was:\n{help}"
                );
            }
        }
    }
}
