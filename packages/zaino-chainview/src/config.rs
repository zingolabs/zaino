//! Poll cadence, retry ladder, telemetry thresholds

use std::time::Duration;

/// Delay between polls of one endpoint
pub(crate) const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Reconcile cadence while its push stream is up (events wake it sooner; the poll stays the truth)
pub(crate) const STREAMED_POLL_INTERVAL: Duration = Duration::from_secs(15);

/// Floor between two polls of one endpoint (a burst of events = one poll per floor, not per event)
pub(crate) const MIN_POLL_SPACING: Duration = Duration::from_millis(200);

/// First retry delay (doubles up to `MAX_BACKOFF`)
pub(crate) const INITIAL_BACKOFF: Duration = Duration::from_millis(500);

pub(crate) const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Consecutive failures before an endpoint is ejected (`Down`)
pub(crate) const MAX_CONSECUTIVE_FAILURES: u32 = 10;

/// Delay between peer + release reads (peer graph and version move far slower than the mempool)
pub(crate) const METADATA_REFRESH: Duration = Duration::from_secs(60);

/// Delay between "catching up" warnings for one endpoint (polled every `POLL_INTERVAL`)
pub(crate) const CATCHING_UP_WARN_INTERVAL: Duration = Duration::from_secs(60);

/// Live tip this far behind its own clock estimate = stale (P(natural 30 min gap) ≈ e^-24)
pub(crate) const STALE_TIP_BLOCKS: u32 = 24;

/// Release halting within this many blocks of its tip = alarm (one week at 75 s)
pub(crate) const END_OF_SERVICE_WARN_BLOCKS: u32 = 7 * 24 * 3600 / 75;

/// Distinct outbound peers across every live endpoint at or below this (and > 0) = eclipse risk
pub(crate) const ECLIPSE_OUTBOUND_MAX: usize = 2;
