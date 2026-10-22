//! Why the store failed, and how each failure reaches the port's errors

use std::{
    any::Any,
    error::Error,
    io::{self, ErrorKind},
    panic::{self, AssertUnwindSafe},
};

use zaino_persistence::{CommitError, FootprintError, OpenError, ReadError, VerifyError};

use crate::{lsm::SegmentError, manifest::ManifestError, pages::PageError};

/// Why an index directory could not be opened, read, proven or committed
#[derive(Debug, thiserror::Error)]
pub(crate) enum StoreError {
    #[error("index io: {0}")]
    Io(#[from] io::Error),

    #[error(transparent)]
    Manifest(#[from] ManifestError),

    #[error(transparent)]
    Page(#[from] PageError),

    #[error(transparent)]
    Segment(#[from] SegmentError),

    /// The directory was committed with other namespaces
    #[error("the directory holds other namespaces: {0}")]
    Specs(String),

    /// A namespace (or its tuning) the store cannot hold
    #[error("namespace {name}: {reason}")]
    Declared { name: String, reason: &'static str },

    /// The store's own state broke a precondition (a bug in this crate)
    #[error("store: {0}")]
    Internal(&'static str),

    #[error("{0} overflows")]
    Overflow(&'static str),

    /// Code under a guard panicked (never this crate's: it has no panicking code)
    #[error("panicked: {0}")]
    Panicked(String),
}

impl StoreError {
    /// Any cause in the chain = the disk or quota is full
    pub(crate) fn disk_full(&self) -> bool {
        self.chain().any(|error| {
            let kind = error.downcast_ref::<io::Error>().map(io::Error::kind);
            matches!(
                kind,
                Some(ErrorKind::StorageFull | ErrorKind::QuotaExceeded)
            )
        })
    }

    /// Stored bytes fail their integrity check (stop, verify, resync)
    ///
    /// - matched, not walked: `transparent` variants leave themselves out of the source chain
    pub(crate) fn corrupted(&self) -> bool {
        match self {
            Self::Page(error) => page_corrupted(error),
            Self::Segment(error) => segment_corrupted(error),
            _ => false,
        }
    }

    fn chain(&self) -> impl Iterator<Item = &(dyn Error + 'static)> {
        let mut next: Option<&(dyn Error + 'static)> = Some(self);
        std::iter::from_fn(move || {
            let current = next?;
            next = current.source();
            Some(current)
        })
    }

    pub(crate) fn into_read(self, operation: &'static str) -> ReadError {
        match self.corrupted() {
            true => ReadError::Corrupted {
                operation,
                source: Box::new(self),
            },
            false => ReadError::ReadFailed {
                operation,
                source: Box::new(self),
            },
        }
    }

    /// A background merge failed for a cause a retry can clear, found before the write touched
    /// anything (never a poison; a corrupt input is [`corrupted`](Self::corrupted))
    pub(crate) fn merge_failed(&self) -> bool {
        matches!(self, Self::Segment(SegmentError::MergeFailed { .. })) && !self.corrupted()
    }

    pub(crate) fn into_commit(self, operation: &'static str) -> CommitError {
        match self {
            error if error.corrupted() => CommitError::Corrupted(Box::new(error)),
            error if error.merge_failed() => CommitError::MergeFailed(Box::new(error)),
            _ if self.disk_full() => CommitError::OutOfSpace,
            Self::Manifest(ManifestError::Full { .. }) => CommitError::ManifestFull,
            error => CommitError::WriteFailed {
                operation,
                source: Box::new(error),
            },
        }
    }

    pub(crate) fn into_open(self, operation: &'static str) -> OpenError {
        OpenError::Unavailable {
            operation,
            source: Box::new(self),
        }
    }

    pub(crate) fn into_verify(self, operation: &'static str) -> VerifyError {
        VerifyError::Failed {
            operation,
            source: Box::new(self),
        }
    }

    pub(crate) fn into_footprint(self, operation: &'static str) -> FootprintError {
        FootprintError::Failed {
            operation,
            source: Box::new(self),
        }
    }
}

fn page_corrupted(error: &PageError) -> bool {
    matches!(
        error,
        PageError::Lost { .. }
            | PageError::Tail { .. }
            | PageError::Sums { .. }
            | PageError::Corrupt { .. }
    )
}

/// A merge's failure is corrupt when its cause is (a retry reads the same bytes)
fn segment_corrupted(error: &SegmentError) -> bool {
    match error {
        SegmentError::Page(error) => page_corrupted(error),
        SegmentError::Corrupt { .. } | SegmentError::Contract { .. } => true,
        SegmentError::MergeFailed { source, .. } => segment_corrupted(source),
        _ => false,
    }
}

/// `run`'s result, a panic inside it caught as [`StoreError::Panicked`] (this crate has no
/// panicking code: a std or dependency bug, or a caller's closure)
pub(crate) fn guarded<R>(run: impl FnOnce() -> R) -> Result<R, StoreError> {
    panic::catch_unwind(AssertUnwindSafe(run)).map_err(|payload| panicked(&*payload))
}

fn panicked(payload: &(dyn Any + Send)) -> StoreError {
    let message = payload
        .downcast_ref::<&str>()
        .map(|message| (*message).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a non-string payload".to_owned());
    StoreError::Panicked(message)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::lsm::Breach;

    #[test]
    fn a_full_disk_is_spotted_at_any_depth() {
        let io = |kind| io::Error::from(kind);
        let full = [
            StoreError::Io(io(ErrorKind::StorageFull)),
            StoreError::Io(io(ErrorKind::QuotaExceeded)),
            StoreError::Manifest(ManifestError::Io(io(ErrorKind::StorageFull))),
            StoreError::Page(PageError::Io(io(ErrorKind::StorageFull))),
        ];
        for error in full {
            assert!(error.disk_full(), "{error}");
            assert!(matches!(
                error.into_commit("write"),
                CommitError::OutOfSpace
            ));
        }
        let other = StoreError::Io(io::Error::other("EIO"));
        assert!(!other.disk_full());
        assert!(matches!(
            other.into_commit("write"),
            CommitError::WriteFailed {
                operation: "write",
                ..
            }
        ));
        let full = StoreError::Manifest(ManifestError::Full { body: 1 });
        assert!(matches!(
            full.into_commit("write"),
            CommitError::ManifestFull
        ));
    }

    #[test]
    fn integrity_failures_read_as_corrupted_others_as_failed() {
        let corrupt = StoreError::Segment(SegmentError::Page(PageError::Corrupt {
            path: PathBuf::from("/x"),
            page: 3,
        }));
        assert!(matches!(
            corrupt.into_read("get"),
            ReadError::Corrupted {
                operation: "get",
                ..
            }
        ));
        let io = StoreError::Io(io::Error::other("EIO"));
        assert!(matches!(io.into_read("get"), ReadError::ReadFailed { .. }));
    }

    #[test]
    fn a_merge_failed_on_corrupt_input_is_corrupted_others_are_merge_failed() {
        let merge = |source: SegmentError| {
            StoreError::Segment(SegmentError::MergeFailed {
                name: "scanned".to_owned(),
                source: Box::new(source),
            })
        };
        let corrupt = [
            SegmentError::Page(PageError::Corrupt {
                path: PathBuf::from("/x"),
                page: 3,
            }),
            SegmentError::Contract {
                segments: vec![1, 2],
                breach: Breach::Repeated { held: 2 },
            },
        ];
        for source in corrupt {
            let error = merge(source);
            assert!(error.corrupted() && !error.merge_failed(), "{error}");
            assert!(matches!(
                error.into_commit("write"),
                CommitError::Corrupted(_)
            ));
        }
        let transient = merge(SegmentError::Page(PageError::Io(io::Error::other("EIO"))));
        assert!(transient.merge_failed() && !transient.corrupted());
        assert!(matches!(
            transient.into_commit("write"),
            CommitError::MergeFailed(_)
        ));
    }

    #[test]
    fn a_panic_under_the_guard_is_an_error_naming_it() {
        let empty: &[u8] = &[];
        let caught = guarded(|| empty.get(1).copied().expect("index out of bounds"));
        let message = caught.expect_err("caught").to_string();
        assert!(message.contains("index out of bounds"), "{message}");
        assert_eq!(guarded(|| 7).expect("no panic"), 7);
    }
}
