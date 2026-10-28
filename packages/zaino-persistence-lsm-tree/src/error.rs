//! Why the store failed, and how each failure reaches the port's errors

use std::{
    any::Any,
    error::Error,
    io::{self, ErrorKind},
    panic::{self, AssertUnwindSafe},
};

use zaino_persistence::{CommitError, FootprintError, OpenError, ReadError, VerifyError};

use crate::{
    lsm::{MergeFault, SegmentError},
    manifest::ManifestError,
    pages::PageError,
};

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

    /// An `LsmConfig` setting out of range
    #[error("lsm config: {0}")]
    Config(&'static str),

    /// The index's thread pool could not start (threads refused: limits, memory)
    #[error("thread pool: {0}")]
    Pool(#[from] rayon::ThreadPoolBuildError),

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
    fn disk_full(&self) -> bool {
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
    fn corrupted(&self) -> bool {
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

    /// A background merge failed for a cause a retry can clear, within its map's budget, found
    /// before the write touched anything (the one write error that never poisons)
    pub(crate) fn merge_failed(&self) -> bool {
        matches!(
            self,
            Self::Segment(SegmentError::MergeFailed {
                fault: MergeFault::Transient,
                ..
            })
        )
    }

    pub(crate) fn into_commit(self, operation: &'static str) -> CommitError {
        match self {
            error if error.corrupted() => CommitError::Corrupted(Box::new(error)),
            error if error.merge_failed() => CommitError::MergeFailed(Box::new(error)),
            error if error.disk_full() => CommitError::OutOfSpace(Box::new(error)),
            Self::Manifest(ManifestError::Full { .. }) => CommitError::ManifestFull,
            error => CommitError::WriteFailed {
                operation,
                source: Box::new(error),
            },
        }
    }

    /// Written as another index = `Mismatch`; committed data lost or corrupt = `Corrupted`
    pub(crate) fn into_open(self, operation: &'static str) -> OpenError {
        let mismatch = matches!(
            self,
            Self::Specs(_)
                | Self::Manifest(
                    ManifestError::Name { .. }
                        | ManifestError::Format { .. }
                        | ManifestError::Magic
                        | ManifestError::Layout { .. }
                )
        );
        let corrupted = self.corrupted()
            || matches!(
                self,
                Self::Manifest(
                    ManifestError::Checksum
                        | ManifestError::Sequence { .. }
                        | ManifestError::BodyLength { .. }
                        | ManifestError::Body(_)
                        | ManifestError::Unmanifested { .. }
                )
            );
        match (mismatch, corrupted) {
            (true, _) => OpenError::Mismatch(Box::new(self)),
            (false, true) => OpenError::Corrupted(Box::new(self)),
            (false, false) => OpenError::Unavailable {
                operation,
                source: Box::new(self),
            },
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

/// Stored bytes found corrupt (a merge: its input, as its fault says; a merge's own output read
/// back wrong is not stored data)
fn segment_corrupted(error: &SegmentError) -> bool {
    match error {
        SegmentError::Page(error) => page_corrupted(error),
        SegmentError::Corrupt { .. } | SegmentError::Contract { .. } => true,
        SegmentError::MergeFailed { fault, .. } => *fault == MergeFault::Corrupt,
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
                CommitError::OutOfSpace(_)
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

    /// Each merge failure's fault, and the commit error it ends as: corrupt input = `Corrupted`;
    /// a sync, a bad read-back or a bug = `WriteFailed`; other I/O = `MergeFailed` (transient)
    /// until escalated, then `OutOfSpace` on a full disk
    #[test]
    fn a_merge_failure_is_answered_by_its_fault() {
        let merge = |fault, source: SegmentError| {
            StoreError::Segment(SegmentError::MergeFailed {
                name: "scanned".to_owned(),
                fault,
                source: Box::new(source),
            })
        };
        let corrupt_page = || {
            SegmentError::Page(PageError::Corrupt {
                path: PathBuf::from("/x"),
                page: 3,
            })
        };
        let eio = || io::Error::other("EIO");
        let cases = [
            (corrupt_page(), MergeFault::Corrupt),
            (
                SegmentError::Contract {
                    segments: vec![1, 2],
                    breach: Breach::Repeated { held: 2 },
                },
                MergeFault::Corrupt,
            ),
            (SegmentError::Io(eio()), MergeFault::Transient),
            (
                SegmentError::Page(PageError::Io(eio())),
                MergeFault::Transient,
            ),
            (SegmentError::Sync(eio()), MergeFault::Fatal),
            (
                SegmentError::ReadBack {
                    segment: 1,
                    source: Box::new(corrupt_page()),
                },
                MergeFault::Fatal,
            ),
            (SegmentError::Internal("bug"), MergeFault::Fatal),
            (
                SegmentError::MergePanicked {
                    name: "scanned".to_owned(),
                },
                MergeFault::Fatal,
            ),
        ];
        for (source, fault) in cases {
            let label = format!("{source}");
            assert_eq!(MergeFault::of(&source), fault, "{label}");
            let error = merge(fault, source);
            let answer = error.into_commit("write");
            let matched = match fault {
                MergeFault::Corrupt => matches!(answer, CommitError::Corrupted(_)),
                MergeFault::Fatal => matches!(answer, CommitError::WriteFailed { .. }),
                MergeFault::Transient => matches!(answer, CommitError::MergeFailed(_)),
            };
            assert!(matched, "{label}: {answer:?}");
        }
        let escalated = merge(
            MergeFault::Fatal,
            SegmentError::Io(io::Error::from(ErrorKind::StorageFull)),
        );
        assert!(!escalated.merge_failed(), "past its budget: not transient");
        assert!(matches!(
            escalated.into_commit("write"),
            CommitError::OutOfSpace(_)
        ));
    }

    /// Open refusals typed: another index = `Mismatch`, lost or corrupt data = `Corrupted`
    #[test]
    fn open_refusals_are_typed() {
        let mismatched = [
            StoreError::Specs("other namespaces".to_owned()),
            StoreError::Manifest(ManifestError::Magic),
            StoreError::Manifest(ManifestError::Layout { len: 1 }),
        ];
        for error in mismatched {
            assert!(matches!(error.into_open("open"), OpenError::Mismatch(_)));
        }
        let corrupted = [
            StoreError::Manifest(ManifestError::Checksum),
            StoreError::Page(PageError::Lost {
                path: PathBuf::from("/x"),
                have: 1,
                need: 2,
            }),
        ];
        for error in corrupted {
            assert!(matches!(error.into_open("open"), OpenError::Corrupted(_)));
        }
        let other = StoreError::Io(io::Error::other("EIO")).into_open("open");
        assert!(matches!(other, OpenError::Unavailable { .. }));
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
