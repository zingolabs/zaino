//! Sync progress per [`REPORT_INTERVAL`]: `Syncing blocks` while the blocks sent trail the
//! verified best, a stall warning when a whole interval sends nothing; silent at the tip

use std::{
    fmt,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use tracing::{info, warn};
use zaino_primitives::types::{Block, Height};

pub(crate) const REPORT_INTERVAL: Duration = Duration::from_secs(30);

/// Last block sent vs the verified best: the producer advances it, the reporter samples it
#[derive(Default)]
pub(crate) struct Progress(Mutex<Tally>);

#[derive(Debug, Clone, Copy, Default)]
struct Tally {
    target: Option<Height>,
    height: Option<Height>,
    blocks: u64,
}

impl Progress {
    fn tally(&self) -> std::sync::MutexGuard<'_, Tally> {
        self.0.lock().expect("progress lock never held across a panic")
    }

    /// Verified best moved
    pub(crate) fn target(&self, best: Height) {
        self.tally().target = Some(best);
    }

    pub(crate) fn added(&self, block: &Block) {
        let mut tally = self.tally();
        tally.height = Some(block.header().height);
        tally.blocks += 1;
    }

    fn sample(&self) -> Sample {
        Sample { at: Instant::now(), tally: *self.tally() }
    }
}

#[derive(Debug, Clone, Copy)]
struct Sample {
    at: Instant,
    tally: Tally,
}

/// Every [`REPORT_INTERVAL`]: a summary while behind the best, silent at it
pub(crate) async fn run(progress: Arc<Progress>) {
    let mut ticks = tokio::time::interval(REPORT_INTERVAL);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticks.tick().await;
    let mut last = progress.sample();
    loop {
        ticks.tick().await;
        let now = progress.sample();
        summarise(&last, &now);
        last = now;
    }
}

fn summarise(last: &Sample, now: &Sample) {
    let Some(target) = now.tally.target else {
        return;
    };
    if now.tally.height >= Some(target) {
        return;
    }
    let elapsed = now.at - last.at;
    let blocks = now.tally.blocks - last.tally.blocks;
    let (height, target) = (now.tally.height.map_or(0, u32::from), u32::from(target));
    if blocks == 0 {
        warn!(height, target, stalled = %Human(elapsed), "Block fetch stalled");
        return;
    }
    let rate = blocks as f64 / elapsed.as_secs_f64();
    let bps = per_second(blocks, elapsed);
    let eta = Human(Duration::from_secs_f64(f64::from(target.saturating_sub(height)) / rate));
    info!(height, target, bps, eta = %eta, "Syncing blocks");
}

/// Whole units per second
fn per_second(count: u64, over: Duration) -> u64 {
    match over.as_secs_f64() {
        secs if secs > 0.0 => (count as f64 / secs).round() as u64,
        _ => 0,
    }
}

/// Two largest units, no spaces: `812ms`, `45s`, `9m57s`, `2h13m`, `3d04h`
pub struct Human(pub Duration);

impl fmt::Display for Human {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let secs = self.0.as_secs();
        if secs == 0 {
            return write!(f, "{}ms", self.0.as_millis());
        }
        let (d, h, m, s) = (secs / 86_400, secs / 3_600 % 24, secs / 60 % 60, secs % 60);
        match (d, h, m) {
            (0, 0, 0) => write!(f, "{s}s"),
            (0, 0, _) => write!(f, "{m}m{s:02}s"),
            (0, _, _) => write!(f, "{h}h{m:02}m"),
            _ => write!(f, "{d}d{h:02}h"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_durations_keep_the_two_largest_units() {
        assert_eq!(Human(Duration::from_millis(812)).to_string(), "812ms");
        for (secs, shown) in [
            (0, "0ms"),
            (45, "45s"),
            (597, "9m57s"),
            (3_600, "1h00m"),
            (7_980, "2h13m"),
            (273_600, "3d04h"),
        ] {
            assert_eq!(Human(Duration::from_secs(secs)).to_string(), shown, "{secs}");
        }
        assert_eq!(per_second(90, Duration::from_secs(30)), 3);
        assert_eq!(per_second(90, Duration::ZERO), 0);
    }
}
