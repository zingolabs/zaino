//! The runtime's pinned read-context.
//!
//! `RuntimeSnapshot` holds a shared FS handle, a pinned NFS view (`None` while
//! syncing), the finalised watermark, the passthrough handle, and the config.
//! Its capability impls are thin: they consult [`crate::resolve`] for the
//! composition decision (route / merge / passthrough) and do only the
//! type-specific work.

use std::sync::Arc;

use futures::stream::{self, BoxStream, StreamExt};

use zaino_core::{
    AddressBalance, AddressDelta, Block, BlockHash, BlockHeader, BlockId, BlockRef, Capability,
    CompactBlock, Height, HeightRange, Transaction, TransactionHash, TransparentAddress, TxStatus,
    Utxo,
};
use zaino_fs::error::{AddressReadError as FsAddressReadError, HeightReadError, LookupError};
use zaino_fs::FinalisedState;
use zaino_nfs::NfsSnapshot;
use zaino_service::error::{
    AddressReadError as SvcAddressReadError, BlockReadError, ReadError, TxReadError,
};
use zaino_service::{AddressRead, BlockRead, CompactBlockRead, TransactionRead};

use crate::config::RuntimeConfig;
use crate::passthrough::Passthrough;
use crate::resolve::{self, Tier};

/// A pinned, reorg-coherent view composed from both components + passthrough.
pub struct RuntimeSnapshot<F, S, P> {
    pub(crate) fs: Arc<F>,
    /// `None` while the NFS window is still syncing — recent reads are then
    /// `NotServiceable`, never a false `None`.
    pub(crate) nfs: Option<S>,
    pub(crate) watermark: Height,
    pub(crate) passthrough: Arc<P>,
    pub(crate) cfg: Arc<RuntimeConfig>,
}

// Manual Clone: the `Arc`s clone regardless of `F`/`P`, so we don't want the
// derive's spurious `F: Clone` / `P: Clone` bounds.
impl<F, S: Clone, P> Clone for RuntimeSnapshot<F, S, P> {
    fn clone(&self) -> Self {
        Self {
            fs: Arc::clone(&self.fs),
            nfs: self.nfs.clone(),
            watermark: self.watermark,
            passthrough: Arc::clone(&self.passthrough),
            cfg: Arc::clone(&self.cfg),
        }
    }
}

impl<F, S, P> CompactBlockRead for RuntimeSnapshot<F, S, P>
where
    F: FinalisedState + 'static,
    S: NfsSnapshot,
    P: Passthrough,
{
    async fn compact_block(&self, at: BlockRef) -> Result<Option<CompactBlock>, BlockReadError> {
        let height = match at {
            BlockRef::Height(h) => h,
            BlockRef::Hash(_) => todo!("resolve hash -> height across NFS/FS, then route"),
        };
        // Route: one tier, by height at the watermark.
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

impl<F, S, P> BlockRead for RuntimeSnapshot<F, S, P>
where
    F: FinalisedState + 'static,
    S: NfsSnapshot,
    P: Passthrough,
{
    async fn tip(&self) -> Result<BlockId, BlockReadError> {
        match &self.nfs {
            Some(nfs) => Ok(nfs.tip()),
            None => Err(BlockReadError::NotServiceable(Capability::Blocks)),
        }
    }

    async fn block(&self, at: BlockRef) -> Result<Option<Block>, BlockReadError> {
        // Passthrough: full blocks aren't stored. By hash for reorg-safety.
        let hash = match at {
            BlockRef::Hash(h) => h,
            BlockRef::Height(_) => {
                todo!("resolve hash-at-height as of the snapshot, then passthrough by hash")
            }
        };
        if !resolve::passthrough_allowed(Capability::Blocks, &self.cfg) {
            return Err(BlockReadError::NotServiceable(Capability::Blocks));
        }
        self.passthrough
            .full_block(hash)
            .await
            .map_err(|e| BlockReadError::Transient(format!("passthrough: {e:?}")))
    }

    async fn block_header(&self, _at: BlockRef) -> Result<Option<BlockHeader>, BlockReadError> {
        todo!("route to the stored compact header, or passthrough for the full header")
    }

    async fn block_height(&self, hash: BlockHash) -> Result<Option<Height>, BlockReadError> {
        // Check recent (NFS, infallible) first, then finalised (FS index).
        if let Some(nfs) = &self.nfs {
            if let Some(h) = nfs.height_of(hash) {
                return Ok(Some(h));
            }
        }
        self.fs.height_of(hash).await.map_err(lookup_err)
    }

    fn stream_blocks(&self, _range: HeightRange) -> BoxStream<'_, Result<Block, ReadError>> {
        stream::empty().boxed()
    }
}

impl<F, S, P> TransactionRead for RuntimeSnapshot<F, S, P>
where
    F: FinalisedState + 'static,
    S: NfsSnapshot,
    P: Passthrough,
{
    async fn transaction(&self, id: TransactionHash) -> Result<Option<Transaction>, TxReadError> {
        // Passthrough: raw transactions aren't stored. By txid (immutable →
        // coherent).
        if !resolve::passthrough_allowed(Capability::Transactions, &self.cfg) {
            return Err(TxReadError::NotServiceable(Capability::Transactions));
        }
        self.passthrough
            .raw_transaction(id)
            .await
            .map_err(|e| TxReadError::Transient(format!("passthrough: {e:?}")))
    }

    async fn transaction_status(&self, _id: TransactionHash) -> Result<TxStatus, TxReadError> {
        todo!("route: tx_location -> height -> mined/orphaned status")
    }
}

impl<F, S, P> AddressRead for RuntimeSnapshot<F, S, P>
where
    F: FinalisedState + 'static,
    S: NfsSnapshot,
    P: Passthrough,
{
    async fn unspent_outpoints(
        &self,
        addr: &TransparentAddress,
    ) -> Result<Vec<Utxo>, SvcAddressReadError> {
        // Merge: needs both tiers to be coherent, so it is unserviceable until
        // the recent window is ready. The combine lives in `resolve`.
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

fn lookup_err(e: LookupError) -> BlockReadError {
    match e {
        LookupError::Backend(s) => BlockReadError::Fatal(s),
    }
}
