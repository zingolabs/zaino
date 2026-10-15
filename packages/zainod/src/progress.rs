//! One progress task: every [`REPORT_INTERVAL`], read from the `GrpcSnapshot` + `SyncProgress`
//!
//! - one line per tick for the [`Phase`] still outstanding: `Syncing blocks` (final, to the
//!   writers), then `Fetching non-finalized blocks` (served → best, above the finalized tip)
//! - stall warning once that line has not moved for [`REPORT_INTERVAL`]
//! - one `Syncing` line per enabled index (applied + durable off its writer's handle, size) while
//!   final blocks outstanding
//! - disk walked at most every [`WALK_EVERY`] (`size` here + the index bytes gauge)
//! - the phase's blocks/s over the last two intervals (`/status` `progress.rate`)

use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    time::Duration,
};

use tokio::{sync::watch, time::Instant};
use tracing::{field::display, info, warn, Span};
use zaino_grpc_snapshot::{GrpcSnapshots, Tips};
use zaino_persistence::{DiskView, IndexKind};
use zaino_primitives::display::{ByteSize, Human};
use zaino_sync::{IndexHandle, SyncProgress};
use zcash_protocol::consensus::BlockHeight;

use crate::error::IndexerError;
use crate::logging::HeightCol;

pub(crate) const REPORT_INTERVAL: Duration = Duration::from_secs(30);
const WALK_EVERY: Duration = Duration::from_secs(120);

/// One enabled index: its log component, directory and writer's handle
pub(crate) struct Index {
    pub(crate) kind: IndexKind,
    pub(crate) span: Span,
    pub(crate) dir: PathBuf,
    pub(crate) handle: IndexHandle<DiskView>,
}

/// Last walk's bytes per index name (unreadable = absent)
type Disk = BTreeMap<&'static str, u64>;

/// Blocks handed since boot + the served height (0 before any), when
#[derive(Clone, Copy)]
struct Sample {
    at: Instant,
    handed: u64,
    served: u32,
}

impl Sample {
    /// `phase`'s blocks since `before` (a reorg lowering the served tip = none)
    fn advanced(&self, before: &Sample, phase: Phase) -> u64 {
        match phase {
            Phase::Final { .. } => self.handed - before.handed,
            Phase::Window { .. } => u64::from(self.served.saturating_sub(before.served)),
        }
    }
}

/// Outstanding work, final blocks first (the NFS window opens above the writers' durable tips)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// handed, else the lowest durable tip (`None` = no block anywhere yet) → the final tip
    Final { done: Option<u32>, target: u32 },
    /// served → best, each counted above the final tip
    Window { fetched: u32, total: u32 },
}

impl Phase {
    /// `None` = nothing outstanding (synced, or no chain yet)
    fn of(tips: &Tips, handed: Option<u32>, durable: Option<u32>) -> Option<Self> {
        let final_tip = u32::from(tips.final_tip?.height);
        let done = handed.max(durable);
        if done.is_none_or(|done| done < final_tip) {
            return Some(Self::Final { done, target: final_tip });
        }
        let (served, best) = (u32::from(tips.served?.height), u32::from(tips.best?.height));
        (!tips.synced && served < best).then(|| Self::Window {
            fetched: served.saturating_sub(final_tip),
            total: best - final_tip,
        })
    }

    fn remaining(self) -> u32 {
        match self {
            Self::Final { done, target } => target - done.unwrap_or(0),
            Self::Window { fetched, total } => total - fetched,
        }
    }
}

