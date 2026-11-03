//! Alarm + warning thresholds (poll cadence + retry ladder = `zaino-traffic`'s)

use std::time::Duration;

/// Block `depth` deep owed finality this long, finalized tip unmoved = finality paused (alarm)
pub(crate) const FINALITY_PAUSE: Duration = Duration::from_secs(60);

/// Tip this many target spacings behind the clock = stale (P(natural gap) ≈ e^-24)
pub const STALE_TIP_BLOCKS: u32 = 24;

/// Release halting within this target time of its tip = alarm (blocks → time at each height's
/// spacing: 75 s, 25 s from NU7)
pub(crate) const END_OF_SERVICE_WARN: Duration = Duration::from_secs(7 * 24 * 3600);
