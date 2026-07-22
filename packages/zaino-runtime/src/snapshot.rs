//! The runtime's composed snapshot.
//!
//! FS (finalised, `≤ watermark`) + a pinned NFS view (recent, `> watermark`),
//! routed by height. This is where the two delegated components meet to satisfy
//! the read capabilities of `zaino-service::Snapshot`.
//!
//! Scaffold: `CompactBlockRead` is wired to show the routing pattern; the rest
//! of the `Snapshot` bundle follows the same shape.

use std::sync::Arc;

use futures::stream::{self, BoxStream, StreamExt};

use zaino_core::{BlockRef, Capability, CompactBlock, Height, HeightRange};
use zaino_fs::error::HeightReadError;
use zaino_fs::FinalisedState;
use zaino_nfs::NfsSnapshot;
use zaino_service::error::{BlockReadError, ReadError};
use zaino_service::CompactBlockRead;

/// A pinned, reorg-coherent view composed from both components. Holds a shared
/// FS handle (finalised reads) + a pinned NFS snapshot (recent reads) + the
/// finalised watermark that splits them.
pub struct RuntimeSnapshot<F, S> {
    pub(crate) fs: Arc<F>,
    pub(crate) nfs: S,
    pub(crate) watermark: Height,
}

// Manual Clone: `Arc<F>` clones regardless of `F`, so we don't want the derive's
// spurious `F: Clone` bound.
impl<F, S: Clone> Clone for RuntimeSnapshot<F, S> {
    fn clone(&self) -> Self {
        Self {
            fs: Arc::clone(&self.fs),
            nfs: self.nfs.clone(),
            watermark: self.watermark,
        }
    }
}

impl<F, S> CompactBlockRead for RuntimeSnapshot<F, S>
where
    F: FinalisedState + 'static,
    S: NfsSnapshot,
{
    async fn compact_block(&self, at: BlockRef) -> Result<Option<CompactBlock>, BlockReadError> {
        match at {
            // Finalised → the FS compact-block index (fallible: backend).
            BlockRef::Height(h) if h <= self.watermark => {
                self.fs.compact_block(h).await.map_err(|e| match e {
                    HeightReadError::AboveWatermark(_) => {
                        BlockReadError::NotServiceable(Capability::Blocks)
                    }
                    HeightReadError::Backend(s) => BlockReadError::Fatal(s),
                })
            }
            // Recent → the pinned NFS window (infallible, in-memory).
            BlockRef::Height(h) => Ok(self.nfs.compact_block(h)),
            // By hash → resolve height (NFS then FS), then route as above.
            BlockRef::Hash(_) => todo!("resolve hash -> height across NFS/FS, then route"),
        }
    }

    fn stream_compact(&self, _range: HeightRange) -> BoxStream<'_, Result<CompactBlock, ReadError>> {
        // TODO: walk the range, routing each height at the watermark
        // (FS index below, NFS Chain above).
        stream::empty().boxed()
    }
}
