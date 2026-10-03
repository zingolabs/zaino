//! In-memory scriptable mock of the inner driving surface.
//!
//! Behind the `testing` feature. Its job is to give a concrete
//! [`IndexerService`] to instantiate the whole stack (outer clients, adapters)
//! against in tests — the executable half of the contract.
//!
//! It exemplifies the ADR-0003 pin: the engine holds an `Arc<MockChain>`;
//! [`snapshot`](TakeSnapshot::snapshot) clones that `Arc`, and
//! [`mutate`](MockIndexerService::mutate) swaps in a new one while live
//! snapshots keep the old — so reads through a snapshot stay coherent across a
//! scripted reorg.
//!
//! First increment: wiring-complete over all capability traits, but most reads
//! return empty / `NotServiceable`. Rich data fabrication (blocks, txs, utxos)
//! lands as tests need it.

use std::sync::{Arc, Mutex};

use futures::stream::{self, BoxStream, StreamExt};

use crate::{
    Answerable, Capability, ForkPoint, Locator, MempoolTx, ReportedUpgrade, ServiceabilityManifest,
    ServiceableRange, SpendStatus, TxStatus,
};
use zaino_primitives::types::rpc::{BlockHeaderVerbose, MiningInfo, NodeInfo, PeerInfo};
use zaino_primitives::types::{
    AddressBalance, AddressDelta, Block, BlockHash, BlockHeader, BlockRef, BlockSelector,
    BlockTime, BlockVerbose, BlockchainInfo, CompactBlock, ConsensusBranchId, ConsensusBranchIds,
    DecodedBlock, Height, HeightRange, Outpoint, PreIndexCompactTx, RawTransaction, ShieldedPool,
    SubtreeRoot, Transaction, TransactionId, TransparentAddress, TransparentInput,
    TransparentReceive, TransparentSpend, Treestate, Utxo, ValuePoolBalance, Zatoshis,
    ZatoshisFlowSum,
};

use crate::error::{
    AddressReadError, BlockHashReadError, BlockReadError, BroadcastRejection, MempoolReadError,
    ReadError, SpendReadError, TransactionViewError, Transient, TreestateReadError, TxReadError,
};
use crate::{
    AddressRead, AddressReceiveRead, BlockHashAt, BlockHashRead, BlockRead, BlockTransactionViews,
    BlockVerboseRead, Broadcast, ChainInfoRead, ChainSegment, CompactBlockRead,
    CompactNullifierRead, ForkReconcile, HeaderRead, HeaderSummary, IndexerService,
    LocatedTransactionView, MempoolContent, MempoolEntry, MempoolListing, MempoolSubscribe,
    MempoolSummary, NodeStatusError, NodeStatusRead, RawTransactionRead, ReportedUpgrades,
    Serviceable, Snapshot, SpendRead, TakeSnapshot, TipSubscribe, TransactionRead,
    TransactionViewRead, TreestateRead,
};

