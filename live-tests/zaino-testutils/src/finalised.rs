//! Gating on zaino's finalised index.

use std::time::Duration;

use anyhow::Result;
use ztest::prelude::{ZainoIndex, ZainoIndexer};

/// Poll the compact-block index's durable extent until >= `target`
///
/// - No served height answers this (pre-commit serves above the durable extent)
/// - Needs a `prometheus` image (no exporter → `Err`, not a hang)
pub async fn wait_for_finalised(
    indexer: &ZainoIndexer,
    target: u32,
    timeout: Duration,
) -> Result<u32> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        // Unpublished until the first commit → keep polling
        let finalised = indexer.finalized_height(ZainoIndex::CompactBlock).await?;
        if let Some(finalised) = finalised.filter(|f| *f >= target) {
            return Ok(finalised);
        }
        let expired = tokio::time::Instant::now() >= deadline;
        anyhow::ensure!(!expired, "finalised {finalised:?}, never {target} within {timeout:?}");
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
