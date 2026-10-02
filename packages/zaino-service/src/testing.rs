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
    Answerable, Capability, ForkPoint, Locator, MempoolTx, NodeQuery, NodeQueryAnswer,
    ReportedUpgrade, ServiceabilityManifest, ServiceableRange, SpendStatus, TxStatus,
};
use zaino_primitives::types::{
    AddressBalance, AddressDelta, Block, BlockHash, BlockHeader, BlockRef, BlockSelector,
    BlockchainInfo, CompactBlock, ConsensusBranchId, ConsensusBranchIds, Height, HeightRange,
    Outpoint, PreIndexCompactTx, RawTransaction, ShieldedPool, SubtreeRoot, Transaction,
    TransactionId, TransparentAddress, Treestate, Utxo, ValuePoolBalance, Zatoshis,
    ZatoshisFlowSum,
};

use crate::error::{
    AddressReadError, BlockReadError, BroadcastRejection, MempoolReadError, ReadError,
    SpendReadError, Transient, TreestateReadError, TxReadError,
};
use crate::{
    AddressRead, BlockRead, Broadcast, ChainInfoRead, ChainSegment, CompactBlockRead,
    CompactNullifierRead, ForkReconcile, IndexerService, MempoolContent, MempoolSubscribe,
    NodeQueryRelay, RawTransactionRead, ReportedUpgrades, Serviceable, Snapshot, SpendRead,
    TakeSnapshot, TipSubscribe, TransactionRead, TreestateRead,
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
    /// Scripted raw transactions, keyed by txid.
    pub raw_transactions: Vec<(TransactionId, RawTransaction)>,
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

impl NodeQueryRelay for MockIndexerService {
    async fn relay_node_query(&self, query: NodeQuery) -> Result<NodeQueryAnswer, Transient> {
        Ok(NodeQueryAnswer(format!("mock passthrough: {query:?}")))
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
        Ok(None)
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
        Err(TreestateReadError::NotServiceable(Capability::Treestate))
    }
    async fn subtree_roots(
        &self,
        _pool: ShieldedPool,
        _start_index: u16,
        _limit: Option<u16>,
    ) -> Result<Vec<SubtreeRoot>, TreestateReadError> {
        Ok(Vec::new())
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
        Ok(Vec::new())
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
        Ok(Vec::new())
    }
}

impl SpendRead for MockSnapshot {
    async fn spend_status(&self, _outpoint: Outpoint) -> Result<SpendStatus, SpendReadError> {
        Ok(SpendStatus::NoSuchOutput)
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
        // The service-level mock has no validator behind it, so it synthesises
        // the aggregate from its pinned tip: the height-bearing fields track the
        // tip, the rest are neutral. A test that needs rich chain-info exercises
        // the real passthrough through a source-level `MockChain`.
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
