//! Telemetry thresholds (poll cadence + retry ladder = `zaino-traffic`'s)

use std::time::Duration;

/// Delay between "catching up" warnings for one endpoint (polled every second)
pub(crate) const CATCHING_UP_WARN_INTERVAL: Duration = Duration::from_secs(60);

/// Live tip this far behind its own clock estimate = stale (P(natural 30 min gap) ≈ e^-24)
pub(crate) const STALE_TIP_BLOCKS: u32 = 24;

/// Release halting within this many blocks of its tip = alarm (one week at 75 s)
pub(crate) const END_OF_SERVICE_WARN_BLOCKS: u32 = 7 * 24 * 3600 / 75;

/// Distinct outbound peers across every live endpoint at or below this (and > 0) = eclipse risk
pub(crate) const ECLIPSE_OUTBOUND_MAX: usize = 2;
