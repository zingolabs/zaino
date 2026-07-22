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

use zaino_core::{
    AddressBalance, AddressDelta, BlockRef, Capability, CompactBlock, Height, HeightRange,
    TransactionHash, TransparentAddress, Utxo,
};
use zaino_fs::error::{AddressReadError as FsAddressReadError, HeightReadError};
use zaino_fs::FinalisedState;
use zaino_nfs::NfsSnapshot;
use zaino_service::error::{AddressReadError as SvcAddressReadError, BlockReadError, ReadError};
use zaino_service::{AddressRead, CompactBlockRead};

/// A pinned, reorg-coherent view composed from both components. Holds a shared
/// FS handle (finalised reads) + a pinned NFS snapshot (recent reads) + the
/// finalised watermark that splits them.
pub struct RuntimeSnapshot<F, S> {
    pub(crate) fs: Arc<F>,
    /// `None` while the NFS window is still syncing — recent reads are then
    /// `NotServiceable`, never a false `None`.
    pub(crate) nfs: Option<S>,
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
            // Recent → the pinned NFS window (infallible, in-memory) when the
            // window is ready; `NotServiceable` while it is still syncing.
            BlockRef::Height(h) => match &self.nfs {
                Some(nfs) => Ok(nfs.compact_block(h)),
                None => Err(BlockReadError::NotServiceable(Capability::Blocks)),
            },
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

impl<F, S> AddressRead for RuntimeSnapshot<F, S>
where
    F: FinalisedState + 'static,
    S: NfsSnapshot,
{
    async fn unspent_outpoints(
        &self,
        addr: &TransparentAddress,
    ) -> Result<Vec<Utxo>, SvcAddressReadError> {
        // US-1.3: an address's unspent set spans both tiers → a **merge**, not a
        // route: finalised UTXOs (FS index) plus those created in the recent
        // window (NFS, re-derived).
        let mut utxos = self.fs.address_unspent(addr).await.map_err(map_fs_addr)?;
        if let Some(nfs) = &self.nfs {
            utxos.extend(nfs.address_unspent(addr));
        }
        // TODO(US-1.3): drop finalised UTXOs spent *within* the recent window —
        // needs recent spends-by-address from NFS.
        Ok(utxos)
    }

    async fn balance(
        &self,
        _addr: &TransparentAddress,
        _range: HeightRange,
    ) -> Result<AddressBalance, SvcAddressReadError> {
        todo!("same FS ∪ NFS merge shape as unspent_outpoints")
    }
    async fn deltas(
        &self,
        _addr: &TransparentAddress,
        _range: HeightRange,
    ) -> Result<Vec<AddressDelta>, SvcAddressReadError> {
        todo!("merge, same shape")
    }
    async fn tx_ids(
        &self,
        _addr: &TransparentAddress,
        _range: HeightRange,
    ) -> Result<Vec<TransactionHash>, SvcAddressReadError> {
        todo!("merge, same shape")
    }
}

fn map_fs_addr(e: FsAddressReadError) -> SvcAddressReadError {
    match e {
        FsAddressReadError::Backend(s) => SvcAddressReadError::Fatal(s),
        FsAddressReadError::NotEnabled => {
            SvcAddressReadError::NotServiceable(Capability::AddressHistory)
        }
    }
}
