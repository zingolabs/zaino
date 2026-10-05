//! Sync progress per [`REPORT_INTERVAL`]: `Syncing blocks` during the bulk pass, `Applying to tip`
//! from its end until the non-final window reaches the tip, a stall warning when a whole interval
//! adds nothing

use std::{
    fmt,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use tracing::{info, warn};
use zaino_primitives::types::{Block, Height};

pub(crate) const REPORT_INTERVAL: Duration = Duration::from_secs(30);

/// Bulk fetch position: the producer advances it, the reporter samples it
#[derive(Default)]
pub(crate) struct Progress(Mutex<Tally>);

#[derive(Debug, Clone, Copy, Default)]
struct Tally {
    pass: Option<Pass>,
    /// Tip the window replay is catching up to (set by [`Progress::finish`])
    catchup: Option<Height>,
    height: Option<Height>,
    blocks: u64,
}

/// One bulk pass: `target` = finalized height it fetches to
#[derive(Debug, Clone, Copy)]
struct Pass {
    target: Height,
    started: Instant,
    blocks: u64,
}

impl Progress {
    fn tally(&self) -> std::sync::MutexGuard<'_, Tally> {
        self.0.lock().expect("progress lock never held across a panic")
    }

    /// Bulk pass from `start` to `target`, both inclusive, begins
    pub(crate) fn start(&self, start: Height, target: Height, tip: Height) {
        let mut tally = self.tally();
        tally.pass = Some(Pass { target, started: Instant::now(), blocks: tally.blocks });
        let (start, target, tip) = (u32::from(start), u32::from(target), u32::from(tip));
        info!(start, target, tip, "Syncing to finalized target");
    }

    /// Quorum tip moved mid-pass
    pub(crate) fn extend(&self, target: Height) {
        if let Some(pass) = self.tally().pass.as_mut() {
            pass.target = target;
        }
    }

    pub(crate) fn added(&self, block: &Block) {
        let mut tally = self.tally();
        let height = block.header().height;
        if tally.catchup.is_some_and(|tip| height >= tip) {
            tally.catchup = None;
        }
        tally.height = Some(height);
        tally.blocks += 1;
    }

    /// Bulk pass reached its target; the window replay up to `tip` follows
    pub(crate) fn finish(&self, tip: Height) {
        let mut tally = self.tally();
        let Some(pass) = tally.pass.take() else {
            return;
        };
        tally.catchup = Some(tip);
        let elapsed = pass.started.elapsed();
        let blocks = tally.blocks - pass.blocks;
        info!(
            height = u32::from(pass.target),
            blocks,
            elapsed = %Human(elapsed),
            bps = per_second(blocks, elapsed),
            "Reached finalized target"
        );
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

/// Every [`REPORT_INTERVAL`]: a summary while a bulk pass runs, silent otherwise
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
    let Some(height) = now.tally.height else {
        return;
    };
    let (target, bulk) = match (now.tally.pass, now.tally.catchup) {
        (Some(pass), _) => (pass.target, true),
        (None, Some(tip)) => (tip, false),
        (None, None) => return,
    };
    let elapsed = now.at - last.at;
    let blocks = now.tally.blocks - last.tally.blocks;
    let (height, target) = (u32::from(height), u32::from(target));
    if blocks == 0 {
        warn!(height, target, stalled = %Human(elapsed), "Block fetch stalled");
        return;
    }
    let rate = blocks as f64 / elapsed.as_secs_f64();
    let bps = per_second(blocks, elapsed);
    let eta = Human(Duration::from_secs_f64(f64::from(target.saturating_sub(height)) / rate));
    match bulk {
        true => info!(height, target, bps, eta = %eta, "Syncing blocks"),
        false => info!(applied = height, tip = target, bps, eta = %eta, "Applying to tip"),
    }
}

/// Whole units per second
fn per_second(count: u64, over: Duration) -> u64 {
    match over.as_secs_f64() {
        secs if secs > 0.0 => (count as f64 / secs).round() as u64,
        _ => 0,
    }
}

/// Two largest units, no spaces: `812ms`, `45s`, `9m57s`, `2h13m`, `3d04h`
pub(crate) struct Human(pub(crate) Duration);

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