/// Scriptable chain state. Extend as tests need more; today it carries just
/// enough to prove wiring and the pin semantics.
#[derive(Clone, Default)]
pub struct MockChain {
    pub tip: Option<BlockRef>,
    pub serviceable: Option<ServiceableRange>,
    pub mempool: Vec<MempoolTx>,
    /// Scripted transparent balances, keyed by address string. An address
    /// absent here has no history, which reads as a zero balance.
    pub balances: Vec<(String, AddressBalance)>,
    /// Scripted address deltas, filtered by address and height on read.
    pub deltas: Vec<AddressDelta>,
    /// Scripted transaction ids for [`AddressRead::tx_ids`], returned for any
    /// queried address on a serviceable chain. The mock carries no per-txid
    /// height, so the range is not applied here — the query layer's range
    /// handling is exercised separately.
    pub txids: Vec<TransactionId>,
    /// Scripted unspent outputs for [`AddressRead::unspent_outpoints`], returned
    /// for any queried address on a serviceable chain.
    pub utxos: Vec<Utxo>,
    /// Scripted treestate for [`TreestateRead::treestate`]. When `Some`, returned
    /// for any height; when `None`, the read answers `NotServiceable` (the mock
    /// has no validator behind it).
    pub treestate: Option<Treestate>,
    /// Scripted subtree roots for [`TreestateRead::subtree_roots`], returned for
    /// any pool/index query.
    pub subtree_roots: Vec<SubtreeRoot>,
    /// Scripted raw transactions, keyed by txid.
    pub raw_transactions: Vec<(TransactionId, RawTransaction)>,
    /// Scripted chain-info aggregate. When `Some`, [`ChainInfoRead::chain_info`]
    /// returns it verbatim; when `None`, the aggregate is synthesised from the
    /// pinned tip with neutral values for everything else. A test that needs to
    /// distinguish a rendered field from a defaulted one scripts this.
    pub blockchain_info: Option<BlockchainInfo>,
    /// Scripted verbose block header. When `Some`,
    /// [`BlockVerboseRead::block_header_verbose`] returns it for any hash; when
    /// `None`, it answers `Ok(None)`.
    pub block_header_verbose: Option<BlockHeaderVerbose>,
    /// Scripted verbose block. When `Some`, [`BlockVerboseRead::block_verbose`]
    /// returns it for a by-height selector (and for a by-hash one too, unless
    /// [`block_verbose_by_hash`](Self::block_verbose_by_hash) overrides it); when
    /// `None`, it answers `Ok(None)`.
    pub block_verbose: Option<BlockVerbose>,
    /// Scripted verbose block for a [`BlockSelector::Hash`] read. When `Some`, a
    /// by-hash [`BlockVerboseRead::block_verbose`] returns it, while a by-height
    /// read still returns [`block_verbose`](Self::block_verbose). Lets a test give
    /// the two selectors different answers and prove the serve adapter resolves a
    /// height to a hash once, then reads the block's contents by that hash.
    pub block_verbose_by_hash: Option<BlockVerbose>,
    /// Scripted full block. When `Some`, [`BlockRead::block`] returns it for any
    /// selector; when `None`, it answers `Ok(None)`.
    pub block: Option<Block>,
    /// Scripted resolved transaction. When `Some`,
    /// [`TransactionViewRead::transaction_view`] returns it for any id; when
    /// `None`, it answers `Ok(None)`.
    pub transaction_view: Option<LocatedTransactionView>,
    /// Scripted resolved block. When `Some`,
    /// [`TransactionViewRead::block_transaction_views`] returns it for any
    /// selector; when `None`, it answers `Ok(None)` (unless
    /// [`block_transaction_views_missing_prevout`](Self::block_transaction_views_missing_prevout)
    /// is set).
    pub block_transaction_views: Option<BlockTransactionViews>,
    /// When `Some`, [`TransactionViewRead::block_transaction_views`] errors with
    /// [`TransactionViewError::MissingPrevout`] naming this outpoint — the
    /// prevout-resolution failure `decoded_block` must not be subject to.
    pub block_transaction_views_missing_prevout: Option<TransparentInput>,
    /// Scripted decoded block. When `Some`,
    /// [`TransactionViewRead::decoded_block`] returns it for a by-height selector
    /// (and for a by-hash one too, unless
    /// [`decoded_block_by_hash`](Self::decoded_block_by_hash) overrides it); when
    /// `None`, it answers `Ok(None)`.
    pub decoded_block: Option<DecodedBlock>,
    /// Scripted decoded block for a [`BlockSelector::Hash`] read. When `Some`, a
    /// by-hash [`TransactionViewRead::decoded_block`] returns it, while a by-height
    /// read still returns [`decoded_block`](Self::decoded_block). The decoded
    /// counterpart of [`block_verbose_by_hash`](Self::block_verbose_by_hash), for
    /// the same height-resolves-to-hash-once proof.
    pub decoded_block_by_hash: Option<DecodedBlock>,
    /// Scripted raw block bytes. When `Some`, [`BlockVerboseRead::raw_block`]
    /// returns them for any selector; when `None`, it answers `Ok(None)`.
    pub raw_block: Option<Vec<u8>>,
    /// Scripted blocks for the timestamp-range selection behind `getblockhashes`.
    /// [`BlockHashRead::block_hashes`] returns those with `low <= time < high`,
    /// ordered ascending by time then by hash — the honest contract, so a test
    /// over the service mock exercises the range and ordering the engine
    /// produces.
    pub block_hashes: Vec<BlockHashAt>,
}

