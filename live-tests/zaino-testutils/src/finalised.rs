//! Gating on zaino's finalised index.

use std::time::Duration;

use anyhow::Result;
use ztest::prelude::{family, Exporter, ZainoIndexer};

/// Per-scrape budget — `timeout` bounds the whole poll loop, not one round trip
const SCRAPE_TIMEOUT: Duration = Duration::from_secs(10);

/// Polls the finalised writer's committed tip until it reaches `target`.
///
/// No served height answers this. Everything above the seam comes from the
/// in-memory chain head, and below it zaino serves straight from the validator
/// it proxies, so both read correct at heights the index has never written.
///
/// `zaino_db_tip_height` and not [`ZainoIndexer::index_frontier`]: that reads
/// `zaino_sync_finalized_height`, set only inside a throttled progress-log
/// branch and so never created on a short chain, then falls back to a gauge
/// zaino does not emit.
///
/// Requires the indexer image to carry the `prometheus` feature; without it
/// there is no exporter to scrape and this returns `Err` rather than hanging.
///
/// Only meaningful where the chain is longer than the seam depth — the writer
/// targets `tip - MAX_NONFINALISED_DEPTH` (1001, or 100 under `fast-test-seam`)
/// and saturates to genesis below that.
pub async fn wait_for_finalised(
    indexer: &ZainoIndexer,
    target: u32,
    timeout: Duration,
) -> Result<u32> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        // Absent until the first batch commit — keep polling, do not abort.
        let frontier = indexer
            .read(SCRAPE_TIMEOUT)
            .await
            .map_err(anyhow::Error::msg)?
            .height_gauge(family("zaino_db_tip_height"));
        if let Some(frontier) = frontier.filter(|f| *f >= target) {
            return Ok(frontier);
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "finalised index reached {frontier:?}, never {target}, within {timeout:?}; the \
             usual cause is an indexer image built without `fast-test-seam`, whose write \
             target is `tip - 1001` and saturates to genesis on a short chain"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
