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
use zaino_fs::{AddressIndex, FinalisedSpine};
use zaino_nfs::{NfsAddressFacts, NfsSpine};
use zaino_service::error::{
    AddressReadError as SvcAddressReadError, BlockReadError, ReadError, TxReadError,
};
use zaino_service::{AddressRead, BlockRead, CompactBlockRead, TransactionRead};

use crate::config::{CapabilitySet, RuntimeConfig};
use crate::passthrough::PassthroughSource;
use crate::resolve::{self, Tier};

/// A pinned, reorg-coherent view composed from both components + the validator
/// source (passthrough).
pub struct RuntimeSnapshot<F, S, Src> {
    pub(crate) fs: Arc<F>,
    /// `None` while the NFS window is still syncing — recent reads are then
    /// `NotServiceable`, never a false `None`.
    pub(crate) nfs: Option<S>,
    pub(crate) watermark: Height,
    pub(crate) source: Arc<Src>,
    pub(crate) cfg: Arc<RuntimeConfig>,
    /// The deployment's served set — reads consult it so a capability is
    /// answerable iff the manifest advertises it (same source of truth).
    pub(crate) served: Arc<CapabilitySet>,
}

// Manual Clone: the `Arc`s clone regardless of `F`/`Src`, so we don't want the
// derive's spurious `F: Clone` / `Src: Clone` bounds.
impl<F, S: Clone, Src> Clone for RuntimeSnapshot<F, S, Src> {
    fn clone(&self) -> Self {
        Self {
            fs: Arc::clone(&self.fs),
            nfs: self.nfs.clone(),
            watermark: self.watermark,
            source: Arc::clone(&self.source),
            cfg: Arc::clone(&self.cfg),
            served: Arc::clone(&self.served),
        }
    }
}

impl<F, S, Src> CompactBlockRead for RuntimeSnapshot<F, S, Src>
where
    F: FinalisedSpine + 'static,
    S: NfsSpine,
    Src: Send + Sync, // captured in the `Send` future, but compact blocks aren't passthrough
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

impl<F, S, Src> BlockRead for RuntimeSnapshot<F, S, Src>
where
    F: FinalisedSpine + 'static,
    S: NfsSpine,
    Src: PassthroughSource,
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
        // Pass through to the validator (a domain read; the adapter owns the
        // transport). `None` is a real miss. Serving policy: an unavailable
        // upstream projects to `Transient` — the consumer may retry.
        self.source
            .block_by_hash(hash)
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

impl<F, S, Src> TransactionRead for RuntimeSnapshot<F, S, Src>
where
    // Only passthrough today; wiring `transaction_status` will add
    // `+ TxLocationIndex` on F (and NFS's recent-tx facet). F/S are only
    // captured in the `Send` future for now.
    F: FinalisedSpine + 'static,
    S: NfsSpine,
    Src: PassthroughSource,
{
    async fn transaction(&self, id: TransactionHash) -> Result<Option<Transaction>, TxReadError> {
        // Passthrough: raw transactions aren't stored. By txid (immutable →
        // coherent).
        if !resolve::passthrough_allowed(Capability::Transactions, &self.cfg) {
            return Err(TxReadError::NotServiceable(Capability::Transactions));
        }
        // Domain read: the adapter has already parsed raw bytes into a domain
        // `Transaction`. `None` is a real miss. Serving policy: an unavailable
        // upstream projects to `Transient` — the consumer may retry.
        self.source
            .transaction(id)
            .await
            .map_err(|e| TxReadError::Transient(format!("passthrough: {e:?}")))
    }

    async fn transaction_status(&self, _id: TransactionHash) -> Result<TxStatus, TxReadError> {
        todo!("route: tx_location -> height -> mined/orphaned status")
    }
}

impl<F, S, Src> AddressRead for RuntimeSnapshot<F, S, Src>
where
    // The variant payoff: `AddressRead` exists only when the FS builds the
    // address index. A minimal FS (no `AddressIndex`) → this impl is absent →
    // the capability is unrepresentable, not a runtime miss.
    F: AddressIndex + 'static,
    S: NfsAddressFacts,
    Src: Send + Sync, // captured in the `Send` future; address history isn't passthrough
{
    async fn unspent_outpoints(
        &self,
        addr: &TransparentAddress,
    ) -> Result<Vec<Utxo>, SvcAddressReadError> {
        // Same served set as the manifest: if the deployment didn't opt in (the
        // index isn't built), the read is `NotServiceable` — advertised and
        // answerable stay in lockstep.
        if !self.served.contains(Capability::AddressHistory) {
            return Err(SvcAddressReadError::NotServiceable(Capability::AddressHistory));
        }
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