/// A concrete [`IndexerService`] over swappable in-memory state.
///
/// `Clone` yields another handle to the *same* engine (shared `Arc<Mutex<..>>`),
/// so one engine can back several outer adapters at once — the composition the
/// runtime performs.
#[derive(Clone)]
pub struct MockIndexerService {
    chain: Arc<Mutex<Arc<MockChain>>>,
}

impl MockIndexerService {
    pub fn new(chain: MockChain) -> Self {
        Self {
            chain: Arc::new(Mutex::new(Arc::new(chain))),
        }
    }

    fn current(&self) -> Arc<MockChain> {
        self.chain
            .lock()
            .expect("mock chain mutex poisoned")
            .clone()
    }

    /// Swap in new state; live snapshots keep the old `Arc` (ADR-0003 demo).
    pub fn mutate(&self, chain: MockChain) {
        *self.chain.lock().expect("mock chain mutex poisoned") = Arc::new(chain);
    }
}

/// A pinned view — an `Arc` of the chain as of the moment it was taken.
#[derive(Clone)]
pub struct MockSnapshot {
    chain: Arc<MockChain>,
}

// --- controls (on the engine) ---

impl TakeSnapshot for MockIndexerService {
    type Snapshot = MockSnapshot;
    async fn snapshot(&self) -> Result<MockSnapshot, Transient> {
        Ok(MockSnapshot {
            chain: self.current(),
        })
    }
}

impl TipSubscribe for MockIndexerService {
    fn subscribe_tip(&self) -> BoxStream<'_, crate::TipEvent> {
        let tip = self.current().tip;
        stream::iter(tip.map(|tip| crate::TipEvent { tip })).boxed()
    }
}

impl MempoolSubscribe for MockIndexerService {
    fn subscribe_mempool(&self) -> BoxStream<'_, MempoolTx> {
        stream::iter(self.current().mempool.clone()).boxed()
    }
}

impl MempoolListing for MockIndexerService {
    async fn mempool_txids(&self) -> Result<Vec<TransactionId>, MempoolReadError> {
        Ok(self.current().mempool.iter().map(|tx| tx.txid).collect())
    }

    async fn mempool_entries(&self) -> Result<Vec<MempoolEntry>, MempoolReadError> {
        // The mock's listing carries no serialized size or fee, so both read as
        // zero; the entry height is the tip each tx is validated against.
        Ok(self
            .current()
            .mempool
            .iter()
            .map(|tx| MempoolEntry {
                txid: tx.txid,
                size: 0,
                fee: Zatoshis::ZERO,
                entry_time: None,
                entry_height: tx.validated_against.height,
            })
            .collect())
    }

    async fn mempool_summary(&self) -> Result<MempoolSummary, MempoolReadError> {
        let size = u64::try_from(self.current().mempool.len())
            .map_err(|_| MempoolReadError::Transient("mempool length overflows u64".into()))?;
        // The mock's listing carries no bytes, so the honest total is zero.
        Ok(MempoolSummary { size, bytes: 0 })
    }
}

impl MempoolContent for MockIndexerService {
    async fn mempool_raw_transaction(
        &self,
        _txid: TransactionId,
    ) -> Result<Option<Vec<u8>>, MempoolReadError> {
        // The mock carries only a mempool listing, not transaction bytes, so it
        // answers the served "no such tx" (a listing/fetch race), never a stub.
        Ok(None)
    }

