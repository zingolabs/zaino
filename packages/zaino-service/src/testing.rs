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

use zaino_core::{
    AddressBalance, AddressDelta, Block, BlockHash, BlockHeader, BlockId, BlockRef, Capability,
    ChainInfo, CompactBlock, ForkPoint, Height, HeightRange, Locator, MempoolTx, Outpoint,
    PassthroughAnswer, PassthroughQuery, RawTransaction, ReportedUpgrade, ServiceabilityManifest,
    ServiceableRange, ShieldedPool, SpendStatus, SubtreeRoot, Transaction, TransactionId,
    TransparentAddress, Treestate, TxStatus, Utxo,
};

use crate::error::{
    AddressReadError, BlockReadError, BroadcastRejection, ReadError, SpendReadError, Transient,
    TreestateReadError, TxReadError,
};
use crate::{
    AddressRead, BlockRead, Broadcast, ChainInfoRead, ChainSegment, CompactBlockRead,
    CompactNullifierRead, ForkReconcile, IndexerService, MempoolSubscribe, Passthrough,
    RawTransactionRead, ReportedUpgrades, Serviceable, Snapshot, SpendRead, TakeSnapshot,
    TipSubscribe, TransactionRead, TreestateRead,
};

/// Scriptable chain state. Extend as tests need more; today it carries just
/// enough to prove wiring and the pin semantics.
#[derive(Clone, Default)]
pub struct MockChain {
    pub tip: Option<BlockId>,
    pub serviceable: Option<ServiceableRange>,
    pub mempool: Vec<MempoolTx>,
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
    fn subscribe_tip(&self) -> BoxStream<'_, zaino_core::TipEvent> {
        let tip = self.current().tip;
        stream::iter(tip.map(|tip| zaino_core::TipEvent { tip })).boxed()
    }
}

impl MempoolSubscribe for MockIndexerService {
    fn subscribe_mempool(&self) -> BoxStream<'_, MempoolTx> {
        stream::iter(self.current().mempool.clone()).boxed()
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
        ServiceabilityManifest::default()
    }
}

impl ReportedUpgrades for MockIndexerService {
    async fn reported_upgrades(&self) -> Result<Vec<ReportedUpgrade>, ReadError> {
        Ok(Vec::new())
    }
}

impl Passthrough for MockIndexerService {
    async fn passthrough(&self, query: PassthroughQuery) -> Result<PassthroughAnswer, Transient> {
        Ok(PassthroughAnswer(format!("mock passthrough: {query:?}")))
    }
}

impl IndexerService for MockIndexerService {}

// --- reads (on the snapshot) ---

impl BlockRead for MockSnapshot {
    async fn tip(&self) -> Result<BlockId, BlockReadError> {
        self.chain
            .tip
            .ok_or(BlockReadError::NotServiceable(Capability::Blocks))
    }
    async fn block(&self, _at: BlockRef) -> Result<Option<Block>, BlockReadError> {
        Ok(None)
    }
    async fn block_header(&self, _at: BlockRef) -> Result<Option<BlockHeader>, BlockReadError> {
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
    async fn compact_block(&self, _at: BlockRef) -> Result<Option<CompactBlock>, BlockReadError> {
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
        _id: TransactionId,
    ) -> Result<Option<RawTransaction>, TxReadError> {
        Ok(None)
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
        _addr: &TransparentAddress,
        _range: HeightRange,
    ) -> Result<AddressBalance, AddressReadError> {
        Err(AddressReadError::NotServiceable(Capability::AddressHistory))
    }
    async fn unspent_outpoints(
        &self,
        _addr: &TransparentAddress,
    ) -> Result<Vec<Utxo>, AddressReadError> {
        Ok(Vec::new())
    }
    async fn deltas(
        &self,
        _addr: &TransparentAddress,
        _range: HeightRange,
    ) -> Result<Vec<AddressDelta>, AddressReadError> {
        Ok(Vec::new())
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
        _at: BlockRef,
    ) -> Result<Option<CompactBlock>, BlockReadError> {
        Ok(None)
    }
}

impl ChainInfoRead for MockSnapshot {
    async fn chain_info(&self) -> Result<ChainInfo, ReadError> {
        let estimated_height = self
            .chain
            .tip
            .map(|id| id.height)
            .unwrap_or(Height::try_from(0).expect("0 is a valid height"));
        Ok(ChainInfo {
            tip: self.chain.tip,
            estimated_height,
        })
    }
}

impl ChainSegment for MockSnapshot {
    fn pinned_tip(&self) -> Option<BlockId> {
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
    fn serviceable_range(&self) -> ServiceableRange {
        self.chain.serviceable.unwrap_or_else(|| {
            let zero = Height::try_from(0).expect("0 is a valid height");
            ServiceableRange {
                finalized_tip: zero,
                tip: zero,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{MockChain, MockIndexerService};
    use crate::{BlockRead, LightServeService, NodeRpcService, TakeSnapshot, WalletLibService};
    use zaino_core::{BlockHash, BlockId, Height};

    /// The one concrete engine satisfies every public profile with zero
    /// profile-specific impl code — proof the read-set + control blanket impls
    /// compose. Compile-time only; the body is a no-op.
    #[test]
    fn mock_satisfies_all_profiles() {
        fn assert_profiles<T: WalletLibService + LightServeService + NodeRpcService>() {}
        assert_profiles::<MockIndexerService>();
    }

    fn block_id(height: u32, tag: u8) -> BlockId {
        BlockId {
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
