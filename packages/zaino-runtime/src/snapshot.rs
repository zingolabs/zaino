//! The runtime's pinned read-context.
//!
//! `RuntimeSnapshot` holds a shared FS handle, a pinned NFS view (`None` while
//! syncing), the finalised watermark, and the config. Its capability impls are
//! thin: they consult [`crate::resolve`] for the composition decision (route /
//! merge / passthrough) and do only the type-specific work.

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

use crate::config::RuntimeConfig;
use crate::resolve::{self, Tier};

/// A pinned, reorg-coherent view composed from both components.
pub struct RuntimeSnapshot<F, S> {
    pub(crate) fs: Arc<F>,
    /// `None` while the NFS window is still syncing — recent reads are then
    /// `NotServiceable`, never a false `None`.
    pub(crate) nfs: Option<S>,
    pub(crate) watermark: Height,
    pub(crate) cfg: Arc<RuntimeConfig>,
}

// Manual Clone: `Arc<F>` clones regardless of `F`, so we don't want the derive's
// spurious `F: Clone` bound.
impl<F, S: Clone> Clone for RuntimeSnapshot<F, S> {
    fn clone(&self) -> Self {
        Self {
            fs: Arc::clone(&self.fs),
            nfs: self.nfs.clone(),
            watermark: self.watermark,
            cfg: Arc::clone(&self.cfg),
        }
    }
}

impl<F, S> CompactBlockRead for RuntimeSnapshot<F, S>
where
    F: FinalisedState + 'static,
    S: NfsSnapshot,
{
    async fn compact_block(&self, at: BlockRef) -> Result<Option<CompactBlock>, BlockReadError> {
        let height = match at {
            BlockRef::Height(h) => h,
            BlockRef::Hash(_) => todo!("resolve hash -> height across NFS/FS, then route"),
        };
        // Route: one tier, by height at the watermark (the boundary is policy;
        // recent-not-ready is our `Option`).
        match resolve::tier_of(height, self.watermark) {
            Tier::Finalised => self.fs.compact_block(height).await.map_err(fs_height_err),
            Tier::Recent => match &self.nfs {
                Some(nfs) => Ok(nfs.compact_block(height)),
                None => Err(BlockReadError::NotServiceable(Capability::Blocks)),
            },
        }
    }

    fn stream_compact(&self, _range: HeightRange) -> BoxStream<'_, Result<CompactBlock, ReadError>> {
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
        // Merge: needs both tiers to be coherent, so it is unserviceable until
        // the recent window is ready. The combine itself lives in `resolve`.
        let Some(nfs) = &self.nfs else {
            return Err(SvcAddressReadError::NotServiceable(Capability::AddressHistory));
        };
        let finalised = self.fs.address_unspent(addr).await.map_err(fs_addr_err)?;
        let recent = nfs.address_unspent(addr);
        Ok(resolve::merge_unspent(finalised, recent))
    }

    async fn balance(
        &self,
        _addr: &TransparentAddress,
        _range: HeightRange,
    ) -> Result<AddressBalance, SvcAddressReadError> {
        todo!("Merge — same shape as unspent_outpoints")
    }
    async fn deltas(
        &self,
        _addr: &TransparentAddress,
        _range: HeightRange,
    ) -> Result<Vec<AddressDelta>, SvcAddressReadError> {
        todo!("Merge")
    }
    async fn tx_ids(
        &self,
        _addr: &TransparentAddress,
        _range: HeightRange,
    ) -> Result<Vec<TransactionHash>, SvcAddressReadError> {
        todo!("Merge")
    }
}

// --- error mapping at the FS → service boundary ---

fn fs_height_err(e: HeightReadError) -> BlockReadError {
    match e {
        HeightReadError::AboveWatermark(_) => BlockReadError::NotServiceable(Capability::Blocks),
        HeightReadError::Backend(s) => BlockReadError::Fatal(s),
    }
}

fn fs_addr_err(e: FsAddressReadError) -> SvcAddressReadError {
    match e {
        FsAddressReadError::Backend(s) => SvcAddressReadError::Fatal(s),
        FsAddressReadError::NotEnabled => {
            SvcAddressReadError::NotServiceable(Capability::AddressHistory)
        }
    }
}
