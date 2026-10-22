//! Alarm + warning thresholds (poll cadence + retry ladder = `zaino-traffic`'s)

use std::time::Duration;

/// Delay between "catching up" warnings for one trusted validator (polled every second)
pub(crate) const CATCHING_UP_WARN_INTERVAL: Duration = Duration::from_secs(60);

/// Block `depth` deep owed finality this long, finalized tip unmoved = finality stalled (alarm)
pub(crate) const FINALITY_STALL: Duration = Duration::from_secs(60);

/// Tip this many target spacings behind the clock = stale (P(natural gap) ≈ e^-24)
pub const STALE_TIP_BLOCKS: u32 = 24;

/// Release halting within this many blocks of its tip = alarm (one week at 75 s)
pub(crate) const END_OF_SERVICE_WARN_BLOCKS: u32 = 7 * 24 * 3600 / 75;
