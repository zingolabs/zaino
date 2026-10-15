//! What every Zaino index stores through: persistence port (`port.rs`) + its engine,
//! [`DiskEngine`] (guide: `usage.md`)

// only `unsafe`: `fs::real` (mmap, sync_file_range, mincore), `fs::probe` (FIEMAP ioctl),
// `thread` (ioprio_set, setpriority)
#![deny(unsafe_code)]

use std::{
    error::Error,
    fmt,
    io::{self, ErrorKind},
    path::{Path, PathBuf},
};

mod dir;
mod disk;
mod emit;
pub mod fs;
mod lsm;
mod manifest;
mod overlay;
mod pages;
mod port;
mod sequence;
mod thread;
mod write_buffer;

pub use disk::{manifest_fits, DiskEngine, DiskStore, DiskView, TableFootprint};
pub use emit::{describe_metrics, METRIC_BUCKETS};
pub use lsm::{Fanout, LsmConfig, SegmentError};
pub use manifest::{IndexKind, ManifestError};
pub use overlay::{AppliedLayer, Entry, Overlay, OverlayView};
pub use pages::PageError;
pub use port::{
    BlockChanges, Checked, MapChanges, MapId, MapRead, MapTable, MapView, PersistenceEngine,
    PinnedView, Schema, SequenceAppends, SequenceId, SequenceRead, SequenceTable, SequenceView,
    Store, Tables, Verification, View, Width,
};
pub use write_buffer::{AppliedView, WriteBuffer};

/// Why an index directory could not be opened, read, proven or committed
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("index io: {0}")]
    Io(#[from] io::Error),

    #[error(transparent)]
    Manifest(#[from] ManifestError),

    #[error(transparent)]
    Page(#[from] PageError),

    #[error(transparent)]
    Segment(#[from] lsm::SegmentError),
}

/// Failed commit of one index's store (store unusable after one, recovery = reopen)
#[derive(Debug)]
pub struct CommitFailed {
    index: &'static str,
    dir: PathBuf,
    cause: StoreError,
}

impl CommitFailed {
    pub(crate) fn new(index: &'static str, dir: &Path, cause: StoreError) -> Self {
        Self { index, dir: dir.to_path_buf(), cause }
    }

    /// Crash naming the index and its disk
    pub fn crash(self) -> ! {
        panic!("{self}")
    }
}

impl fmt::Display for CommitFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (index, dir) = (self.index, self.dir.display());
        match self.cause.disk_full() {
            true => write!(f, "{index} index commit failed: disk {dir} full"),
            false => write!(f, "{index} index commit failed at {dir}: {}", self.cause),
        }
    }
}

impl Error for CommitFailed {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.cause)
    }
}

impl StoreError {
    fn disk_full(&self) -> bool {
        let mut cause: Option<&(dyn Error + 'static)> = Some(self);
        while let Some(error) = cause {
            let kind = error.downcast_ref::<io::Error>().map(io::Error::kind);
            if matches!(kind, Some(ErrorKind::StorageFull | ErrorKind::QuotaExceeded)) {
                return true;
            }
            cause = error.source();
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_disk_is_spotted_at_any_depth_of_the_error() {
        let io = |kind| io::Error::from(kind);
        let cases = [
            (StoreError::Io(io(ErrorKind::StorageFull)), true),
            (StoreError::Io(io(ErrorKind::QuotaExceeded)), true),
            (StoreError::Manifest(manifest::ManifestError::Io(io(ErrorKind::StorageFull))), true),
            (StoreError::Io(io::Error::other("EIO")), false),
        ];
        for (error, full) in cases {
            assert_eq!(error.disk_full(), full, "{error:?}");
        }
    }

    #[test]
    fn a_failed_commit_names_the_index_and_its_disk_or_its_cause() {
        let full = StoreError::Io(io::Error::from(ErrorKind::StorageFull));
        let failed = CommitFailed::new("tb", Path::new("/idx"), full);
        assert_eq!(failed.to_string(), "tb index commit failed: disk /idx full");
        let eio =
            CommitFailed::new("tb", Path::new("/idx"), StoreError::Io(io::Error::other("EIO")));
        assert_eq!(eio.to_string(), "tb index commit failed at /idx: index io: EIO");
    }

    #[test]
    #[should_panic(expected = "tb index commit failed at /idx: index io: EIO")]
    fn crash_panics_with_the_failure() {
        CommitFailed::new("tb", Path::new("/idx"), StoreError::Io(io::Error::other("EIO"))).crash();
    }
}
