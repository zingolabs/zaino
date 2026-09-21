//! The composer: pairs a finalised store with a non-finalised view.

use std::future::Future;

use zaino_service::error::Transient;
use zaino_service::{CompactBlockRead, Snapshot, TakeSnapshot};

use crate::snapshot::ChainViewSnapshot;
use crate::view::NonFinalisedView;

/// Composes a finalised store `Fs` and a non-finalised view `Nfs` into one
/// served chain for compact-block serving.
///
/// [`snapshot`](TakeSnapshot::snapshot) captures the FS snapshot and the NFS
/// view **together**, in one shot, so the seam watermark and the volatile
/// window are coherent — a read through the resulting [`ChainViewSnapshot`]
/// never sees a watermark from one instant and a window from another.
pub struct ChainView<Fs, Nfs> {
    /// The finalised store, taken as of each snapshot. Serves the durable
    /// prefix up to its watermark.
    fs: Fs,
    /// The non-finalised view, cloned into each snapshot. Serves the volatile
    /// window above the watermark.
    nfs: Nfs,
}

impl<Fs, Nfs> ChainView<Fs, Nfs>
where
    Fs: TakeSnapshot,
    Fs::Snapshot: Snapshot + CompactBlockRead,
    Nfs: NonFinalisedView + Clone + 'static,
{
    /// Compose over a finalised store and a non-finalised view.
    pub fn new(fs: Fs, nfs: Nfs) -> Self {
        Self { fs, nfs }
    }
}

impl<Fs, Nfs> TakeSnapshot for ChainView<Fs, Nfs>
where
    Fs: TakeSnapshot,
    Fs::Snapshot: Snapshot + CompactBlockRead,
    Nfs: NonFinalisedView + Clone + 'static,
{
    type Snapshot = ChainViewSnapshot<Fs::Snapshot, Nfs>;

    fn snapshot(&self) -> impl Future<Output = Result<Self::Snapshot, Transient>> + Send {
        // Capture the FS snapshot and the NFS view together — one shot — so the
        // watermark pinned by the FS and the window held by the NFS are the same
        // instant's coordinates.
        let fs = self.fs.snapshot();
        let nfs = self.nfs.clone();
        async move { Ok(ChainViewSnapshot::new(fs.await?, nfs)) }
    }
}
