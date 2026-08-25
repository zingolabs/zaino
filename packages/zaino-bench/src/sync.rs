//! Initial-sync timing — "how long does it take to sync mainnet?"
//!
//! Samples zainod's Prometheus endpoint until its sync loop reports it has
//! reached the chain tip, then reports wall-clock time and the block rate.
//! Nothing here touches the node under test beyond an HTTP GET, so the number
//! it produces is the node's, not the harness's.

use std::fmt::Write as _;
use std::io::Write as _;
use std::time::{Duration, Instant};

use clap::Args;

use crate::error::BenchError;
use crate::metrics::{self, names, Scrape};

/// Measure how long zainod takes to sync from its current height to the tip.
///
/// Start this *before* zainod: it waits for `/metrics` to come up, so t0 is the
/// moment the node starts rather than the moment you reached a second terminal.
#[derive(Args)]
pub(super) struct SyncArgs {
    /// zainod's Prometheus scrape endpoint.
    ///
    /// Requires zainod built with `--features prometheus` and `metrics_endpoint`
    /// set in its config.
    #[arg(long, default_value = "http://127.0.0.1:9998/metrics")]
    metrics_url: String,

    /// Seconds between samples.
    #[arg(long, default_value = "10")]
    poll_interval_secs: u64,

    /// Stop once the finalised height reaches this, instead of waiting for the
    /// tip. Useful for a bounded run over a fixed span of blocks.
    #[arg(long)]
    until_height: Option<u64>,

    /// Fail if the finalised height does not advance for this many seconds.
    #[arg(long, default_value = "900")]
    stall_timeout_secs: u64,

    /// Also write every sample to this CSV, for graphing the sync curve.
    #[arg(long)]
    csv: Option<String>,
}

/// One sample of the node's sync progress.
struct Sample {
    elapsed: Duration,
    finalized_height: u64,
    target_height: u64,
    /// The node's own `zaino.sync.lag_blocks` gauge, recorded raw. Not used
    /// for progress or completion — see [`Sample::lag`] for why — but kept in
    /// the CSV so the run has the node's own reading next to the derived one.
    lag_blocks: Option<u64>,
    db_tip_height: Option<u64>,
    chain_tip_height: Option<u64>,
    transactions: Option<u64>,
    reached_tip: bool,
}

impl Sample {
    /// Blocks still to go.
    ///
    /// Derived from the two height gauges rather than read from
    /// `zaino.sync.lag_blocks`. That gauge is set to
    /// `chain_tip - finalized_height_floor(chain_tip)`, which is the
    /// non-finalised seam depth — a constant, not the distance left to sync. It
    /// reads as ~100 whether the node is at the tip or three million blocks
    /// short of it, so it cannot drive progress or completion here. It is still
    /// recorded in the CSV as the node reported it.
    fn lag(&self) -> u64 {
        self.target_height.saturating_sub(self.finalized_height)
    }

    /// Whether the finalised writer has caught up with the height it is syncing
    /// to.
    ///
    /// This, not the `zaino.sync.has_reached_tip` gauge, is what ends a run.
    /// That gauge is set when the sync loop's iteration returns `Ok`, and
    /// `FinalisedState::sync_to_height` returns `Ok` as soon as it has *spawned*
    /// the background sync (it is single-flight: a poll landing on an in-flight
    /// sync is a no-op that also returns `Ok`). So the gauge goes to 1 seconds
    /// after start-up and stays there for the whole multi-hour sync — it means
    /// "the sync loop is healthy", not "the index is at the tip".
    ///
    /// `target_height` is the finalised floor the writer was spawned against,
    /// so `finalized >= target` is the writer's own definition of done, read
    /// from the outside.
    fn caught_up(&self) -> bool {
        self.target_height > 0 && self.finalized_height >= self.target_height
    }
}

