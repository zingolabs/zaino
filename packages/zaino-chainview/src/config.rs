//! Poll cadence, retry ladder

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