    async fn mempool_compact_transaction(
        &self,
        _txid: TransactionId,
    ) -> Result<Option<PreIndexCompactTx>, MempoolReadError> {
        Ok(None)
    }
}

impl Broadcast for MockIndexerService {
    async fn broadcast(&self, _raw_tx: Vec<u8>) -> Result<TransactionId, BroadcastRejection> {
        // A mock cannot compute the real txid; return a deterministic placeholder
        // so the broadcast path is exercisable.
        Ok(TransactionId::from([0u8; 32]))
    }
}

impl Serviceable for MockIndexerService {
    fn serviceability(&self) -> ServiceabilityManifest {
        // The mock snapshot answers every read, so the honest manifest is
        // "answerable to the pinned tip" for all of them. Live would also be
        // defensible; ToHeight keeps a consumer's height checks exercisable.
        match self.current().tip {
            Some(tip) => ServiceabilityManifest::uniform(Answerable::ToHeight(tip.height)),
            None => ServiceabilityManifest::uniform(Answerable::NotYet),
        }
    }
}

impl ReportedUpgrades for MockIndexerService {
    async fn reported_upgrades(&self) -> Result<Vec<ReportedUpgrade>, ReadError> {
        Ok(Vec::new())
    }
}

impl NodeStatusRead for MockIndexerService {
    async fn node_info(&self) -> Result<NodeInfo, NodeStatusError> {
        // The mock has no validator behind it, so "not ready" is the honest
        // answer — and it is the arm the adapter must surface as retryable.
        Err(NodeStatusError::NotReady)
    }
    async fn mining_info(&self) -> Result<MiningInfo, NodeStatusError> {
        Err(NodeStatusError::NotReady)
    }
    async fn peer_info(&self) -> Result<Vec<PeerInfo>, NodeStatusError> {
        Ok(Vec::new())
    }
    async fn network_sol_ps(
        &self,
        _blocks: Option<u32>,
        _height: Option<Height>,
    ) -> Result<u64, NodeStatusError> {
        Ok(0)
    }
}

impl IndexerService for MockIndexerService {}

// --- reads (on the snapshot) ---

impl BlockRead for MockSnapshot {
    async fn tip(&self) -> Result<BlockRef, BlockReadError> {
        self.chain
            .tip
            .ok_or(BlockReadError::NotServiceable(Capability::Blocks))
    }
    async fn block(&self, _at: BlockSelector) -> Result<Option<Block>, BlockReadError> {
        // A scripted block is returned for any selector; absent it, a served
        // `None`. The full block is otherwise a passthrough the service mock has
        // no validator behind, so scripting is the only way to exercise it.
        Ok(self.chain.block.clone())
    }
    async fn block_header(
        &self,
        _at: BlockSelector,
    ) -> Result<Option<BlockHeader>, BlockReadError> {
        Ok(None)
    }
    async fn block_height(&self, _hash: BlockHash) -> Result<Option<Height>, BlockReadError> {
        Ok(None)
    }
    fn stream_blocks(&self, _range: HeightRange) -> BoxStream<'_, Result<Block, ReadError>> {
        stream::empty().boxed()
    }
}

impl CompactBlockRead for MockSnapshot {
    async fn compact_block(
        &self,
        _at: BlockSelector,
    ) -> Result<Option<CompactBlock>, BlockReadError> {
        Ok(None)
    }
    fn stream_compact(
        &self,
        _range: HeightRange,
    ) -> BoxStream<'_, Result<CompactBlock, ReadError>> {
        stream::empty().boxed()
    }
}

impl HeaderRead for MockSnapshot {
    async fn header(&self, _h: Height) -> Result<Option<HeaderSummary>, BlockReadError> {
        // The mock scripts no blocks (like `compact_block`): every height is a
        // domain miss. The impl exists so `MockSnapshot` satisfies the tier bundle
        // the composer bounds on.
        Ok(None)
    }
}

