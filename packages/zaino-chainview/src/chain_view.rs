//! The composer: pairs a finalised store with a non-finalised head, both named
//! only through the shared `zaino-service` ports.

use std::future::Future;

use zaino_service::error::Transient;
use zaino_service::{ChainSegment, CompactBlockRead, TakeSnapshot};

use crate::snapshot::ChainViewSnapshot;

/// Composes a finalised store `Fs` and a non-finalised head `Nfs` into one
/// served chain for compact-block serving.
///
/// Both sides are named through the same shared ports — each is a
/// [`TakeSnapshot`] whose snapshot is a [`ChainSegment`] (coherence coordinate)
/// and a [`CompactBlockRead`] (compact-block serving). The composer assigns the
/// roles by slot: `fs` is the durable prefix, `nfs` the volatile suffix.
///
/// [`snapshot`](TakeSnapshot::snapshot) captures both snapshots **together**, in
/// one shot, so the seam watermark and the volatile window are coherent — a read
/// through the resulting [`ChainViewSnapshot`] never sees a watermark from one
/// instant and a window from another.
pub struct ChainView<Fs, Nfs> {
    /// The finalised store, taken as of each snapshot. Serves the durable
    /// prefix up to its watermark.
    fs: Fs,
    /// The non-finalised head, pinned into each snapshot. Serves the volatile
    /// window above the watermark.
    nfs: Nfs,
}

impl<Fs, Nfs> ChainView<Fs, Nfs>
where
    Fs: TakeSnapshot,
    Fs::Snapshot: ChainSegment + CompactBlockRead,
    Nfs: TakeSnapshot,
    Nfs::Snapshot: ChainSegment + CompactBlockRead,
{
    /// Compose over a finalised store and a non-finalised head.
    pub fn new(fs: Fs, nfs: Nfs) -> Self {
        Self { fs, nfs }
    }
}

// A composer is only as clonable as the two handles it holds, both of which are
// cheap `Arc`-backed clones in practice. Derived by hand rather than with
// `#[derive(Clone)]` so no `Clone` bound leaks onto the `TakeSnapshot` impls —
// the composition needs no `Clone`, only the served facade does.
impl<Fs: Clone, Nfs: Clone> Clone for ChainView<Fs, Nfs> {
    fn clone(&self) -> Self {
        Self {
            fs: self.fs.clone(),
            nfs: self.nfs.clone(),
        }
    }
}

impl<Fs, Nfs> TakeSnapshot for ChainView<Fs, Nfs>
where
    Fs: TakeSnapshot,
    Fs::Snapshot: ChainSegment + CompactBlockRead,
    Nfs: TakeSnapshot,
    Nfs::Snapshot: ChainSegment + CompactBlockRead,
{
    type Snapshot = ChainViewSnapshot<Fs::Snapshot, Nfs::Snapshot>;

    fn snapshot(&self) -> impl Future<Output = Result<Self::Snapshot, Transient>> + Send {
        // Capture both snapshots together — one shot — so the watermark the FS
        // pins and the window the NFS pins are the same instant's coordinates.
        let fs = self.fs.snapshot();
        let nfs = self.nfs.snapshot();
        async move { Ok(ChainViewSnapshot::new(fs.await?, nfs.await?)) }
    }
}