pub(super) async fn run(args: SyncArgs) -> Result<(), BenchError> {
    let poll_interval = Duration::from_secs(args.poll_interval_secs.max(1));
    let stall_timeout = Duration::from_secs(args.stall_timeout_secs);
    let client = reqwest::Client::new();

    eprintln!("Metrics:  {}", args.metrics_url);
    eprintln!("Sampling every {}s", poll_interval.as_secs());
    eprintln!();

    let first = metrics::await_metric(
        &client,
        &args.metrics_url,
        names::SYNC_FINALIZED_HEIGHT,
        poll_interval,
    )
    .await;
    let started = Instant::now();
    let start_height = first.height(names::SYNC_FINALIZED_HEIGHT)?;

    eprintln!("Start height: {start_height}");
    if let Some(target) = first.get(names::SYNC_TARGET_HEIGHT) {
        eprintln!("Target height (at t0): {}", target as u64);
    }
    eprintln!();

    let mut samples = vec![sample(&first, Duration::ZERO)?];
    let mut last_progress = (started, start_height);

    loop {
        if finished(samples.last(), args.until_height) {
            break;
        }

        tokio::time::sleep(poll_interval).await;

        let scrape = metrics::scrape(&client, &args.metrics_url).await?;
        let current = sample(&scrape, started.elapsed())?;

        if current.finalized_height > last_progress.1 {
            last_progress = (Instant::now(), current.finalized_height);
        } else if last_progress.0.elapsed() >= stall_timeout {
            report(&samples, start_height);
            return Err(BenchError::SyncStalled {
                height: current.finalized_height,
                timeout: stall_timeout,
            });
        }

        eprintln!("{}", progress_line(&current, samples.last()));
        samples.push(current);
    }

    report(&samples, start_height);

    if let Some(path) = &args.csv {
        write_csv(path, &samples)?;
        eprintln!();
        eprintln!("  Sample curve written to {path}");
    }

    Ok(())
}

/// Whether the run is done: the finalised height has caught up with the height
/// being synced to, or the caller's `--until-height` has been passed.
fn finished(latest: Option<&Sample>, until_height: Option<u64>) -> bool {
    let Some(latest) = latest else {
        return false;
    };

    match until_height {
        Some(target) => latest.finalized_height >= target,
        None => latest.caught_up(),
    }
}

fn sample(scrape: &Scrape, elapsed: Duration) -> Result<Sample, BenchError> {
    Ok(Sample {
        elapsed,
        finalized_height: scrape.height(names::SYNC_FINALIZED_HEIGHT)?,
        target_height: scrape.height(names::SYNC_TARGET_HEIGHT)?,
        lag_blocks: scrape
            .get(names::SYNC_LAG_BLOCKS)
            .map(|v| v.max(0.0) as u64),
        db_tip_height: scrape.get(names::DB_TIP_HEIGHT).map(|v| v as u64),
        chain_tip_height: scrape.get(names::CHAIN_TIP_HEIGHT).map(|v| v as u64),
        transactions: scrape.get(names::SYNC_TRANSACTIONS_TOTAL).map(|v| v as u64),
        // Absent until the sync loop completes its first iteration, which is
        // "not yet at tip" rather than an error.
        reached_tip: scrape.get(names::SYNC_HAS_REACHED_TIP).unwrap_or(0.0) >= 1.0,
    })
}

fn progress_line(current: &Sample, previous: Option<&Sample>) -> String {
    let mut line = format!(
        "  [{:>8.0}s] height {:>9} / {:<9}  lag {:>8}",
        current.elapsed.as_secs_f64(),
        current.finalized_height,
        current.target_height,
        current.lag(),
    );

    if let Some(rate) = interval_rate(current, previous) {
        let _ = write!(line, "  {rate:>9.0} blocks/s");
    }
    if current.caught_up() {
        line.push_str("  ✅ caught up");
    }

    line
}

/// Blocks per second over the interval between two samples.
///
/// `None` when there is no earlier sample, or when two samples share a
/// timestamp — a rate over a zero interval is not a number worth printing.
fn interval_rate(current: &Sample, previous: Option<&Sample>) -> Option<f64> {
    let previous = previous?;
    let seconds = (current.elapsed.saturating_sub(previous.elapsed)).as_secs_f64();
    if seconds <= 0.0 {
        return None;
    }
    let blocks = current
        .finalized_height
        .saturating_sub(previous.finalized_height);
    Some(blocks as f64 / seconds)
}

fn report(samples: &[Sample], start_height: u64) {
    let Some(last) = samples.last() else {
        return;
    };

    let elapsed = last.elapsed.as_secs_f64();
    let blocks = last.finalized_height.saturating_sub(start_height);

    eprintln!();
    eprintln!("══════════════════════════════════════════");
    eprintln!("  Initial Sync — Summary");
    eprintln!("══════════════════════════════════════════");
    eprintln!("  Start height:       {start_height}");
    eprintln!("  End height:         {}", last.finalized_height);
    eprintln!("  Blocks synced:      {blocks}");
    eprintln!("  Target height:      {}", last.target_height);
    if let Some(db_tip) = last.db_tip_height {
        eprintln!("  Db tip height:      {db_tip}");
    }
    if let Some(chain_tip) = last.chain_tip_height {
        eprintln!("  Chain tip height:   {chain_tip}");
    }
    if let Some(transactions) = last.transactions {
        eprintln!("  Transactions indexed: {transactions}");
    }
    eprintln!("  Wall-clock time:    {}", human_duration(last.elapsed));
    if elapsed > 0.0 {
        eprintln!(
            "  Mean rate:          {:.0} blocks/s",
            blocks as f64 / elapsed
        );
    }
    eprintln!(
        "  Caught up:          {}",
        if last.caught_up() { "yes" } else { "no" }
    );
    eprintln!(
        "  Node's has_reached_tip gauge: {} (set once the sync loop is healthy, \
         not on arrival at the tip)",
        if last.reached_tip { "1" } else { "0" }
    );
}

