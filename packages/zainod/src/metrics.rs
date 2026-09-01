//! Prometheus recorder + `/metrics` scrape listener.

use std::net::SocketAddr;

use metrics_exporter_prometheus::{Matcher, PrometheusBuilder};
use tracing::info;

// Metric names are owned by the crates that emit them, so the `describe_*`
// registrations below share one source of truth with the emit sites and can
// never drift.
use zaino_chain_head_service::metric_names::*;
use zaino_rpc::metric_names::*;
use zaino_serve::metric_names::*;
use zaino_state::mempool_metric_names::*;
use zaino_state::metric_names::*;
use zaino_status::metric_names::*;

use crate::error::IndexerError;

const BUILD_INFO: &str = "zainod.build_info";

/// Supervisor restart counter.
///
/// - Named here, not in `metric_names` (zainod = both emitter & registrar)
const RESTARTS_TOTAL: &str = "zainod.restarts_total";

/// Per-block timings: block read, treestate read, assembly.
///
/// - Finer floor than the shared ladder (sub-ms at bulk-sync block rates)
/// - 0.4ms vs 5ms source read = warm vs cold, must not share a bucket
const PER_BLOCK_SECONDS: &[f64] = &[
    0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Coarse timings: inbound serving, outbound calls, batch commit.
///
/// - Batch commit spans a whole-batch fsync (tens of seconds is in range)
const COARSE_SECONDS: &[f64] = &[
    0.001, 0.005, 0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 300.0,
];

/// Minutes-to-hours operations: accumulator rebuild, client-held server streams.
const LONG_SECONDS: &[f64] = &[
    0.01, 0.1, 1.0, 5.0, 15.0, 60.0, 300.0, 900.0, 1800.0, 3600.0, 10800.0, 43200.0,
];

/// Reorg depth, integer ladder (not a duration → not a seconds ladder).
///
/// - Dense at small ints: 1 = routine, 3 = look, past the NFS window = incident
const REORG_DEPTHS: &[f64] = &[1.0, 2.0, 3.0, 4.0, 5.0, 7.0, 10.0, 20.0, 50.0, 100.0];

const BATCH_BLOCK_COUNTS: &[f64] = &[1.0, 2.0, 4.0, 8.0, 16.0, 32.0, 64.0, 128.0, 256.0, 512.0];

/// Every emitted histogram: name, `# HELP`, bucket bounds.
///
/// - One table drives both; described-but-unbucketed = a silent summary, which is
///   how `reorg_depth` shipped
/// - `Matcher::Full`, never `Suffix`: overlapping matchers sort lexicographically
///   and the first wins, so `_seconds` beat `_treestate_fetch_seconds`
/// - Keyed by constant → a rename loses buckets at compile time
const HISTOGRAMS: &[(&str, &str, &[f64])] = &[
    (
        SYNC_BLOCK_FETCH_SECONDS,
        "Seconds from requesting one block to holding it deserialized in memory, by ingest stage. \
         Under the rpc ingest path that is a validator round trip plus decode; under direct it is \
         a RocksDB read plus zebra's in-process deserialization, which is CPU spent inside zaino — \
         so a high value is not by itself an upstream problem. Reads that returned no block are \
         counted in fetch_misses_total instead of timed here",
        PER_BLOCK_SECONDS,
    ),
    (
        SYNC_TREESTATE_FETCH_SECONDS,
        "Seconds for the commitment-tree-root read each block also costs, by ingest stage. \
         A second source round trip, timed apart so one going slow cannot hide behind the other",
        PER_BLOCK_SECONDS,
    ),
    (
        SYNC_BLOCK_ASSEMBLE_SECONDS,
        "Seconds of zaino's own work turning a fetched block into an indexed one, by ingest \
         stage: conversion, pool-root resolution and metadata. Disjoint from block_fetch_seconds \
         and treestate_fetch_seconds — the three are the whole per-block cost and sum to it, and \
         none is derived by subtracting another. Runs only on a successfully fetched block, so \
         its count tracks the fetch count and trails it by the assemblies that errored",
        PER_BLOCK_SECONDS,
    ),
    (
        SYNC_BATCH_WRITE_SECONDS,
        "Seconds to write and sort one batch of blocks into the B-tree, excluding the fsync. \
         Tracks whether the working set still fits in RAM",
        COARSE_SECONDS,
    ),
    (
        SYNC_FSYNC_SECONDS,
        "Seconds spent in the LMDB checkpoint fsync after a batch write. Tracks the storage \
         device, and is split from batch_write_seconds because the two saturate independently",
        COARSE_SECONDS,
    ),
    (
        SYNC_BATCH_BLOCKS,
        "Blocks in one committed write batch; divides batch_write_seconds into a per-block cost",
        BATCH_BLOCK_COUNTS,
    ),
    (
        SYNC_ACCUMULATOR_SECONDS,
        "Seconds spent bringing the txout-set accumulator to the tip, by mode: `delta` applies \
         O(range) work, `rebuild` is a full from-genesis scan, `current` did nothing. Runs at the \
         end of every sync pass and is the most likely cause of a sync that appears stalled",
        LONG_SECONDS,
    ),
    (
        CHAIN_HEAD_REORG_DEPTH,
        "Depth of chain reorganizations in blocks (0 for same-height reorgs); \
         its _count is the total reorg event count",
        REORG_DEPTHS,
    ),
    (
        DB_VALIDATION_SECONDS,
        "Seconds to structurally re-validate one stored block. Excludes the already-validated \
         fast path, so every observation is real work",
        COARSE_SECONDS,
    ),
    (
        GRPC_REQUEST_DURATION_SECONDS,
        "Duration of inbound gRPC requests by method; its _count is the request volume. \
         Streaming methods time stream setup only — see grpc stream_seconds for their real cost",
        COARSE_SECONDS,
    ),
    (
        GRPC_STREAM_SECONDS,
        "Full lifetime of an inbound gRPC server stream by method, from setup until the last \
         item or the client hanging up",
        LONG_SECONDS,
    ),
    (
        JSONRPC_REQUEST_DURATION_SECONDS,
        "Duration of inbound JSON-RPC requests by method; its _count is the request volume",
        COARSE_SECONDS,
    ),
    (
        RPC_OUTBOUND_DURATION_SECONDS,
        "Duration of one outbound JSON-RPC attempt that received a response, by method. Timed \
         per attempt, so retry sleeps are excluded; attempts that failed at the transport are \
         counted in requests_total rather than timed here",
        COARSE_SECONDS,
    ),
    (
        MEMPOOL_POLL_SECONDS,
        "Duration of one mempool poll; its _count is the poll rate, the mempool writer's heartbeat",
        COARSE_SECONDS,
    ),
];

/// Install the recorder + spawn the HTTP listener.
///
/// - Call once, before any `metrics::*!()` (earlier calls silently no-op)
pub fn init(endpoint: SocketAddr) -> Result<(), IndexerError> {
    let mut builder = PrometheusBuilder::new().with_http_listener(endpoint);
    for (metric, _, buckets) in HISTOGRAMS {
        builder = builder
            .set_buckets_for_metric(Matcher::Full((*metric).to_string()), buckets)
            .map_err(|e| {
                IndexerError::MetricsError(format!(
                    "Failed to set histogram buckets for `{metric}`: {e}"
                ))
            })?;
    }
    builder.install().map_err(|e| {
        IndexerError::MetricsError(format!("Failed to install metrics recorder: {e}"))
    })?;

    describe_metrics();
    initialise_counters();
    set_build_info();
    spawn_process_collector();

    info!(%endpoint, "Prometheus metrics endpoint started");
    Ok(())
}

/// Count one supervisor restart.
///
/// - Restart re-seeds every gauge & resets every counter → otherwise no trace
pub fn record_restart() {
    metrics::counter!(RESTARTS_TOTAL).increment(1);
}

/// Process CPU / RSS / fds / threads, on a timer.
///
/// - CPU-bound vs disk-bound vs waiting-on-validator all move the same latency
///   histograms; block time / process CPU separates them
/// - Timer not scrape-time (exporter listener has no scrape hook); 10s << any
///   useful rate window
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

/// `# HELP` lines for every metric.
///
/// - Histogram help comes from [`HISTOGRAMS`] → cannot describe without bucketing
fn describe_metrics() {
    for (metric, help, _) in HISTOGRAMS {
        metrics::describe_histogram!(*metric, *help);
    }

    // Liveness
    metrics::describe_gauge!(
        STATUS,
        format!(
            "Current state of each component, as a discriminant: {}. Emitted for every component \
             that reports a status, labelled by name",
            enumerate(&STATUS_VALUES)
        )
    );
    metrics::describe_counter!(
        SYNC_ITERATIONS_TOTAL,
        "Total sync-worker iterations by outcome; its rate is the worker's heartbeat. \
         A flat rate here means wedged, whatever the throughput counters say"
    );
    metrics::describe_counter!(
        CHAIN_HEAD_REORG_TOTAL,
        "Total chain reorganization events observed by the chain head"
    );
    metrics::describe_histogram!(
        CHAIN_HEAD_REORG_DEPTH,
        "Depth of chain reorganizations in blocks (0 for same-height reorgs)"
    );
    metrics::describe_gauge!(
        SYNC_CONSECUTIVE_FAILURES,
        "Consecutive failed sync iterations; 0 when healthy. Distinguishes a transient blip \
         from a sustained outage the worker is backing off through"
    );
    metrics::describe_gauge!(
        SYNC_BACKOFF_SECONDS,
        "Current sync-loop retry backoff in seconds; 0 when healthy"
    );
    metrics::describe_counter!(
        RESTARTS_TOTAL,
        "Times the supervisor has restarted the indexer. Every gauge is re-seeded and every \
         counter resets across a restart, so without this a crash loop is invisible"
    );
    metrics::describe_gauge!(
        FINALISED_EPHEMERAL,
        "1 while finalised-state reads are served by the ephemeral passthrough rather than the \
         persistent database (initial sync, or a migration in progress); 0 once the on-disk index \
         is serving. Note this reads 1 for the whole life of a process configured with \
         ephemeral_finalised_state = true"
    );
    metrics::describe_gauge!(
        ACCUMULATOR_BUILT_HEIGHT,
        "Height the persisted txout-set accumulator currently reflects. Lagging far behind the DB \
         tip means the next sync will trigger a full from-genesis rebuild"
    );
    metrics::describe_gauge!(
        ACCUMULATOR_REBUILD_ACTIVE,
        "1 while a from-genesis txout-set accumulator rebuild is running. This is a multi-pass \
         full-chain scan; expect elevated read I/O for its duration"
    );

    // Progress, per frontier. lag = chain_tip - finalized (consumer-derived,
    // not exported)
    metrics::describe_gauge!(
        CHAIN_TIP_HEIGHT,
        "Latest chain tip height reported by the source"
    );
    metrics::describe_gauge!(
        SYNC_FINALIZED_HEIGHT,
        "Height the finalized index is committed to: written and fsynced, so a crash cannot lose \
         it. Republished on every sync pass, including passes that do no work, so it is present \
         at startup and while caught up"
    );
    metrics::describe_gauge!(
        SYNC_FETCHED_HEIGHT,
        "Height the sync loop has built to in memory; advances per block, ahead of the next commit"
    );
    metrics::describe_gauge!(
        SYNC_TARGET_HEIGHT,
        "Height the write path is working towards: chain tip minus the non-finalised reorg buffer. \
         Sync is complete against this, not against the raw chain tip"
    );
    metrics::describe_gauge!(
        DB_VALIDATED_HEIGHT,
        "Height the finalized index is structurally validated to. Behind finalized_height means \
         reads above it pay a synchronous re-validation on the serving path"
    );
    metrics::describe_gauge!(
        SYNC_ACCUMULATOR_HEIGHT,
        "Height the txout-set accumulator is built to; gettxoutsetinfo is only correct up to it"
    );

    // Read routing: index or proxy
    metrics::describe_gauge!(
        ROUTER_EPHEMERAL_MODE,
        "How much of the finalised-state service is being served straight from the backing \
         source instead of the persistent database: 0 none, 1 read-only (long-running sync), \
         2 full (migration; writes frozen too)"
    );
    metrics::describe_counter!(
        FINALISED_ROUTED_TOTAL,
        "Routed finalised-state capability resolutions by capability and backend \
         (primary / ephemeral / unavailable). The only thing that distinguishes serving from \
         the local index from proxying the validator. Counts write capabilities as well as \
         reads — filter on `capability` for one surface"
    );
    metrics::describe_gauge!(
        MIGRATION_ACTIVE,
        "1 while a database migration holds full ephemeral routing, 0 otherwise"
    );
    metrics::describe_gauge!(
        MIGRATION_PROGRESS_HEIGHT,
        "Height an in-progress migration backfill has reached; resumable across restarts"
    );

    // Health
    metrics::describe_counter!(
        SYNC_FETCH_MISSES_TOTAL,
        "Source reads that produced no block, by ingest stage, read kind, and outcome \
         (miss / error). A miss at the tip is the non-finalised loop's normal terminator, so \
         this doubles as the poll-rate-versus-block-rate ratio; errors are an incident"
    );
    metrics::describe_counter!(
        DB_ON_DEMAND_VALIDATIONS_TOTAL,
        "Reads that had to re-validate a block synchronously because it sat above \
         validated_height. Charged to whichever client happened to ask"
    );
    metrics::describe_counter!(
        SYNC_BATCH_FLUSH_TOTAL,
        "Write batches by what ended them: bytes / blocks / interval cap, or `target` meaning \
         the writer caught up. Says whether sync_write_batch_size is tuned for the position \
         in the chain the writer is at"
    );

    // Throughput per op class. rate() = ops/sec, all `stage`-labelled
    // (see `BlockWork` in zaino-state `chain_index::ingest`)
    metrics::describe_counter!(
        SYNC_TRANSACTIONS_TOTAL,
        "Total transactions ingested, by ingest stage"
    );
    metrics::describe_counter!(
        SYNC_TRANSPARENT_INPUTS_TOTAL,
        "Total transparent inputs ingested, by ingest stage. Each resolves a prior \
         outpoint and writes the spent index"
    );
    metrics::describe_counter!(
        SYNC_TRANSPARENT_OUTPUTS_TOTAL,
        "Total transparent outputs ingested, by ingest stage"
    );
    metrics::describe_counter!(
        SYNC_SAPLING_SPENDS_TOTAL,
        "Total Sapling spends ingested, by ingest stage"
    );
    metrics::describe_counter!(
        SYNC_SAPLING_OUTPUTS_TOTAL,
        "Total Sapling outputs ingested, by ingest stage. Kept apart from spends \
         because this is the half a consumer can check against the note-commitment \
         tree's growth over the same range"
    );
    metrics::describe_counter!(
        SYNC_ORCHARD_ACTIONS_TOTAL,
        "Total Orchard actions ingested, by ingest stage"
    );
    metrics::describe_counter!(
        SYNC_IRONWOOD_ACTIONS_TOTAL,
        "Total Ironwood actions ingested, by ingest stage"
    );

    // Storage. used_bytes vs host RAM = write-throughput knee, vs map_size = how
    // full. No LMDB reader slots (`mdb_env_info` = raw FFI, crate forbids unsafe)
    metrics::describe_gauge!(
        DB_USED_BYTES,
        "Bytes in use by the LMDB environment, sampled on a background timer so it stays \
         fresh while the writer is idle"
    );
    metrics::describe_gauge!(DB_MAP_SIZE_BYTES, "Bytes the LMDB map is sized to");

    // Inbound serving
    metrics::describe_counter!(
        GRPC_ERRORS_TOTAL,
        "Total inbound gRPC errors by method and status code"
    );
    metrics::describe_counter!(
        GRPC_STREAM_ITEMS_TOTAL,
        "Items delivered over inbound gRPC server streams by method. Counted on delivery, so \
         a stream the client abandoned is not credited with what it never received"
    );
    metrics::describe_gauge!(
        GRPC_STREAMS_ACTIVE,
        "Inbound gRPC server streams currently open, by method. Catches a client holding a \
         stream open forever, which no duration histogram shows until it ends"
    );
    metrics::describe_counter!(
        JSONRPC_ERRORS_TOTAL,
        "Total inbound JSON-RPC errors by method and zcashd-compatible error code"
    );

    // Outbound JSON-RPC
    metrics::describe_counter!(
        RPC_OUTBOUND_REQUESTS_TOTAL,
        "Total outbound JSON-RPC attempts by method and outcome (ok / rpc_error / retried / \
         transport_error). A rising `retried` fraction means the validator's work queue is full \
         and more concurrency will not help; `transport_error` covers the HTTP and timeout \
         failures that return before any JSON-RPC code exists"
    );

    // Mempool
    metrics::describe_gauge!(
        MEMPOOL_COHERENCE_FROZEN_SECONDS,
        "How long tip-coherent mempool reads have been frozen; 0 when live. \
         Brief spikes are normal tip transitions — a sustained non-zero value \
         means the validator tip and Zaino's have stopped agreeing"
    );
    metrics::describe_gauge!(
        MEMPOOL_TRANSACTIONS,
        "Transactions in the published mempool set"
    );
    metrics::describe_gauge!(
        MEMPOOL_BYTES,
        "Size of the published mempool set: `raw` is serialized transaction bytes, `cost` is \
         the ZIP-401 accounting the capacity bound is applied to"
    );
    metrics::describe_gauge!(
        MEMPOOL_UNADMITTED,
        "Transactions known to the validator but refused by Zaino's capacity bound"
    );
    metrics::describe_gauge!(
        MEMPOOL_COMPLETENESS,
        format!(
            "Whether the published mempool set is a full view of the validator's, as a \
             discriminant: {}. Any non-zero value means Zaino knows it is serving a partial set",
            enumerate(&MEMPOOL_COMPLETENESS_VALUES)
        )
    );

    metrics::describe_gauge!(
        BUILD_INFO,
        "Static build metadata; always 1. Version exposed as a label."
    );
}

/// `["a", "b"]` → `0=a, 1=b`, for help text.
///
/// - Legend ships in the scrape (a dashboard-side copy drifts, and drifts silently)
fn enumerate(values: &[&str]) -> String {
    values
        .iter()
        .enumerate()
        .map(|(discriminant, name)| format!("{discriminant}={name}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Seed every work counter at 0.
///
/// - Distinguishes "nothing indexed yet" from "build does not report it"
/// - Per label value; seeding the bare family leaves the real series absent
/// - No height gauges (0 = a false height) and no `method` families (values
///   unknown until a client asks)
fn initialise_counters() {
    for name in [
        SYNC_TRANSACTIONS_TOTAL,
        SYNC_TRANSPARENT_INPUTS_TOTAL,
        SYNC_TRANSPARENT_OUTPUTS_TOTAL,
        SYNC_SAPLING_SPENDS_TOTAL,
        SYNC_SAPLING_OUTPUTS_TOTAL,
        SYNC_ORCHARD_ACTIONS_TOTAL,
        SYNC_IRONWOOD_ACTIONS_TOTAL,
    ] {
        for stage in INGEST_STAGES {
            metrics::counter!(name, INGEST_STAGE => stage).increment(0);
        }
    }

    // Liveness. 0 here is a true statement ("nothing has failed yet"), unlike a
    // 0 height, and absence reads as unsupported exactly when it is being checked
    for outcome in ["ok", "error"] {
        metrics::counter!(SYNC_ITERATIONS_TOTAL, SYNC_OUTCOME => outcome).increment(0);
    }
    metrics::counter!(RESTARTS_TOTAL).increment(0);

    for reason in BATCH_FLUSH_REASONS {
        metrics::counter!(SYNC_BATCH_FLUSH_TOTAL, BATCH_FLUSH_REASON => reason).increment(0);
    }
}

/// `zainod_build_info{version="x.y.z"} 1` — deployed version, queryable in PromQL.
///
/// - Mirrors `zebrad_build_info`
fn set_build_info() {
    metrics::gauge!(
        BUILD_INFO,
        "version" => env!("CARGO_PKG_VERSION"),
    )
    .set(1.0);
}

#[cfg(test)]
mod tests {
    use metrics_exporter_prometheus::PrometheusBuilder;

    use super::*;

    /// Run `body` against an [`init`]-configured recorder, return the scrape.
    ///
    /// - Shares `init`'s bucket registration (own ladder → passes while the
    ///   shipped binary renders summaries)
    fn scrape(body: impl FnOnce()) -> String {
        let mut builder = PrometheusBuilder::new();
        for (metric, _, buckets) in HISTOGRAMS {
            builder = builder
                .set_buckets_for_metric(Matcher::Full((*metric).to_string()), buckets)
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

    /// - Asserts on the rendered scrape, not the calls (`describe_counter!` alone
    ///   also "registers", passing any description-only check)
    #[test]
    fn counters_are_scrapeable_before_their_first_increment() {
        let scrape = scrape(|| {});
        for series in [
            "zaino_sync_transactions_total",
            "zaino_sync_transparent_inputs_total",
            "zaino_sync_transparent_outputs_total",
            "zaino_sync_sapling_spends_total",
            "zaino_sync_sapling_outputs_total",
            "zaino_sync_orchard_actions_total",
            "zaino_sync_ironwood_actions_total",
        ] {
            // Per stage: one series per label value, so seeding one leaves the
            // other absent — and the steady-state loop is the missing one
            for stage in INGEST_STAGES {
                assert!(
                    scrape.contains(&format!("{series}{{stage=\"{stage}\"}} 0")),
                    "`{series}` for stage `{stage}` is absent from a fresh scrape, so \
                     a consumer cannot tell zero from unsupported. Scrape was:\n{scrape}"
                );
            }
        }
    }

    /// - Absent until the first iteration = indistinguishable from unsupported,
    ///   at exactly the moment startup liveness is being checked
    #[test]
    fn liveness_counters_are_scrapeable_before_the_first_iteration() {
        let scrape = scrape(|| {});
        for outcome in ["ok", "error"] {
            assert!(
                scrape.contains(&format!(
                    "zaino_sync_iterations_total{{outcome=\"{outcome}\"}} 0"
                )),
                "the sync heartbeat for outcome `{outcome}` is absent from a fresh scrape. \
                 Scrape was:\n{scrape}"
            );
        }
        assert!(
            scrape.contains("zainod_restarts_total 0"),
            "the restart counter is absent from a fresh scrape, so a first crash is \
             indistinguishable from a build that does not count them. Scrape was:\n{scrape}"
        );
    }

    /// - 0 = a false height, absent = honest until something measures one
    #[test]
    fn height_gauges_are_absent_until_measured() {
        let scrape = scrape(|| {});
        assert!(
            !scrape.contains("zaino_sync_finalized_height "),
            "a finalized-height gauge was published before any block was \
             indexed, which reads as a tip at genesis. Scrape was:\n{scrape}"
        );
    }

    /// - Unbucketed → exporter renders a summary: rolling-window quantiles, not
    ///   aggregatable across instances, and the series still appears
    /// - Driven off `HISTOGRAMS`, not a hand-listed set (how `reorg_depth` slipped)
    #[test]
    fn every_histogram_scrapes_as_a_bucketed_histogram() {
        let scrape = scrape(|| {
            for (metric, _, buckets) in HISTOGRAMS {
                // Inside the first bucket → the `le` assert exercises the
                // configured ladder, not the +Inf overflow
                metrics::histogram!(*metric).record(buckets[0]);
            }
        });

        for (metric, _, buckets) in HISTOGRAMS {
            let series = metric.replace('.', "_");
            assert!(
                scrape.contains(&format!("{series}_bucket")),
                "`{series}` rendered without buckets, so it is a summary and \
                 histogram_quantile() over it is unavailable. Scrape was:\n{scrape}"
            );
            // On its *own* ladder: overlapping matchers resolve by lexicographic
            // accident, the bug `Matcher::Full` prevents
            let lowest = format!("{series}_bucket{{le=\"{}\"}}", buckets[0]);
            assert!(
                scrape.contains(&lowest),
                "`{series}` did not get its configured bucket ladder; expected a \
                 `{lowest}` series. Scrape was:\n{scrape}"
            );
        }
    }

    /// - Other two histogram tests check the table against *itself*; an emitted-
    ///   but-unlisted histogram passes them, then ships as a summary
    /// - Oracle = each emitting crate's `metric_names::HISTOGRAM_METRICS`
    /// - Both directions: missing = summary, stale = `# HELP` for a dead series
    #[test]
    fn histogram_table_covers_every_emitted_histogram_exactly() {
        let mut emitted: Vec<&str> = Vec::new();
        emitted.extend(zaino_state::metric_names::HISTOGRAM_METRICS);
        emitted.extend(zaino_state::mempool_metric_names::HISTOGRAM_METRICS);
        emitted.extend(zaino_serve::metric_names::HISTOGRAM_METRICS);
        emitted.extend(zaino_rpc::metric_names::HISTOGRAM_METRICS);
        emitted.extend(zaino_chain_head_service::metric_names::HISTOGRAM_METRICS);
        emitted.sort_unstable();

        let mut registered: Vec<&str> = HISTOGRAMS.iter().map(|(name, _, _)| *name).collect();
        registered.sort_unstable();

        assert_eq!(
            registered, emitted,
            "`HISTOGRAMS` and the emitting crates' `HISTOGRAM_METRICS` disagree. A name \
             only in the emitters is a histogram with no buckets, which scrapes as a \
             summary; a name only here describes and buckets a metric nothing emits."
        );
    }

    /// - Duplicate = a second help string + ladder, winner decided by iteration order
    #[test]
    fn histogram_table_has_no_duplicate_metrics() {
        let mut names: Vec<&str> = HISTOGRAMS.iter().map(|(name, _, _)| *name).collect();
        names.sort_unstable();
        let unique = names.len();
        names.dedup();
        assert_eq!(
            names.len(),
            unique,
            "`HISTOGRAMS` names a metric more than once; its help text and buckets \
             would then depend on registration order"
        );
    }

    /// - Gauges publish a raw int; the legend is the only thing making it readable
    /// - Variant added without extending the list = permanently unlabelled state
    #[test]
    fn discriminant_legends_are_rendered_into_help_text() {
        // `# HELP` is emitted only for sampled metrics, and neither is
        // pre-created (both label sets are runtime facts) → sample here rather
        // than have registration invent values a deployment may never produce
        let scrape = scrape(|| {
            metrics::gauge!(STATUS, STATUS_COMPONENT => "test").set(0.0);
            metrics::gauge!(MEMPOOL_COMPLETENESS).set(0.0);
        });
        for (metric, values) in [
            ("zaino_status", &STATUS_VALUES[..]),
            (
                "zaino_mempool_completeness",
                &MEMPOOL_COMPLETENESS_VALUES[..],
            ),
        ] {
            let help = scrape
                .lines()
                .find(|line| line.starts_with(&format!("# HELP {metric} ")))
                .unwrap_or_else(|| panic!("`{metric}` has no HELP line. Scrape was:\n{scrape}"));
            for (discriminant, name) in values.iter().enumerate() {
                assert!(
                    help.contains(&format!("{discriminant}={name}")),
                    "`{metric}` help text omits `{discriminant}={name}`, so that state \
                     is unreadable in a dashboard. Help line was:\n{help}"
                );
            }
        }
    }
}
