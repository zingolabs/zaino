//! Storage core implementing an LSM-Tree Index: the file layer, the manifest commit point,
//! immutable sorted segments, and the offline verifier's report types (`docs/design/durability.md`)

// only `unsafe` = `fs::real::{map_read_only, start_writeback}` (mmap, sync_file_range)
#![deny(unsafe_code)]

use std::{
    error::Error,
    io::{self, ErrorKind},
    path::Path,
};

pub mod dir;
pub mod fs;
pub mod lsm;
pub mod manifest;
pub mod pages;

/// Why an index directory could not be opened, read, proven or committed
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("index io: {0}")]
    Io(#[from] io::Error),

    #[error(transparent)]
    Manifest(#[from] manifest::ManifestError),

    #[error(transparent)]
    Page(#[from] pages::PageError),

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
    use std::panic::{self, AssertUnwindSafe};

    use super::*;

    #[test]
    fn commit_failed_names_the_index_and_disk_and_spots_a_full_disk_at_any_depth() {
        let io = |kind| io::Error::from(kind);
        let cases = [
            (StoreError::Io(io(ErrorKind::StorageFull)), "tb index commit failed: disk /idx full"),
            (
                StoreError::Io(io(ErrorKind::QuotaExceeded)),
                "tb index commit failed: disk /idx full",
            ),
            (
                StoreError::Manifest(manifest::ManifestError::Io(io(ErrorKind::StorageFull))),
                "tb index commit failed: disk /idx full",
            ),
            (
                StoreError::Io(io::Error::other("EIO")),
                "tb index commit failed at /idx: index io: EIO",
            ),
        ];
        for (error, expected) in cases {
            let panicked = panic::catch_unwind(AssertUnwindSafe(|| {
                error.commit_failed("tb", Path::new("/idx"));
            }));
            let payload = panicked.expect_err("commit_failed always panics");
            assert_eq!(payload.downcast_ref::<String>().map(String::as_str), Some(expected));
        }
    }
}