fn human_duration(duration: Duration) -> String {
    let total = duration.as_secs();
    format!(
        "{}h {:02}m {:02}s ({:.1}s)",
        total / 3600,
        (total % 3600) / 60,
        total % 60,
        duration.as_secs_f64()
    )
}

fn write_csv(path: &str, samples: &[Sample]) -> Result<(), BenchError> {
    let csv_error = |source| BenchError::Csv {
        path: path.to_string(),
        source,
    };

    let mut file = std::fs::File::create(path).map_err(csv_error)?;
    writeln!(
        file,
        "elapsed_secs,finalized_height,target_height,lag_blocks,node_lag_gauge,db_tip_height,chain_tip_height,transactions_total,interval_blocks_per_sec"
    )
    .map_err(csv_error)?;

    let mut previous: Option<&Sample> = None;
    for current in samples {
        writeln!(
            file,
            "{:.3},{},{},{},{},{},{},{},{}",
            current.elapsed.as_secs_f64(),
            current.finalized_height,
            current.target_height,
            current.lag(),
            optional(current.lag_blocks),
            optional(current.db_tip_height),
            optional(current.chain_tip_height),
            optional(current.transactions),
            interval_rate(current, previous)
                .map(|rate| format!("{rate:.3}"))
                .unwrap_or_default(),
        )
        .map_err(csv_error)?;
        previous = Some(current);
    }

    Ok(())
}

fn optional(value: Option<u64>) -> String {
    value.map(|v| v.to_string()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(elapsed_secs: u64, finalized_height: u64, reached_tip: bool) -> Sample {
        Sample {
            elapsed: Duration::from_secs(elapsed_secs),
            finalized_height,
            target_height: 3_390_744,
            lag_blocks: None,
            db_tip_height: Some(finalized_height),
            chain_tip_height: Some(3_390_744),
            transactions: None,
            reached_tip,
        }
    }

    /// The node's `lag_blocks` gauge reports the non-finalised seam depth, a
    /// constant, so a run three million blocks short of the tip and one sitting
    /// on it both publish roughly the same value. `lag` must therefore derive
    /// from the heights and ignore the gauge entirely.
    #[test]
    fn lag_is_derived_from_the_heights_not_the_nodes_gauge() {
        let mut sample = at(10, 3_000_000, false);
        assert_eq!(sample.lag(), 390_744, "target - finalized");

        sample.lag_blocks = Some(100);
        assert_eq!(
            sample.lag(),
            390_744,
            "the seam-depth gauge must not override the derived lag"
        );
    }

    /// The regression behind an early exit: the node sets `has_reached_tip` as
    /// soon as its sync loop is healthy, which is seconds into a multi-hour
    /// sync. Completion must follow the heights instead.
    #[test]
    fn the_nodes_reached_tip_gauge_does_not_end_a_run() {
        let early = at(30, 8_897, true);
        assert!(
            !early.caught_up(),
            "8897 of 3390744 is not caught up, whatever the gauge says"
        );
        assert!(!finished(Some(&early), None), "the run must keep going");

        let done = at(9_000, 3_390_744, false);
        assert!(done.caught_up(), "finalized == target is caught up");
        assert!(finished(Some(&done), None), "and ends the run");
    }

    #[test]
    fn a_run_without_until_height_finishes_when_it_catches_up() {
        assert!(!finished(Some(&at(10, 100, false)), None));
        assert!(finished(Some(&at(10, 3_390_744, false)), None));
    }

    #[test]
    fn until_height_finishes_the_run_before_the_tip() {
        assert!(!finished(Some(&at(10, 99, false)), Some(100)));
        assert!(finished(Some(&at(10, 100, false)), Some(100)));
        assert!(finished(Some(&at(10, 101, false)), Some(100)));
    }

    #[test]
    fn a_run_with_no_samples_yet_is_not_finished() {
        assert!(!finished(None, None));
        assert!(!finished(None, Some(100)));
    }

    #[test]
    fn interval_rate_is_blocks_over_the_gap() {
        let previous = at(10, 1_000, false);
        let current = at(20, 6_000, false);
        assert_eq!(interval_rate(&current, Some(&previous)), Some(500.0));
    }

    #[test]
    fn interval_rate_needs_two_samples_and_a_positive_gap() {
        let current = at(10, 1_000, false);
        assert_eq!(interval_rate(&current, None), None);
        assert_eq!(interval_rate(&current, Some(&at(10, 900, false))), None);
    }
}
