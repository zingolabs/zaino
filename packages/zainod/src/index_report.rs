//! One line per enabled index: its committed (`durable`) height + bytes on disk
//!
//! - `Syncing` every [`REPORT_INTERVAL`] (lands under the NFS's `Syncing blocks`) while the NFS
//!   serves behind the verified tip
//! - `Serving` once it serves the tip
//! - Disk walked at most every [`WALK_EVERY`] (`size` here + `/statusz` usage)

use std::{
    io,
    path::{Path, PathBuf},
    time::Duration,
};

use tokio::{sync::watch, time::Instant};
use tokio_util::sync::CancellationToken;
use tracing::{
    field::{display, DisplayValue},
    info, warn,
};

use zaino_persistence::{disk_bytes, DiskView, View as _};

use zaino_nfs::REPORT_INTERVAL;
use zaino_sync::ByteSize;

use crate::error::IndexerError;
use crate::logging::HeightCol;

const WALK_EVERY: Duration = Duration::from_secs(120);

/// What the report reads: the writer's committed view, the NFS's serving judgement
pub(crate) struct Watched {
    pub(crate) committed: watch::Receiver<DiskView>,
    pub(crate) synced: watch::Receiver<bool>,
}

impl Watched {
    fn durable(&self) -> HeightCol {
        HeightCol(self.committed.borrow().tip().map(|tip| u32::from(tip.height)))
    }
}

/// Last disk walk: when, and its bytes (`None` = unreadable)
struct Walked(Option<(Instant, Option<u64>)>);

/// Until `cancel`; `dir` = the index's directory; each walk also lands in `measured` (`/statusz`)
pub(crate) async fn run(
    mut index: Watched,
    dir: PathBuf,
    measured: watch::Sender<Option<Usage>>,
    cancel: CancellationToken,
) -> Result<(), IndexerError> {
    let mut ticks = tokio::time::interval_at(Instant::now() + REPORT_INTERVAL, REPORT_INTERVAL);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut walked = Walked(None);
    let mut serving = *index.synced.borrow();
    loop {
        tokio::select! {
            () = cancel.cancelled() => return Ok(()),
            changed = index.synced.changed() => {
                if changed.is_err() {
                    return gone(&cancel).await;
                }
                let now = *index.synced.borrow_and_update();
                if now && !serving {
                    let size = walked.size(&dir, &measured, Duration::ZERO).await?;
                    info!(durable = %index.durable(), size, "Serving");
                }
                serving = now;
            }
            _ = ticks.tick() => {
                let size = walked.size(&dir, &measured, WALK_EVERY).await?;
                if !serving {
                    info!(durable = %index.durable(), size, "Syncing");
                }
            }
        }
    }
}

/// Index gone: nothing left to report
async fn gone(cancel: &CancellationToken) -> Result<(), IndexerError> {
    cancel.cancelled().await;
    Ok(())
}

impl Walked {
    /// Bytes on disk, re-walked once the last walk is older than `fresh` (`None` = unreadable)
    async fn size(
        &mut self,
        dir: &Path,
        measured: &watch::Sender<Option<Usage>>,
        fresh: Duration,
    ) -> Result<Option<DisplayValue<ByteSize>>, IndexerError> {
        let stale = self.0.is_none_or(|(at, _)| at.elapsed() >= fresh);
        if stale {
            let walk = dir.to_path_buf();
            let total = match tokio::task::spawn_blocking(move || usage(&walk)).await? {
                Ok(walked) => {
                    let total = walked.total;
                    measured.send_replace(Some(walked));
                    Some(total)
                }
                Err(error) => {
                    warn!(%error, "Index size unreadable");
                    None
                }
            };
            self.0 = Some((Instant::now(), total));
        }
        Ok(self.0.and_then(|(_, total)| total).map(|total| display(ByteSize(total))))
    }
}

/// Bytes under an index directory
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Usage {
    pub(crate) total: u64,
    /// Each top-level subdirectory's bytes, by name
    pub(crate) subdirs: Vec<(String, u64)>,
}

/// One pass over `dir`: its own files count towards `total` only; a file removed mid-walk counts
/// as gone
fn usage(dir: &Path) -> io::Result<Usage> {
    let mut usage = Usage { total: 0, subdirs: Vec::new() };
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            let bytes = disk_bytes(&entry.path())?;
            usage.total += bytes;
            usage.subdirs.push((entry.file_name().to_string_lossy().into_owned(), bytes));
            continue;
        }
        match entry.metadata() {
            Ok(meta) => usage.total += meta.len(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    usage.subdirs.sort();
    Ok(usage)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Root files count towards the total only; each subdirectory, nested files included, is its
    /// own share, in name order
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

        let expected = Usage {
            total: 430,
            subdirs: vec![("receives".to_owned(), 120), ("spent".to_owned(), 300)],
        };
        assert_eq!(usage(root.path()).expect("walk"), expected);
    }
}
