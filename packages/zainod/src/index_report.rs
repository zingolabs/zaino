//! One `Index on disk` line per enabled index every [`INTERVAL`] while it bulk syncs: durable
//! tip, bytes on disk, each subdirectory's share; silent while it serves

use std::{
    io,
    path::{Path, PathBuf},
    time::Duration,
};

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{field::display, info, warn};

use zaino_persistence::{dir::disk_bytes, lsm::Size};
use zaino_primitives::types::Height;

use crate::error::IndexerError;

const INTERVAL: Duration = Duration::from_secs(120);

/// Until `cancel`; `finalized` = the index's durable tip, `synced` = its serving gate, `dir` = its
/// directory
pub(crate) async fn run(
    finalized: watch::Receiver<Option<Height>>,
    mut synced: watch::Receiver<bool>,
    dir: PathBuf,
    cancel: CancellationToken,
) -> Result<(), IndexerError> {
    loop {
        let follower_gone = tokio::select! {
            () = cancel.cancelled() => return Ok(()),
            syncing = synced.wait_for(|serving| !serving) => syncing.is_err(),
        };
        if follower_gone {
            cancel.cancelled().await;
            return Ok(());
        }
        let mut ticks = tokio::time::interval(INTERVAL);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticks.tick().await;
        loop {
            tokio::select! {
                () = cancel.cancelled() => return Ok(()),
                _ = synced.wait_for(|serving| *serving) => break,
                _ = ticks.tick() => {}
            }
            let durable = (*finalized.borrow()).map(u32::from);
            let walk = dir.clone();
            match tokio::task::spawn_blocking(move || usage(&walk)).await? {
                Ok(Usage { total, subdirs }) => {
                    let parts = (!subdirs.is_empty()).then(|| {
                        display(crate::logging::parts(
                            subdirs.into_iter().map(|(name, bytes)| (name, Size(bytes))),
                        ))
                    });
                    info!(durable, size = %Size(total), parts, "Index on disk");
                }
                Err(error) => warn!(durable, %error, "Index size unreadable"),
            }
        }
    }
}

/// Bytes under an index directory
#[derive(Debug, PartialEq, Eq)]
struct Usage {
    total: u64,
    /// Each top-level subdirectory's bytes, by name
    subdirs: Vec<(String, u64)>,
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