impl BlockHashRead for MockSnapshot {
    async fn block_hashes(
        &self,
        low: BlockTime,
        high: BlockTime,
    ) -> Result<Vec<BlockHashAt>, BlockHashReadError> {
        // The honest timestamp-range contract: keep the scripted blocks in
        // `[low, high)`, ordered ascending by time then by hash. A mock with no
        // scripted blocks returns an empty list, never a stub failure.
        let mut hits: Vec<BlockHashAt> = self
            .chain
            .block_hashes
            .iter()
            .copied()
            .filter(|entry| low <= entry.time && entry.time < high)
            .collect();
        hits.sort_by(|a, b| a.time.cmp(&b.time).then_with(|| a.hash.cmp(&b.hash)));
        Ok(hits)
    }
}

impl TransactionRead for MockSnapshot {
    async fn transaction(&self, _id: TransactionId) -> Result<Option<Transaction>, TxReadError> {
        Ok(None)
    }
    async fn transaction_status(&self, _id: TransactionId) -> Result<TxStatus, TxReadError> {
        Ok(TxStatus::Unknown)
    }
}

impl RawTransactionRead for MockSnapshot {
    async fn raw_transaction(
        &self,
        id: TransactionId,
    ) -> Result<Option<RawTransaction>, TxReadError> {
        Ok(self
            .chain
            .raw_transactions
            .iter()
            .find(|(scripted, _)| *scripted == id)
            .map(|(_, tx)| tx.clone()))
    }
}

impl TreestateRead for MockSnapshot {
    async fn treestate(&self, _at: Height) -> Result<Treestate, TreestateReadError> {
        // A scripted treestate is returned verbatim for any height, so a test can
        // pin every field through the serving adapter; absent it, the mock has no
        // validator behind it, so it is not serviceable.
        self.chain
            .treestate
            .clone()
            .ok_or(TreestateReadError::NotServiceable(Capability::Treestate))
    }
    async fn subtree_roots(
        &self,
        _pool: ShieldedPool,
        _start_index: u16,
        _limit: Option<u16>,
    ) -> Result<Vec<SubtreeRoot>, TreestateReadError> {
        Ok(self.chain.subtree_roots.clone())
    }
}

impl AddressRead for MockSnapshot {
    async fn balance(
        &self,
        addr: &TransparentAddress,
        _range: HeightRange,
    ) -> Result<AddressBalance, AddressReadError> {
        Ok(self
            .chain
            .balances
            .iter()
            .find(|(scripted, _)| scripted == addr.as_str())
            .map(|(_, balance)| balance.clone())
            .unwrap_or(AddressBalance {
                balance: Zatoshis::ZERO,
                received: ZatoshisFlowSum::from_summed(0),
            }))
    }
    async fn unspent_outpoints(
        &self,
        _addr: &TransparentAddress,
    ) -> Result<Vec<Utxo>, AddressReadError> {
        Ok(self.chain.utxos.clone())
    }
    async fn deltas(
        &self,
        addr: &TransparentAddress,
        range: HeightRange,
    ) -> Result<Vec<AddressDelta>, AddressReadError> {
        Ok(self
            .chain
            .deltas
            .iter()
            .filter(|delta| delta.address.as_str() == addr.as_str())
            .filter(|delta| delta.height >= range.start && delta.height <= range.end)
            .cloned()
            .collect())
    }
    async fn tx_ids(
        &self,
        _addr: &TransparentAddress,
        _range: HeightRange,
    ) -> Result<Vec<TransactionId>, AddressReadError> {
        Ok(self.chain.txids.clone())
    }
}

impl SpendRead for MockSnapshot {
    async fn spend_status(&self, _outpoint: Outpoint) -> Result<SpendStatus, SpendReadError> {
        Ok(SpendStatus::NoSuchOutput)
    }
}

