//! One progress task: every [`REPORT_INTERVAL`], read from the global snapshot + `NfsProgress`
//!
//! - `Syncing blocks` (handed, best, bps, eta) while the blocks handed trail the best; a stall
//!   warning when a whole interval hands over nothing
//! - one `Syncing` line per enabled index (durable, size) while not synced
//! - disk walked at most every [`WALK_EVERY`] (`size` here + `/statusz` `disk`)

use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    time::Duration,
};

use serde::Serialize;
use tokio::{sync::watch, time::Instant};
use tracing::{field::display, info, warn, Span};
use zaino_nfs::NfsProgress;
use zaino_persistence::{disk_bytes, DiskView, IndexKind};
use zaino_snapshot::Snapshots;
use zaino_sync::{ByteSize, Human};

use crate::error::IndexerError;
use crate::logging::HeightCol;

pub(crate) const REPORT_INTERVAL: Duration = Duration::from_secs(30);
const WALK_EVERY: Duration = Duration::from_secs(120);

/// One enabled index: its log component and directory
pub(crate) struct Index {
    pub(crate) kind: IndexKind,
    pub(crate) span: Span,
    pub(crate) dir: PathBuf,
}

/// Bytes under one index directory: its total, each top-level subdirectory's share by name
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Usage {
    pub(crate) size_bytes: u64,
    pub(crate) tables: BTreeMap<String, u64>,
}

/// Last walk per index name (unreadable = absent)
pub(crate) type Disk = BTreeMap<&'static str, Usage>;

/// Blocks handed so far, when
#[derive(Clone, Copy)]
struct Sample {
    at: Instant,
    blocks: u64,
}

/// Until `cancel`; each walk lands in `disk`
pub(crate) async fn run(
    snapshots: Snapshots<DiskView>,
    progress: NfsProgress,
    indexes: Vec<Index>,
    disk: watch::Sender<Disk>,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<(), IndexerError> {
    let mut ticks = tokio::time::interval_at(Instant::now() + REPORT_INTERVAL, REPORT_INTERVAL);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last = Sample { at: Instant::now(), blocks: progress.blocks() };
    let mut walked: Option<Instant> = None;
    loop {
        tokio::select! {
            () = cancel.cancelled() => return Ok(()),
            _ = ticks.tick() => {}
        }
        if walked.is_none_or(|at| at.elapsed() >= WALK_EVERY) {
            disk.send_replace(walk(&indexes).await?);
            walked = Some(Instant::now());
        }
        let now = Sample { at: Instant::now(), blocks: progress.blocks() };
        let snap = snapshots.load();
        let tips = snap.tips();
        if let (Some(best), handed) = (tips.best, progress.handed()) {
            summarise(last, now, handed.map_or(0, u32::from), u32::from(best.height));
        }
        if !tips.synced {
            let durable: Vec<_> =
                snap.indexed().into_iter().flat_map(|indexed| indexed.durable()).collect();
            let sizes = disk.borrow();
            for index in &indexes {
                let tip = durable.iter().find(|(kind, _)| *kind == index.kind);
                let durable = HeightCol(tip.and_then(|(_, tip)| *tip).map(|tip| tip.height.into()));
                let size =
                    sizes.get(index.kind.name()).map(|usage| display(ByteSize(usage.size_bytes)));
                index.span.in_scope(|| info!(%durable, size, "Syncing"));
            }
        }
        last = now;
    }
}

/// One line per interval while handed trails `best`: a rate + eta, or a stall
fn summarise(last: Sample, now: Sample, handed: u32, best: u32) {
    if handed >= best {
        return;
    }
    let elapsed = now.at - last.at;
    let blocks = now.blocks - last.blocks;
    if blocks == 0 {
        warn!(height = handed, target = best, stalled = %Human(elapsed), "Block fetch stalled");
        return;
    }
    let rate = blocks as f64 / elapsed.as_secs_f64();
    let bps = per_second(blocks, elapsed);
    let eta = Human(Duration::from_secs_f64(f64::from(best - handed) / rate));
    info!(height = handed, target = best, bps, eta = %eta, "Syncing blocks");
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
    let walked =
        tokio::task::spawn_blocking(move || dirs.iter().map(|dir| usage(dir)).collect::<Vec<_>>());
    let mut disk = Disk::new();
    for (index, walked) in indexes.iter().zip(walked.await?) {
        match walked {
            Ok(usage) => drop(disk.insert(index.kind.name(), usage)),
            Err(error) => index.span.in_scope(|| warn!(%error, "Index size unreadable")),
        }
    }
    Ok(disk)
}

/// One pass over `dir`: its own files count towards the total only; a file removed mid-walk
/// counts as gone
fn usage(dir: &Path) -> io::Result<Usage> {
    let mut usage = Usage { size_bytes: 0, tables: BTreeMap::new() };
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            let bytes = disk_bytes(&entry.path())?;
            usage.size_bytes += bytes;
            usage.tables.insert(entry.file_name().to_string_lossy().into_owned(), bytes);
            continue;
        }
        match entry.metadata() {
            Ok(meta) => usage.size_bytes += meta.len(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(usage)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Root files count towards the total only; each subdirectory, nested files included, is its
    /// own share, by name
    #[test]
    fn usage_splits_the_total_by_top_level_subdirectory() {
        let root = tempfile::tempdir().expect("tempdir");
        let write = |path: &str, bytes: usize| {
            std::fs::write(root.path().join(path), vec![0; bytes]).expect(path)
        };
        for subdir in ["spent", "receives", "receives/nested"] {
            std::fs::create_dir(root.path().join(subdir)).expect(subdir);
        }
        write("MANIFEST", 10);
        write("spent/0.seg", 300);
        write("receives/0.seg", 100);
        write("receives/nested/1.seg", 20);

        let tables = [("receives".to_owned(), 120), ("spent".to_owned(), 300)];
        let expected = Usage { size_bytes: 430, tables: tables.into_iter().collect() };
        assert_eq!(usage(root.path()).expect("walk"), expected);
    }

    #[test]
    fn per_second_rounds_and_a_zero_interval_is_zero() {
        assert_eq!(per_second(90, Duration::from_secs(30)), 3);
        assert_eq!(per_second(100, Duration::from_secs(30)), 3);
        assert_eq!(per_second(90, Duration::ZERO), 0);
    }
}
