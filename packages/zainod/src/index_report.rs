//! One status line per enabled index, with its bytes on disk and each subdirectory's share:
//!
//! - `Index on disk` every [`SYNCING_EVERY`] while it bulk syncs (durable tip)
//! - `Serving index` every [`SERVING_EVERY`] once it serves (durable + applied tips, requests
//!   answered since the last line)

use std::{
    io,
    path::{Path, PathBuf},
    time::Duration,
};

use tokio::{sync::watch, time::Instant};
use tokio_util::sync::CancellationToken;
use tracing::{field::display, info, warn};

use zaino_persistence::{dir::disk_bytes, lsm::Size};
use zaino_primitives::types::Height;
use zaino_sync::Reads;

use crate::error::IndexerError;

const SYNCING_EVERY: Duration = Duration::from_secs(120);
const SERVING_EVERY: Duration = Duration::from_secs(300);

/// What the report reads off one index's `Published`
pub(crate) struct Watched {
    pub(crate) finalized: watch::Receiver<Option<Height>>,
    pub(crate) applied: watch::Receiver<Option<Height>>,
    pub(crate) synced: watch::Receiver<bool>,
    /// `None` = no service reads this index
    pub(crate) reads: Option<Reads>,
}

/// Until `cancel`; `dir` = the index's directory; each walk also lands in `measured` (`/statusz`)
pub(crate) async fn run(
    mut index: Watched,
    dir: PathBuf,
    measured: watch::Sender<Option<Usage>>,
    cancel: CancellationToken,
) -> Result<(), IndexerError> {
    loop {
        let serving = *index.synced.borrow();
        let every = if serving { SERVING_EVERY } else { SYNCING_EVERY };
        let mut ticks = tokio::time::interval_at(Instant::now() + every, every);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut answered = index.reads.as_ref().map(Reads::total);
        loop {
            let gate = tokio::select! {
                () = cancel.cancelled() => return Ok(()),
                moved = index.synced.wait_for(|now| *now != serving) => Some(moved.is_err()),
                _ = ticks.tick() => None,
            };
            match gate {
                // index gone: nothing left to report
                Some(true) => {
                    cancel.cancelled().await;
                    return Ok(());
                }
                Some(false) => break,
                None => {}
            }
            let durable = (*index.finalized.borrow()).map(u32::from);
            let walk = dir.clone();
            let walked = match tokio::task::spawn_blocking(move || usage(&walk)).await? {
                Ok(usage) => usage,
                Err(error) => {
                    warn!(durable, %error, "Index size unreadable");
                    continue;
                }
            };
            measured.send_replace(Some(walked.clone()));
            let Usage { total, subdirs } = walked;
            let size = display(Size(total));
            let parts = (!subdirs.is_empty()).then(|| {
                let shares = subdirs.into_iter().map(|(name, bytes)| (name, Size(bytes)));
                display(crate::logging::parts(shares))
            });
            if !serving {
                info!(durable, size, parts, "Index on disk");
                continue;
            }
            let applied = (*index.applied.borrow()).map(u32::from);
            let now = index.reads.as_ref().map(Reads::total);
            let since = now.zip(answered).map(|(now, before)| now - before);
            answered = now;
            info!(durable, applied, size, parts, requests = since, "Serving index");
        }
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