impl AddressReceiveRead for MockSnapshot {
    async fn receives(
        &self,
        _addr: &TransparentAddress,
        _range: HeightRange,
    ) -> Result<Vec<TransparentReceive>, AddressReadError> {
        Ok(Vec::new())
    }
    async fn spends(
        &self,
        _outpoints: &[Outpoint],
        _range: HeightRange,
    ) -> Result<Vec<TransparentSpend>, AddressReadError> {
        Ok(Vec::new())
    }
}

impl ForkReconcile for MockSnapshot {
    async fn fork_point(&self, _locator: Locator) -> Result<Option<ForkPoint>, ReadError> {
        Ok(None)
    }
    fn blocks_to_tip(&self, _from: Height) -> BoxStream<'_, Result<Block, ReadError>> {
        stream::empty().boxed()
    }
}

impl CompactNullifierRead for MockSnapshot {
    async fn compact_block_nullifiers(
        &self,
        _at: BlockSelector,
    ) -> Result<Option<CompactBlock>, BlockReadError> {
        Ok(None)
    }
}

impl ChainInfoRead for MockSnapshot {
    async fn chain_info(&self) -> Result<BlockchainInfo, ReadError> {
        // A scripted aggregate is returned verbatim, so a test can pin every
        // field through the serving adapter.
        if let Some(info) = &self.chain.blockchain_info {
            return Ok(info.clone());
        }
        // Otherwise the service-level mock has no validator behind it, so it
        // synthesises the aggregate from its pinned tip: the height-bearing
        // fields track the tip, the rest are neutral. A test that needs rich
        // chain-info either scripts `blockchain_info` here or exercises the real
        // passthrough through a source-level `MockChain`.
        let tip = self.chain.tip;
        let height = tip.map(|id| id.height).unwrap_or(Height::GENESIS);
        Ok(BlockchainInfo {
            chain: "main".to_string(),
            blocks: height,
            headers: height,
            estimated_height: height,
            best_block_hash: tip.map(|id| id.hash).unwrap_or(BlockHash::ZERO),
            difficulty: 0.0,
            verification_progress: 1.0,
            chain_work: None,
            pruned: false,
            size_on_disk: 0,
            commitments: 0,
            chain_supply: ValuePoolBalance {
                id: "transparent".to_string(),
                chain_value: Zatoshis::ZERO,
                monitored: true,
                value_delta: None,
            },
            value_pools: Vec::new(),
            upgrades: Vec::new(),
            consensus: ConsensusBranchIds {
                chain_tip: ConsensusBranchId::new(0),
                next_block: ConsensusBranchId::new(0),
            },
        })
    }
}

impl BlockVerboseRead for MockSnapshot {
    async fn block_header_verbose(
        &self,
        _hash: BlockHash,
    ) -> Result<Option<BlockHeaderVerbose>, BlockReadError> {
        // A scripted header is returned verbatim for any hash, so a test can pin
        // every field through the serving adapter; absent it, a served `None`.
        Ok(self.chain.block_header_verbose.clone())
    }
    async fn block_verbose(
        &self,
        at: BlockSelector,
    ) -> Result<Option<BlockVerbose>, BlockReadError> {
        // A by-hash read takes the by-hash script when one is set, so a test can
        // prove the serve adapter re-reads by the resolved hash, not by height.
        match at {
            BlockSelector::Hash(_) if self.chain.block_verbose_by_hash.is_some() => {
                Ok(self.chain.block_verbose_by_hash.clone())
            }
            _ => Ok(self.chain.block_verbose.clone()),
        }
    }
    async fn raw_block(&self, _at: BlockSelector) -> Result<Option<Vec<u8>>, BlockReadError> {
        // Scripted raw bytes returned for any selector; absent them, a served
        // `None`. The full block is otherwise a passthrough the service mock has no
        // validator behind, so scripting is the only way to exercise it.
        Ok(self.chain.raw_block.clone())
    }
}

