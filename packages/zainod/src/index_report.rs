//! One line per enabled index: `durable` / `merged` / `applied` heights + bytes on disk
//!
//! - `Syncing` every [`REPORT_EVERY`] in bulk sync (no window above `durable`)
//! - `Committed bulk` once, as the window first opens over a bulk pass (the handoff flush)
//! - `Serving` per serving-gate opening
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

use zaino_persistence::dir::disk_bytes;
use zaino_primitives::types::Height;
use zaino_sync::Reads;

use crate::error::IndexerError;
use crate::logging::{HeightCol, Size3};

/// = the sync report's interval (index lines land under its `Syncing blocks`)
const REPORT_EVERY: Duration = Duration::from_secs(30);
const WALK_EVERY: Duration = Duration::from_secs(120);

/// What the report reads off one index's `Published`
pub(crate) struct Watched {
    pub(crate) finalized: watch::Receiver<Option<Height>>,
    pub(crate) applied: watch::Receiver<Option<Height>>,
    pub(crate) merged: watch::Receiver<Option<Height>>,
    pub(crate) synced: watch::Receiver<bool>,
    /// `None` = no service reads this index
    pub(crate) reads: Option<Reads>,
}

impl Watched {
    fn heights(&self) -> (Option<Height>, Option<Height>, Option<Height>) {
        (*self.finalized.borrow(), *self.merged.borrow(), *self.applied.borrow())
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
    let mut ticks = tokio::time::interval_at(Instant::now() + REPORT_EVERY, REPORT_EVERY);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut walked = Walked(None);
    let booted = *index.finalized.borrow();
    // bulked = durable moved / a batch merged with no window open (a bulk pass ran)
    let (mut bulked, mut committed) = (false, false);
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
                    let (durable, _, applied) = index.heights();
                    info!(
                        durable = %HeightCol(durable.map(u32::from)),
                        applied = %HeightCol(applied.map(u32::from)),
                        size,
                        "Serving"
                    );
                }
                serving = now;
            }
            changed = index.applied.changed() => {
                if changed.is_err() {
                    return gone(&cancel).await;
                }
                index.applied.borrow_and_update();
                let (durable, merged, applied) = index.heights();
                let window = applied > durable;
                bulked |= !window && (merged.is_some() || durable > booted);
                if window && bulked && !committed {
                    committed = true;
                    let size = walked.size(&dir, &measured, WALK_EVERY).await?;
                    info!(durable = %HeightCol(durable.map(u32::from)), size, "Committed bulk");
                }
            }
            _ = ticks.tick() => {
                let size = walked.size(&dir, &measured, WALK_EVERY).await?;
                let (durable, merged, applied) = index.heights();
                let window = applied > durable;
                bulked |= !window && (merged.is_some() || durable > booted);
                if !serving && !window {
                    info!(
                        durable = %HeightCol(durable.map(u32::from)),
                        merged = %HeightCol(merged.map(u32::from)),
                        applied = %HeightCol(applied.map(u32::from)),
                        size,
                        "Syncing"
                    );
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
    ) -> Result<Option<DisplayValue<Size3>>, IndexerError> {
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
        Ok(self.0.and_then(|(_, total)| total).map(|total| display(Size3(total))))
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
