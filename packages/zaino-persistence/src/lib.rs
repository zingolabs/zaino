//! What every Zaino index stores through: persistence port (`port.rs`) + its engine,
//! [`DiskEngine`] (guide: `usage.md`)

// only `unsafe` = `fs::real::{map_read_only, start_writeback, resident_bytes}` (mmap,
// sync_file_range, mincore)
#![deny(unsafe_code)]

use std::{
    error::Error,
    io::{self, ErrorKind},
    path::Path,
};

mod dir;
mod disk;
mod emit;
pub mod fs;
pub mod lsm;
mod manifest;
mod overlay;
mod pages;
mod port;
mod sequence;
mod write_buffer;

pub use dir::disk_bytes;
pub use disk::{DiskEngine, DiskStore, DiskView, TableFootprint};
pub use emit::{describe_metrics, METRIC_BUCKETS};
pub use lsm::LsmConfig;
pub use manifest::{IndexKind, ManifestError};
pub use overlay::{Entry, Overlay, OverlayView, Uncommitted};
pub use pages::PageError;
pub use port::{
    BlockChanges, Checked, CommittedView, MapChanges, MapId, MapRead, MapTable, MapView,
    PersistenceEngine, Schema, SequenceAppends, SequenceId, SequenceRead, SequenceTable,
    SequenceView, Store, Tables, Verification, View, Width,
};
pub use write_buffer::{StagedView, WriteBuffer};

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

impl StoreError {
    /// Failed commit → crash naming the disk (store unusable after one, recovery = reopen)
    pub fn commit_failed(&self, index: &str, dir: &Path) -> ! {
        if self.disk_full() {
            panic!("{index} index commit failed: disk {} full", dir.display());
        }
        panic!("{index} index commit failed at {}: {self}", dir.display());
    }

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
    #[should_panic(expected = "tb index commit failed: disk /idx full")]
    fn a_commit_failed_on_a_full_disk_names_the_disk() {
        StoreError::Io(io::Error::from(ErrorKind::StorageFull))
            .commit_failed("tb", Path::new("/idx"));
    }

    #[test]
    #[should_panic(expected = "tb index commit failed at /idx: index io: EIO")]
    fn a_commit_failed_otherwise_names_the_cause() {
        StoreError::Io(io::Error::other("EIO")).commit_failed("tb", Path::new("/idx"));
    }
}
