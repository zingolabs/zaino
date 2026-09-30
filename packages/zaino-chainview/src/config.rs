//! Poll cadence, retry ladder, ancestry fetch, telemetry thresholds

use std::time::Duration;

/// Delay between polls of one endpoint
pub(crate) const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// First retry delay (doubles up to `MAX_BACKOFF`)
pub(crate) const INITIAL_BACKOFF: Duration = Duration::from_millis(500);

pub(crate) const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Consecutive failures before an endpoint is ejected (`Down`)
pub(crate) const MAX_CONSECUTIVE_FAILURES: u32 = 10;

/// Delay between `getpeerinfo` reads (the peer graph moves far slower than the mempool)
pub(crate) const PEER_REFRESH: Duration = Duration::from_secs(60);

/// Delay between "catching up" warnings for one endpoint (polled every `POLL_INTERVAL`)
pub(crate) const CATCHING_UP_WARN_INTERVAL: Duration = Duration::from_secs(60);

/// `getblockheader` calls in flight per endpoint (first build = `depth` of them, once)
pub(crate) const LINK_FETCH_CONCURRENCY: usize = 16;

/// Heights fetched per descent step below the held tip (a reorg rarely reaches past one batch)
pub(crate) const LINK_BATCH: u32 = 16;

/// Live tip this far behind its own clock estimate = stale (P(natural 30 min gap) ≈ e^-24)
pub(crate) const STALE_TIP_BLOCKS: u32 = 24;

/// Distinct outbound peers across every live endpoint at or below this (and > 0) = eclipse risk
pub(crate) const ECLIPSE_OUTBOUND_MAX: usize = 2;