impl TransactionViewRead for MockSnapshot {
    async fn transaction_view(
        &self,
        _id: TransactionId,
    ) -> Result<Option<LocatedTransactionView>, TransactionViewError> {
        // A scripted view is returned verbatim for any id, so a test can pin every
        // field through the serving adapter; absent it, a served `None`.
        Ok(self.chain.transaction_view.clone())
    }
    async fn block_transaction_views(
        &self,
        _at: BlockSelector,
    ) -> Result<Option<BlockTransactionViews>, TransactionViewError> {
        if let Some(outpoint) = self.chain.block_transaction_views_missing_prevout.clone() {
            return Err(TransactionViewError::MissingPrevout { outpoint });
        }
        Ok(self.chain.block_transaction_views.clone())
    }
    async fn decoded_block(
        &self,
        at: BlockSelector,
    ) -> Result<Option<DecodedBlock>, TransactionViewError> {
        // The decoded block as scripted, with no prevout resolution — so a test
        // can let `block_transaction_views` fail while this succeeds. A by-hash
        // read takes the by-hash script when one is set, mirroring `block_verbose`.
        match at {
            BlockSelector::Hash(_) if self.chain.decoded_block_by_hash.is_some() => {
                Ok(self.chain.decoded_block_by_hash.clone())
            }
            _ => Ok(self.chain.decoded_block.clone()),
        }
    }
}

impl ChainSegment for MockSnapshot {
    fn pinned_tip(&self) -> Option<BlockRef> {
        self.chain.tip
    }

    fn coverage(&self) -> Option<HeightRange> {
        // The mock serves `[genesis, tip]` when it holds a tip, nothing when
        // empty — mirroring the finalised store's neutral coverage.
        self.chain.tip.map(|tip| HeightRange {
            start: Height::GENESIS,
            end: tip.height,
        })
    }
}

impl Snapshot for MockSnapshot {
    fn serviceable_range(&self) -> Option<ServiceableRange> {
        self.chain.serviceable
    }
}

#[cfg(test)]
mod tests {
    use super::{MockChain, MockIndexerService};
    use crate::{BlockRead, FullWalletService, LightWalletService, NodeRpcService, TakeSnapshot};
    use zaino_primitives::types::{BlockHash, BlockRef, Height};

    /// The one concrete engine satisfies every use case's service with zero
    /// use-case-specific impl code — proof the read-set + control blanket impls
    /// compose. Compile-time only; the body is a no-op.
    #[test]
    fn mock_satisfies_every_use_case() {
        fn assert_services<T: FullWalletService + LightWalletService + NodeRpcService>() {}
        assert_services::<MockIndexerService>();
    }

    /// Every capability has a serviceability answer, including the node-status
    /// passthrough bundle.
    #[test]
    fn node_status_is_a_capability() {
        use crate::{Answerable, Capability, Serviceable};
        let engine = MockIndexerService::new(MockChain::default());
        assert_eq!(
            engine.serviceability().get(Capability::NodeStatus),
            Answerable::NotYet
        );
    }

    fn block_id(height: u32, tag: u8) -> BlockRef {
        BlockRef {
            height: Height::try_from(height).expect("valid height"),
            hash: BlockHash::from([tag; 32]),
        }
    }

    /// A snapshot pins the tip it was taken at, even after the engine's view
    /// swaps to a new one (ADR-0003).
    #[tokio::test]
    async fn snapshot_pins_tip_across_mutation() {
        let a = block_id(100, 0xAA);
        let b = block_id(101, 0xBB);
        let engine = MockIndexerService::new(MockChain {
            tip: Some(a),
            ..Default::default()
        });

        let pinned = engine.snapshot().await.expect("snapshot");
        assert_eq!(pinned.tip().await.expect("tip"), a);

        engine.mutate(MockChain {
            tip: Some(b),
            ..Default::default()
        });

        // The old snapshot still sees the pinned tip; a fresh one sees the new.
        assert_eq!(pinned.tip().await.expect("tip"), a);
        let fresh = engine.snapshot().await.expect("snapshot");
        assert_eq!(fresh.tip().await.expect("tip"), b);
    }
}
