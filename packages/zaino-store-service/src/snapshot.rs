//! [`EngineSnapshot`] — the pinned read surface.
//!
//! A pinned, reorg-coherent view over the composed chain: the composer's
//! [`ChainViewSnapshot`], dressed as the full read surface. The coherence marker,
//! the served range, and the compact-block reads delegate to the inner composed
//! view; the passthrough reads (treestate, transparent addresses) delegate to the
//! [`RemoteChainView`] captured alongside it; the reads no substrate yet sources
//! are `NotServiceable` stubs on top.

use futures::stream::{self, BoxStream, StreamExt};

use zaino_chainview::ChainViewSnapshot;
use zaino_source::{
    GetAddressBalance, GetAddressDeltas, GetAddressTxids, GetAddressUtxos, GetTreestate,
};

use zaino_core::{
    AddressBalance, AddressDelta, Block, BlockHash, BlockHeader, BlockId, BlockRef, Capability,
    ChainInfo, CompactBlock, Height, HeightRange, Outpoint, ServiceableRange, ShieldedPool,
    SpendStatus, SubtreeRoot, Transaction, TransactionId, TransparentAddress, Treestate, TxStatus,
    Utxo,
};
use zaino_service::error::{
    AddressReadError, BlockReadError, ReadError, SpendReadError, TreestateReadError, TxReadError,
};
use zaino_service::{
    AddressRead, BlockRead, ChainInfoRead, ChainSegment, CompactBlockRead, CompactNullifierRead,
    Snapshot, SpendRead, TransactionRead, TreestateRead,
};

use crate::remote::RemoteChainView;

/// Genesis height, the fallback tip for a view holding nothing.
fn genesis() -> Height {
    Height::try_from(0).expect("0 is a valid height")
}

/// A pinned view over the composed chain, plus the live passthrough handle.
pub struct EngineSnapshot<F, N, Src> {
    /// Pinned local reads — the composed FS⊕NFS view.
    local: ChainViewSnapshot<F, N>,
    /// Live passthrough reads — the validator through the source ports. Captured
    /// at snapshot time; its reads are live (not pinned), which is sound for the
    /// immutable data light clients query.
    remote: RemoteChainView<Src>,
}

impl<F, N, Src> EngineSnapshot<F, N, Src> {
    /// Pair a pinned local view with the passthrough handle riding alongside it.
    /// Built only by the engine's [`snapshot`](crate::Engine), which captures the
    /// local sides in one shot so the pin stays coherent across the seam.
    pub(crate) fn new(local: ChainViewSnapshot<F, N>, remote: RemoteChainView<Src>) -> Self {
        Self { local, remote }
    }
}

impl<F: Clone, N: Clone, Src: Clone> Clone for EngineSnapshot<F, N, Src> {
    fn clone(&self) -> Self {
        Self {
            local: self.local.clone(),
            remote: self.remote.clone(),
        }
    }
}

// --- coherence marker + served range -----------------------------------------
//
// Delegated to the composed view: the pin, the coverage, and the finalised/tip
// boundary are exactly what the composer already computes across the seam.

impl<F, N, Src> ChainSegment for EngineSnapshot<F, N, Src>
where
    F: ChainSegment + CompactBlockRead,
    N: ChainSegment + CompactBlockRead,
    Src: Clone + Send + Sync + 'static,
{
    fn pinned_tip(&self) -> Option<BlockId> {
        self.local.pinned_tip()
    }

    fn coverage(&self) -> Option<HeightRange> {
        self.local.coverage()
    }
}

impl<F, N, Src> Snapshot for EngineSnapshot<F, N, Src>
where
    F: ChainSegment + CompactBlockRead,
    N: ChainSegment + CompactBlockRead,
    Src: Clone + Send + Sync + 'static,
{
    fn serviceable_range(&self) -> ServiceableRange {
        self.local.serviceable_range()
    }
}

// --- reads -------------------------------------------------------------------
//
// Compact-block serving and chain-info read from the composed view; treestate
// and transparent-address reads pass through to the validator; the reads no
// substrate yet sources are NotServiceable.

impl<F, N, Src> CompactBlockRead for EngineSnapshot<F, N, Src>
where
    F: ChainSegment + CompactBlockRead,
    N: ChainSegment + CompactBlockRead,
    Src: Clone + Send + Sync + 'static,
{
    async fn compact_block(&self, at: BlockRef) -> Result<Option<CompactBlock>, BlockReadError> {
        self.local.compact_block(at).await
    }
    fn stream_compact(&self, range: HeightRange) -> BoxStream<'_, Result<CompactBlock, ReadError>> {
        self.local.stream_compact(range)
    }
}

impl<F, N, Src> BlockRead for EngineSnapshot<F, N, Src>
where
    F: ChainSegment + CompactBlockRead,
    N: ChainSegment + CompactBlockRead,
    Src: Clone + Send + Sync + 'static,
{
    async fn tip(&self) -> Result<BlockId, BlockReadError> {
        // The composed tip — the NFS tip, falling back to the finalised tip.
        self.local
            .pinned_tip()
            .ok_or(BlockReadError::NotServiceable(Capability::Blocks))
    }
    async fn block(&self, _at: BlockRef) -> Result<Option<Block>, BlockReadError> {
        Err(BlockReadError::NotServiceable(Capability::Blocks))
    }
    async fn block_header(&self, _at: BlockRef) -> Result<Option<BlockHeader>, BlockReadError> {
        Err(BlockReadError::NotServiceable(Capability::Blocks))
    }
    async fn block_height(&self, _hash: BlockHash) -> Result<Option<Height>, BlockReadError> {
        Err(BlockReadError::NotServiceable(Capability::Blocks))
    }
    fn stream_blocks(&self, _range: HeightRange) -> BoxStream<'_, Result<Block, ReadError>> {
        stream::empty().boxed()
    }
}