/// Until `cancel`; each tick's blocks/s over the last two in `rate`
pub(crate) async fn run(
    grpc_snapshots: GrpcSnapshots<DiskView>,
    progress: SyncProgress,
    indexes: Vec<Index>,
    rate: watch::Sender<Option<f64>>,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<(), IndexerError> {
    let mut ticks = tokio::time::interval_at(Instant::now() + REPORT_INTERVAL, REPORT_INTERVAL);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let sample = |tips: &Tips| {
        let served = tips.served.map_or(0, |served| served.height.into());
        Sample { at: Instant::now(), handed: progress.blocks(), served }
    };
    let mut last = sample(&grpc_snapshots.load().tips());
    let mut before_last = last;
    let (mut last_phase, mut moved_at) = (None::<Phase>, last.at);
    let (mut disk, mut walked) = (Disk::new(), None::<Instant>);
    loop {
        tokio::select! {
            () = cancel.cancelled() => return Ok(()),
            _ = ticks.tick() => {}
        }
        if walked.is_none_or(|at| at.elapsed() >= WALK_EVERY) {
            disk = walk(&indexes).await?;
            for (index, bytes) in &disk {
                crate::disk_monitor::walked(index, *bytes);
            }
            walked = Some(Instant::now());
        }
        let views = crate::disk_monitor::views(&indexes);
        tokio::task::spawn_blocking(move || crate::disk_monitor::sample(&views)).await?;
        let tips = grpc_snapshots.load().tips();
        let now = sample(&tips);
        let durable = indexes
            .iter()
            .map(|index| index.handle.durable_height().map(u32::from))
            .min()
            .flatten();
        let phase = Phase::of(&tips, progress.handed().map(u32::from), durable);
        let over_two = (now.at - before_last.at).as_secs_f64();
        rate.send_replace(phase.map(|phase| now.advanced(&before_last, phase) as f64 / over_two));
        if let Some(phase) = phase {
            let kind = |phase: Option<Phase>| phase.as_ref().map(std::mem::discriminant);
            if kind(last_phase) != kind(Some(phase)) || now.advanced(&last, phase) > 0 {
                moved_at = now.at;
            }
            report(phase, now.advanced(&last, phase), now.at - last.at, now.at - moved_at);
        }
        if matches!(phase, Some(Phase::Final { .. })) {
            let column = |height: Option<BlockHeight>| HeightCol(height.map(u32::from));
            for index in &indexes {
                let applied = column(index.handle.applied_height());
                let durable = column(index.handle.durable_height());
                let size = disk.get(index.kind.name()).map(|bytes| display(ByteSize(*bytes)));
                index.span.in_scope(|| info!(%applied, %durable, size, "Syncing"));
            }
        }
        (before_last, last, last_phase) = (last, now, phase);
    }
}

/// One line per tick: `advanced` blocks `over` the interval (rate + eta), or a stall once `idle`
/// (unmoved) for a whole [`REPORT_INTERVAL`]
fn report(phase: Phase, advanced: u64, over: Duration, idle: Duration) {
    let bps = per_second(advanced, over);
    let eta = (advanced > 0).then(|| {
        let secs_per_block = over.as_secs_f64() / advanced as f64;
        display(Human(Duration::from_secs_f64(f64::from(phase.remaining()) * secs_per_block)))
    });
    let stalled = (idle >= REPORT_INTERVAL).then(|| display(Human(idle)));
    match (phase, stalled) {
        (Phase::Final { done, target }, None) => {
            info!(height = %HeightCol(done), target, bps, eta, "Syncing blocks");
        }
        (Phase::Final { done, target }, Some(stalled)) => {
            warn!(height = %HeightCol(done), target, %stalled, "Block fetch stalled");
        }
        (Phase::Window { fetched, total }, None) => {
            info!(fetched, total, bps, eta, "Fetching non-finalized blocks");
        }
        (Phase::Window { fetched, total }, Some(stalled)) => {
            warn!(fetched, total, %stalled, "Non-finalized block fetch stalled");
        }
    }
}

/// Whole units per second
fn per_second(count: u64, over: Duration) -> u64 {
    match over.as_secs_f64() {
        secs if secs > 0.0 => (count as f64 / secs).round() as u64,
        _ => 0,
    }
}

/// Every index directory, off the runtime (an unreadable one logged here, under its component)
async fn walk(indexes: &[Index]) -> Result<Disk, IndexerError> {
    let dirs: Vec<PathBuf> = indexes.iter().map(|index| index.dir.clone()).collect();
    let walked = tokio::task::spawn_blocking(move || {
        dirs.iter().map(|dir| disk_bytes(dir)).collect::<Vec<_>>()
    });
    let mut disk = Disk::new();
    for (index, walked) in indexes.iter().zip(walked.await?) {
        match walked {
            Ok(bytes) => drop(disk.insert(index.kind.name(), bytes)),
            Err(error) => index.span.in_scope(|| warn!(%error, "Index size unreadable")),
        }
    }
    Ok(disk)
}

/// Bytes of every file under `path`, subdirectories included (plain `stat`s, no lock)
///
/// - file removed mid-walk = gone
pub(crate) fn disk_bytes(path: &Path) -> io::Result<u64> {
    let mut total = 0;
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let bytes = match entry.metadata() {
            Ok(meta) if meta.is_dir() => disk_bytes(&entry.path()),
            Ok(meta) => Ok(meta.len()),
            Err(error) => Err(error),
        };
        match bytes {
            Ok(bytes) => total += bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every file counted, nested included; missing directory = error
    #[test]
    fn disk_bytes_sums_every_file_under_the_directory() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::write(root.path().join("MANIFEST"), [0; 10]).expect("manifest");
        std::fs::create_dir(root.path().join("by_hash")).expect("subdir");
        std::fs::write(root.path().join("by_hash/0.seg"), [0; 4096]).expect("segment");
        std::fs::write(root.path().join("by_hash/1.seg"), []).expect("empty segment");

        assert_eq!(disk_bytes(root.path()).expect("walk"), 4106);
        let missing = disk_bytes(&root.path().join("absent")).expect_err("absent");
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);
    }

    /// Final blocks first (handed, else the lowest durable tip), then the window up to best; the
    /// us-west-1 restart (durable = final = anchor 3,515,519, best 1,003 above) = the window
    #[test]
    fn the_phase_is_final_blocks_then_the_non_finalized_window_then_nothing() {
        use zaino_primitives::types::BlockRef;
        use zcash_protocol::BlockHash;
        let at =
            |height: u32| BlockRef { hash: BlockHash([0; 32]), height: BlockHeight::from(height) };
        let tips = |final_tip: u32, served: u32, best: u32, synced: bool| Tips {
            best: Some(at(best)),
            final_tip: Some(at(final_tip)),
            served: Some(at(served)),
            held_by: zaino_chainview::TrustedValidatorSet::default(),
            synced,
        };
        let restart = tips(3_515_519, 3_515_519, 3_516_522, false);
        let cases = [
            (
                "fresh: nothing handed or durable",
                restart,
                None,
                None,
                Some(Phase::Final { done: None, target: 3_515_519 }),
            ),
            (
                "durable below final, follower idle",
                restart,
                None,
                Some(3_515_000),
                Some(Phase::Final { done: Some(3_515_000), target: 3_515_519 }),
            ),
            (
                "handed past the durable tip",
                restart,
                Some(3_515_100),
                Some(3_515_000),
                Some(Phase::Final { done: Some(3_515_100), target: 3_515_519 }),
            ),
            (
                "restart: durable = final, handed none",
                restart,
                None,
                Some(3_515_519),
                Some(Phase::Window { fetched: 0, total: 1_003 }),
            ),
            (
                "window part fetched",
                tips(3_515_519, 3_515_859, 3_516_522, false),
                None,
                Some(3_515_519),
                Some(Phase::Window { fetched: 340, total: 1_003 }),
            ),
            (
                "served = best, synced",
                tips(3_515_522, 3_516_522, 3_516_522, true),
                Some(3_515_522),
                Some(3_515_522),
                None,
            ),
            (
                "synced, a block behind",
                tips(3_515_523, 3_516_522, 3_516_523, true),
                Some(3_515_523),
                Some(3_515_523),
                None,
            ),
        ];
        for (case, tips, handed, durable, expected) in cases {
            assert_eq!(Phase::of(&tips, handed, durable), expected, "{case}");
        }
        let no_chain = Tips { best: None, final_tip: None, ..restart };
        assert_eq!(Phase::of(&no_chain, None, None), None, "no chain yet: nothing to report");
    }

    #[test]
    fn per_second_rounds_and_a_zero_interval_is_zero() {
        assert_eq!(per_second(90, Duration::from_secs(30)), 3);
        assert_eq!(per_second(100, Duration::from_secs(30)), 3);
        assert_eq!(per_second(90, Duration::ZERO), 0);
    }
}
