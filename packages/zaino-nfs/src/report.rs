//! Sync progress per [`REPORT_INTERVAL`]: `Syncing blocks` while the blocks handed to the indexes
//! trail the verified best, a stall warning when a whole interval hands over nothing; silent at
//! the tip

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use tracing::{info, warn};
use zaino_primitives::types::Height;
use zaino_sync::Human;

pub const REPORT_INTERVAL: Duration = Duration::from_secs(30);

/// Last block handed over vs the verified best: the driver advances it, the reporter samples it
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

    pub(crate) fn target(&self, best: Height) {
        self.tally().target = Some(best);
    }

    pub(crate) fn handed(&self, height: Height) {
        let mut tally = self.tally();
        tally.height = Some(height);
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

/// Every [`REPORT_INTERVAL`] until dropped: a summary while behind the best, silent at it
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_second_rounds_and_a_zero_interval_is_zero() {
        assert_eq!(per_second(90, Duration::from_secs(30)), 3);
        assert_eq!(per_second(100, Duration::from_secs(30)), 3);
        assert_eq!(per_second(90, Duration::ZERO), 0);
    }
}