impl<F, N, Src> TransactionRead for EngineSnapshot<F, N, Src>
where
    F: ChainSegment + CompactBlockRead,
    N: ChainSegment + CompactBlockRead,
    Src: Clone + Send + Sync + 'static,
{
    async fn transaction(&self, _id: TransactionId) -> Result<Option<Transaction>, TxReadError> {
        Err(TxReadError::NotServiceable(Capability::RawTransaction))
    }
    async fn transaction_status(&self, _id: TransactionId) -> Result<TxStatus, TxReadError> {
        Err(TxReadError::NotServiceable(Capability::TransactionLocation))
    }
}

impl<F, N, Src> TreestateRead for EngineSnapshot<F, N, Src>
where
    F: ChainSegment + CompactBlockRead,
    N: ChainSegment + CompactBlockRead,
    Src: GetTreestate + Send + Sync + 'static,
{
    async fn treestate(&self, at: Height) -> Result<Treestate, TreestateReadError> {
        // Passthrough: zaino does not index treestate, so the remote view answers
        // it live. That treestate is remote is the read-set's per-capability
        // local/passthrough classification, expressed here.
        self.remote.treestate(at).await
    }
    async fn subtree_roots(
        &self,
        _pool: ShieldedPool,
        _range: HeightRange,
    ) -> Result<Vec<SubtreeRoot>, TreestateReadError> {
        // Still stubbed: the service asks by height range, the source by
        // subtree-index + limit — passthrough needs a translation, not a straight
        // relay. Wired once that mapping lands.
        Err(TreestateReadError::NotServiceable(Capability::SubtreeRoots))
    }
}

impl<F, N, Src> AddressRead for EngineSnapshot<F, N, Src>
where
    F: ChainSegment + CompactBlockRead,
    N: ChainSegment + CompactBlockRead,
    Src: GetAddressBalance
        + GetAddressUtxos
        + GetAddressTxids
        + GetAddressDeltas
        + Send
        + Sync
        + 'static,
{
    // Passthrough: zaino does not yet index transparent addresses, so the remote
    // view answers these live from the validator. That address reads are remote
    // is the read-set's per-capability classification, expressed here — and a
    // stopgap: passing addresses to the validator discloses them, which a local
    // transparent index exists to avoid (see `RemoteChainView::balance`).
    async fn balance(
        &self,
        addr: &TransparentAddress,
        _range: HeightRange,
    ) -> Result<AddressBalance, AddressReadError> {
        // The requested range is not honoured — `getaddressbalance` is
        // range-less; this is balance as of the validator's tip.
        self.remote.balance(addr).await
    }
    async fn unspent_outpoints(
        &self,
        addr: &TransparentAddress,
    ) -> Result<Vec<Utxo>, AddressReadError> {
        self.remote.unspent_outpoints(addr).await
    }
    async fn deltas(
        &self,
        addr: &TransparentAddress,
        range: HeightRange,
    ) -> Result<Vec<AddressDelta>, AddressReadError> {
        self.remote.deltas(addr, range).await
    }
    async fn tx_ids(
        &self,
        addr: &TransparentAddress,
        range: HeightRange,
    ) -> Result<Vec<TransactionId>, AddressReadError> {
        self.remote.tx_ids(addr, range).await
    }
}

impl<F, N, Src> SpendRead for EngineSnapshot<F, N, Src>
where
    F: ChainSegment + CompactBlockRead,
    N: ChainSegment + CompactBlockRead,
    Src: Clone + Send + Sync + 'static,
{
    async fn spend_status(&self, _outpoint: Outpoint) -> Result<SpendStatus, SpendReadError> {
        Err(SpendReadError::NotServiceable(Capability::SpendStatus))
    }
}

impl<F, N, Src> CompactNullifierRead for EngineSnapshot<F, N, Src>
where
    F: ChainSegment + CompactBlockRead,
    N: ChainSegment + CompactBlockRead,
    Src: Clone + Send + Sync + 'static,
{
    async fn compact_block_nullifiers(
        &self,
        _at: BlockRef,
    ) -> Result<Option<CompactBlock>, BlockReadError> {
        Err(BlockReadError::NotServiceable(Capability::Blocks))
    }
}

impl<F, N, Src> ChainInfoRead for EngineSnapshot<F, N, Src>
where
    F: ChainSegment + CompactBlockRead,
    N: ChainSegment + CompactBlockRead,
    Src: Clone + Send + Sync + 'static,
{
    async fn chain_info(&self) -> Result<ChainInfo, ReadError> {
        // Read from the composed pinned tip; an empty view falls back to genesis.
        let tip = self.local.pinned_tip();
        let estimated_height = tip.map(|id| id.height).unwrap_or_else(genesis);
        Ok(ChainInfo {
            tip,
            estimated_height,
        })
    }
}
