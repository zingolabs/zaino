//! Steerable stand-ins for the three providers a chain view composes.
//!
//! # Why these are a feature and not `#[cfg(test)]`
//!
//! The convenient reason: a crate testing its own use of a chain view needs one
//! it can steer, and a second set of fakes downstream is how two suites come to
//! disagree about what the ports mean.
//!
//! The load-bearing reason: **these are the second implementation of the
//! ports.** `zaino-chain-store-zainodb` and `zaino-chain-head-service` are the
//! first. A port with only one implementation is that implementation's surface
//! with extra steps, and nothing detects the difference. Writing these against
//! the traits alone, in a crate depending on neither adapter, is what makes "a
//! consumer can bring their own store and head" compiler-checked rather than
//! asserted.
//!
//! # One chain, three views of it
//!
//! Every provider draws from the same [`Chain`], and each covers a range of it.
//! That is what makes coverage shapes explicit at the call site:
//!
//! ```ignore
//! let chain = Chain::of_length(1200);
//! let store = FakeStore::covering(&chain, 100);        // built to 100
//! let head  = FakeHead::covering(&chain, 900, 1100);   // window 900..=1100
//! let source = FakeSource::over(&chain);               // knows everything
//! // -> a hole at 101..=899, which the validator fills
//! ```

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex};

use futures::Stream;
use tokio::sync::{broadcast, watch};

use zaino_chain_head::{
    ChainHeadBlock, ChainHeadBlockIter, ChainHeadBlockService, ChainHeadError,
    ChainHeadFreezeEvents, ChainHeadSnapshot, ChainHeadTransactionLocations,
    ChainHeadTransactionService, ChainHeadTxPosition, SpenderLocation,
};
use zaino_chain_store::{
    ChainStoreError, ChainStoreFreezeSink, ChainStoreIngest, ChainStoreReader, ChainStoreService,
    ChainStoreSourceError, CompactBlockRead, FrozenBlock, MigrationState, PoolFilter, Provenance,
    SchemaVersion, SpenderRef, SpentOutputIndex, StoreCapabilities, StoreCapability, StoreSchema,
    StoreWatermark, StoredBlock, StoredBlockRead, StoredTx, StoredTxOut, TransactionIndex,
    TxOutSetAccumulator, TxOutSetIndex,
};
use zaino_component::{ComponentName, ComponentStatus, Health, Lifecycle, StatusSource};
use zaino_primitives::types::{
    rpc::{ChainTip, ChainTipStatus},
    AbsoluteChainWork, AddressBalance, AddressDelta, Block, BlockConfirmations, BlockHash,
    BlockHeader, BlockRef, BlockTreeSizes, BlockTxPosition, BlockVerbose, ChainMetadata,
    ChainStateEpoch, CompactDifficulty, EquihashNonce, EquihashSolution, Height, MerkleRoot,
    Outpoint, PreIndexCompactBlock, PreIndexCompactTx, RelativeChainWork, ShieldedPool,
    SingleBlockWork, SubtreeRoot, TransactionId, TransactionLocation, TreeRoots, TreeSize,
    Treestate, Utxo,
};
use zaino_source::{
    GetAddressBalanceError, GetAddressDeltasError, GetAddressTxidsError, GetAddressUtxosError,
    GetBlockByHashError, GetBlockError, GetBlockVerboseError, GetCommitmentTreeRootsError,
    GetSubtreeRootsError, GetTransactionError, GetTreestateByHashError, GetTreestateError,
    OneShotGetAddressBalance, OneShotGetAddressDeltas, OneShotGetAddressTxids,
    OneShotGetAddressUtxos, OneShotGetBlock, OneShotGetBlockByHash, OneShotGetBlockVerbose,
    OneShotGetCommitmentTreeRoots, OneShotGetPreIndexCompactBlock, OneShotGetRawBlock,
    OneShotGetRawBlockByHash, OneShotGetSubtreeRoots, OneShotGetTransaction, OneShotGetTreestate,
    OneShotGetTreestateByHash, QueryError, TransactionResponse,
};

// ***** The chain everything draws from *****

/// A chain the providers each cover part of.
#[derive(Debug, Clone)]
pub struct Chain {
    blocks: Vec<Block>,
}

impl Chain {
    /// A chain of `len` blocks, genesis at height 0.
    pub fn of_length(len: u32) -> Self {
        Self {
            blocks: (0..len).map(block_at).collect(),
        }
    }

    /// A chain from real blocks, ascending from genesis.
    ///
    /// Lets a suite drive the composition with the checked-in vectors rather
    /// than synthetic blocks, which is what proves the stitched output is
    /// correct and not merely well-shaped.
    pub fn from_blocks(blocks: Vec<Block>) -> Self {
        Self { blocks }
    }

    /// The block at `height`.
    pub fn block(&self, height: u32) -> Option<&Block> {
        self.blocks.get(height as usize)
    }

    /// The hash at `height`.
    pub fn hash(&self, height: u32) -> BlockHash {
        hash_of(height)
    }

    /// The chain's tip height.
    pub fn tip(&self) -> u32 {
        self.blocks.len().saturating_sub(1) as u32
    }
}

/// A height, for a test that does not care about the protocol limit.
pub fn height(h: u32) -> Height {
    Height::try_from(h).expect("test height is within the protocol limit")
}

/// The hash of the block at `h`.
///
/// Derived from the height so a test can predict a hash without holding the
/// block, and so blocks stay distinct across a long chain — a single repeated
/// byte would collide past 256.
pub fn hash_of(h: u32) -> BlockHash {
    let mut bytes = [0u8; 32];
    bytes[..4].copy_from_slice(&h.to_le_bytes());
    BlockHash::from(bytes)
}

/// A transaction id distinguishable by its first byte.
pub fn txid(tag: u8) -> TransactionId {
    TransactionId::from([tag; 32])
}

/// Commitment roots with nothing in them.
pub fn empty_tree_roots() -> TreeRoots {
    TreeRoots {
        sapling: None,
        orchard: None,
        ironwood: None,
    }
}

/// A block at `h`.
pub fn block_at(h: u32) -> Block {
    Block {
        header: BlockHeader {
            hash: hash_of(h),
            version: 4,
            prev_hash: hash_of(h.saturating_sub(1)),
            height: height(h),
            time: 0,
            merkle_root: MerkleRoot::from([0u8; 32]),
            block_commitments: zaino_primitives::types::BlockCommitments::from([0u8; 32]),
            bits: CompactDifficulty::try_from_bits(0x2007_ffff).expect("valid nBits"),
            nonce: EquihashNonce::from([0u8; 32]),
            // Regtest: 36 bytes rather than 1344, so a long chain costs
            // kilobytes instead of megabytes. Nothing here verifies work.
            solution: EquihashSolution::Regtest([0u8; 36]),
        },
        transactions: Vec::new(),
        chain_metadata: ChainMetadata {
            sapling_tree_size: TreeSize::ZERO,
            orchard_tree_size: TreeSize::ZERO,
            ironwood_tree_size: TreeSize::ZERO,
        },
    }
}

/// Absolute chainwork that increases with height.
/// Cumulative chainwork at `h`, with every block worth one unit.
///
/// Genesis included, so the total at `h` is `h + 1`. That is what makes the
/// chain-head rebase checkable against the store: the head accumulates one unit
/// per block too, and `chainwork(anchor) + work(B)` lands exactly on the value
/// the store holds for the same block.
fn chainwork_of(h: u32) -> AbsoluteChainWork {
    AbsoluteChainWork::new(core::num::NonZeroU128::MIN.saturating_add(u128::from(h)))
}

/// The same value, for a test asserting on the rebase.
pub fn chainwork_at(h: u32) -> AbsoluteChainWork {
    chainwork_of(h)
}

/// The stored projection of a block.
///
/// Carries the block's real transactions through, so a store fake serves what
/// the chain actually holds.
fn stored_from_block(block: &Block) -> StoredBlock {
    StoredBlock {
        header: block.header.clone(),
        transactions: block
            .transactions
            .iter()
            .map(|tx| StoredTx::transparent_only(PreIndexCompactTx::from(tx)))
            .collect(),
        tree_roots: empty_tree_roots(),
        chainwork: chainwork_of(u32::from(block.header.height)),
    }
}

/// The chain-head projection of a block.
fn head_from_block(block: &Block) -> ChainHeadBlock {
    ChainHeadBlock {
        reference: BlockRef {
            hash: block.header.hash,
            height: block.header.height,
        },
        parent_hash: block.header.prev_hash,
        // Placeholder. Work is anchor-relative, so it depends on where the
        // window floor is, which a single block does not know — `FakeHead`
        // assigns it once the window is known.
        work: RelativeChainWork::ZERO,
        block: block.clone(),
        tree_roots: empty_tree_roots(),
    }
}

/// The chain-head projection of a synthetic block at `h`.
fn head_block_at(h: u32) -> ChainHeadBlock {
    head_from_block(&block_at(h))
}

// ***** The finalised store *****

/// A store covering `[genesis, top]` of a chain.
#[derive(Debug, Clone)]
pub struct FakeStore {
    inner: Arc<FakeStoreInner>,
}

#[derive(Debug)]
struct FakeStoreInner {
    /// Behind a lock because a store that can be frozen into grows while it is
    /// being read, which is the whole point of the sync loop under test.
    blocks: Mutex<Vec<StoredBlock>>,
    /// The chain this store builds itself from when asked.
    ///
    /// Empty unless [`FakeStore::buildable_from`] supplied one, so a store that
    /// was never told how to build refuses rather than inventing blocks — the
    /// same distinction the real store draws between having a validator and
    /// having been pointed at a height.
    buildable: Vec<StoredBlock>,
    positions: HashMap<[u8; 32], BlockTxPosition>,
    spenders: HashMap<Outpoint, SpenderRef>,
    capabilities: StoreCapabilities,
    watermark: watch::Sender<StoreWatermark>,
}

impl FakeStore {
    /// A store built to `top`, holding *that chain's* blocks.
    ///
    /// Takes the chain rather than synthesising: a fake that fabricated its own
    /// blocks would agree with itself and with nothing else, and a projection
    /// bug would be invisible to every test driven through it.
    pub fn covering(chain: &Chain, top: u32) -> Self {
        Self::new(
            (0..=top)
                .filter_map(|height| chain.block(height))
                .map(stored_from_block)
                .collect(),
        )
    }

    /// A store holding nothing.
    pub fn empty() -> Self {
        Self::new(Vec::new())
    }

    fn new(blocks: Vec<StoredBlock>) -> Self {
        let watermark = StoreWatermark {
            tip: blocks.last().map(StoredBlock::reference),
            provenance: Provenance::Durable,
        };
        let (sender, _) = watch::channel(watermark);
        Self {
            inner: Arc::new(FakeStoreInner {
                blocks: Mutex::new(blocks),
                buildable: Vec::new(),
                positions: HashMap::new(),
                spenders: HashMap::new(),
                capabilities: StoreCapabilities::new(StoreCapability::ALL),
                watermark: sender,
            }),
        }
    }

    /// The same store offering only these capabilities.
    pub fn with_capabilities(
        mut self,
        capabilities: impl IntoIterator<Item = StoreCapability>,
    ) -> Self {
        let inner = Arc::make_mut(&mut self.inner);
        inner.capabilities = StoreCapabilities::new(capabilities);
        self
    }

    /// The same store reporting `txid` mined at `position`.
    pub fn with_transaction(mut self, txid: TransactionId, position: BlockTxPosition) -> Self {
        Arc::make_mut(&mut self.inner)
            .positions
            .insert(<[u8; 32]>::from(txid), position);
        self
    }

    /// The same store, able to build itself from `chain` up to `top`.
    ///
    /// What a real store's validator gives it. Without this a store has no way
    /// to close a gap, which is itself worth testing: a sync loop must report
    /// the failure rather than spin.
    pub fn buildable_from(mut self, chain: &Chain, top: u32) -> Self {
        Arc::make_mut(&mut self.inner).buildable = (0..=top)
            .filter_map(|height| chain.block(height))
            .map(stored_from_block)
            .collect();
        self
    }

    /// The heights this store now holds, for a test to assert on.
    pub fn heights(&self) -> Vec<u32> {
        self.held()
            .iter()
            .map(|block| u32::from(block.header.height))
            .collect()
    }

    /// The same store reporting `outpoint` spent by `spender`.
    pub fn with_spender(mut self, outpoint: Outpoint, spender: SpenderRef) -> Self {
        Arc::make_mut(&mut self.inner)
            .spenders
            .insert(outpoint, spender);
        self
    }

    /// Every block this store holds.
    fn held(&self) -> std::sync::MutexGuard<'_, Vec<StoredBlock>> {
        self.inner.blocks.lock().expect("fake store mutex poisoned")
    }

    fn watermark_now(&self) -> StoreWatermark {
        StoreWatermark {
            tip: self.held().last().map(StoredBlock::reference),
            provenance: Provenance::Durable,
        }
    }

    /// Publishes the watermark this store now has.
    ///
    /// Called by every path that writes, because coverage is derived from the
    /// watermark: a store that grew without saying so would leave a composer
    /// routing to the validator for heights it holds.
    fn publish_watermark(&self) {
        let watermark = self.watermark_now();
        let _ = self.inner.watermark.send(watermark);
    }

    /// Refuses a height above the watermark, as a real store does.
    ///
    /// Load-bearing rather than mere fidelity: it turns a composer that routes
    /// past its own coverage into a visible failure rather than a silent
    /// `None`.
    fn bounded(&self, height: Height) -> Result<(), ChainStoreError> {
        let watermark = self.watermark_now();
        if watermark.covers(height) {
            return Ok(());
        }
        Err(ChainStoreError::AboveWatermark {
            requested: height,
            watermark: watermark.tip.map_or(Height::GENESIS, |tip| tip.height),
        })
    }

    fn require(&self, capability: StoreCapability) -> Result<(), ChainStoreError> {
        if self.inner.capabilities.contains(capability) {
            return Ok(());
        }
        Err(ChainStoreError::Unavailable(capability))
    }

    /// Cloned rather than borrowed: the blocks live behind a lock now, and
    /// holding that lock across an `await` in a caller is how a fake acquires a
    /// deadlock a real store does not have.
    fn range(&self, start: Height, end: Height) -> Result<Vec<StoredBlock>, ChainStoreError> {
        if start > end {
            return Err(ChainStoreError::InvalidRange { start, end });
        }
        self.bounded(end)?;
        let lo = u32::from(start) as usize;
        let hi = u32::from(end) as usize;
        Ok(self.held().get(lo..=hi).unwrap_or(&[]).to_vec())
    }
}

/// Cloned when a builder method mutates a shared store.
///
/// Hand-written because `watch::Sender` is not `Clone`: a clone gets a fresh
/// channel carrying the same value, which is right — a copy of a store is not
/// the same store, and a subscriber to one should not see the other's changes.
impl Clone for FakeStoreInner {
    fn clone(&self) -> Self {
        Self {
            blocks: Mutex::new(
                self.blocks
                    .lock()
                    .expect("fake store mutex poisoned")
                    .clone(),
            ),
            buildable: self.buildable.clone(),
            positions: self.positions.clone(),
            spenders: self.spenders.clone(),
            capabilities: self.capabilities,
            watermark: watch::channel(*self.watermark.borrow()).0,
        }
    }
}

impl ChainStoreService for FakeStore {
    type Reader = FakeStore;

    fn reader(&self) -> Self::Reader {
        self.clone()
    }

    fn subscribe_watermark(&self) -> watch::Receiver<StoreWatermark> {
        self.inner.watermark.subscribe()
    }
}

impl ChainStoreReader for FakeStore {
    fn watermark(&self) -> StoreWatermark {
        self.watermark_now()
    }

    fn capabilities(&self) -> StoreCapabilities {
        self.inner.capabilities
    }

    async fn schema(&self) -> Result<StoreSchema, ChainStoreError> {
        Ok(StoreSchema {
            version: SchemaVersion {
                major: 1,
                minor: 0,
                patch: 0,
            },
            migration: MigrationState::Settled,
        })
    }

    async fn block_hash(&self, height: Height) -> Result<Option<BlockHash>, ChainStoreError> {
        self.bounded(height)?;
        Ok(self
            .held()
            .get(u32::from(height) as usize)
            .map(|block| block.header.hash))
    }

    async fn block_height(&self, hash: BlockHash) -> Result<Option<Height>, ChainStoreError> {
        Ok(self
            .held()
            .iter()
            .find(|block| block.header.hash == hash)
            .map(|block| block.header.height))
    }
}

/// Always ready: a fake has no database to be unhealthy about.
///
/// `FakeStore` is its own reader, so this one impl serves both ports — which is
/// also what a real store does, and the reason the two report the same
/// component rather than two.
impl StatusSource for FakeStore {
    fn status(&self) -> ComponentStatus {
        ComponentStatus::new(
            ComponentName("fake-chain-store"),
            Lifecycle::Ready,
            Health::Healthy,
        )
    }
}

/// Building from the chain this store was told it could build from.
///
/// Deliberately not "fetch whatever is asked for": a store that could always
/// build to any height would make a freeze gap unreachable, and the gap is the
/// path the sync loop exists to take.
impl ChainStoreIngest for FakeStore {
    async fn build_to(&self, target: Height) -> Result<(), ChainStoreSourceError> {
        let wanted = u32::from(target) as usize;
        let available = self.inner.buildable.len();
        if wanted >= available {
            return Err(ChainStoreSourceError::NotReady {
                message: format!(
                    "fake store can build to {} at most, not {target}",
                    available.saturating_sub(1),
                ),
                cause: None,
            });
        }

        let mut held = self.held();
        if held.len() <= wanted {
            *held = self.inner.buildable[..=wanted].to_vec();
        }
        drop(held);

        self.publish_watermark();
        Ok(())
    }

    async fn rewind_to(&self, height: Height) -> Result<(), ChainStoreError> {
        self.held().truncate(u32::from(height) as usize + 1);
        self.publish_watermark();
        Ok(())
    }

    async fn wait_until_built(&self) {}

    async fn shutdown(&self) -> Result<(), ChainStoreError> {
        Ok(())
    }
}

/// Appending at `tip + 1`, with the same three outcomes a real store has.
///
/// Skipping below the tip, refusing a gap, and deriving chainwork rather than
/// taking it are the contract the sync loop is written against, so a fake that
/// simply accepted everything would let a loop that never repairs anything
/// pass.
impl ChainStoreFreezeSink for FakeStore {
    async fn freeze(&self, blocks: &[FrozenBlock]) -> Result<(), ChainStoreError> {
        let mut held = self.held();

        for block in blocks {
            let expected = held
                .last()
                .map_or(0, |tip| u32::from(tip.header.height) + 1);
            let height = u32::from(block.header.height);

            if height < expected {
                continue;
            }
            if height > expected {
                let store_tip = held.last().map(|tip| tip.header.height);
                drop(held);
                return Err(ChainStoreError::FreezeGap {
                    store_tip,
                    first_frozen: block.header.height,
                });
            }

            // Derived here rather than taken from the caller, as the real
            // store does.
            held.push(StoredBlock {
                header: block.header.clone(),
                transactions: block.transactions.clone(),
                tree_roots: block.tree_roots.clone(),
                chainwork: chainwork_of(height),
            });
        }

        drop(held);
        self.publish_watermark();
        Ok(())
    }
}

impl StoredBlockRead for FakeStore {
    async fn blocks_chunk(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<StoredBlock>, ChainStoreError> {
        self.require(StoreCapability::StoredBlocks)?;
        self.range(start, end)
    }

    async fn blocks_stream(
        &self,
        start: Height,
        end: Height,
    ) -> Result<
        impl Stream<Item = Result<Vec<StoredBlock>, ChainStoreError>> + Send + use<>,
        ChainStoreError,
    > {
        let blocks = self.blocks_chunk(start, end).await?;
        Ok(futures::stream::once(async move { Ok(blocks) }))
    }
}

impl CompactBlockRead for FakeStore {
    async fn compact_chunk(
        &self,
        start: Height,
        end: Height,
        _pools: PoolFilter,
    ) -> Result<Vec<zaino_primitives::types::CompactBlock>, ChainStoreError> {
        self.require(StoreCapability::CompactBlocks)?;
        Ok(self.range(start, end)?.iter().map(compact_of).collect())
    }

    async fn compact_stream(
        &self,
        start: Height,
        end: Height,
        pools: PoolFilter,
    ) -> Result<
        impl Stream<Item = Result<Vec<zaino_primitives::types::CompactBlock>, ChainStoreError>>
            + Send
            + use<>,
        ChainStoreError,
    > {
        let blocks = self.compact_chunk(start, end, pools).await?;
        Ok(futures::stream::once(async move { Ok(blocks) }))
    }
}

/// The compact projection of a stored block.
fn compact_of(block: &StoredBlock) -> zaino_primitives::types::CompactBlock {
    zaino_primitives::types::CompactBlock {
        hash: block.header.hash,
        prev_hash: block.header.prev_hash,
        height: u32::from(block.header.height),
        time: block.header.time,
        bits: block.header.bits,
        transactions: block
            .transactions
            .iter()
            .map(|tx| tx.compact.clone())
            .collect(),
        chain_metadata: ChainMetadata {
            sapling_tree_size: TreeSize::ZERO,
            orchard_tree_size: TreeSize::ZERO,
            ironwood_tree_size: TreeSize::ZERO,
        },
    }
}

impl TransactionIndex for FakeStore {
    async fn tx_position(
        &self,
        txid: &TransactionId,
    ) -> Result<Option<BlockTxPosition>, ChainStoreError> {
        self.require(StoreCapability::Transactions)?;
        Ok(self.inner.positions.get(&<[u8; 32]>::from(*txid)).copied())
    }

    async fn txid_at(
        &self,
        position: BlockTxPosition,
    ) -> Result<Option<TransactionId>, ChainStoreError> {
        self.require(StoreCapability::Transactions)?;
        Ok(self
            .inner
            .positions
            .iter()
            .find(|(_, held)| **held == position)
            .map(|(txid, _)| TransactionId::from(*txid)))
    }
}

impl SpentOutputIndex for FakeStore {
    async fn outpoint_spenders(
        &self,
        outpoints: &[Outpoint],
    ) -> Result<Vec<Option<SpenderRef>>, ChainStoreError> {
        self.require(StoreCapability::SpentOutputs)?;
        Ok(outpoints
            .iter()
            .map(|outpoint| self.inner.spenders.get(outpoint).copied())
            .collect())
    }

    async fn previous_outputs(
        &self,
        outpoints: &[Outpoint],
    ) -> Result<Vec<Option<StoredTxOut>>, ChainStoreError> {
        self.require(StoreCapability::SpentOutputs)?;
        Ok(outpoints.iter().map(|_| None).collect())
    }

    async fn unspent_output(
        &self,
        _outpoint: Outpoint,
    ) -> Result<Option<StoredTxOut>, ChainStoreError> {
        self.require(StoreCapability::SpentOutputs)?;
        Ok(None)
    }

    async fn transparent_outputs(
        &self,
        _position: BlockTxPosition,
    ) -> Result<Option<Vec<StoredTxOut>>, ChainStoreError> {
        self.require(StoreCapability::SpentOutputs)?;
        Ok(None)
    }
}

impl TxOutSetIndex for FakeStore {
    async fn txout_set(&self) -> Result<TxOutSetAccumulator, ChainStoreError> {
        self.require(StoreCapability::TxOutSet)?;
        Ok(TxOutSetAccumulator::default())
    }
}

// ***** The chain head *****

/// A chain head retaining a window of a chain.
#[derive(Debug, Clone)]
pub struct FakeHead {
    snapshot: Arc<Mutex<Arc<FakeHeadSnapshot>>>,
    epoch: watch::Sender<ChainStateEpoch>,
    /// Blocks passing below the seam.
    ///
    /// A real `broadcast`, not a queue the test drains: lagging is part of this
    /// port's contract, and a fake that could not lag would let a sync loop
    /// that mishandles `Lagged` pass.
    frozen: broadcast::Sender<ChainHeadBlock>,
}

impl FakeHead {
    /// A head retaining `floor..=tip`.
    pub fn covering(chain: &Chain, floor: u32, tip: u32) -> Self {
        Self::new(FakeHeadSnapshot::new(
            (floor..=tip)
                .filter_map(|height| chain.block(height))
                .map(head_from_block)
                .collect(),
        ))
    }

    /// A head publishing this view.
    pub fn new(snapshot: FakeHeadSnapshot) -> Self {
        let epoch = snapshot.epoch();
        let (sender, _) = watch::channel(epoch);
        Self {
            snapshot: Arc::new(Mutex::new(Arc::new(snapshot))),
            epoch: sender,
            frozen: broadcast::channel(FROZEN_CAPACITY).0,
        }
    }

    /// Emits `block` as having fallen below the seam.
    ///
    /// Takes the chain rather than a hand-built block for the same reason
    /// [`FakeStore::covering`] does: a freeze the store then reads back must be
    /// the same block the reads would have served.
    pub fn freeze(&self, chain: &Chain, height: u32) {
        let Some(block) = chain.block(height) else {
            return;
        };
        let _ = self.frozen.send(head_from_block(block));
    }

    /// Publishes a new view, as an advance or a reorg would.
    ///
    /// The lever the coherence tests pull: a snapshot taken before this call
    /// must keep answering from the view it pinned.
    pub fn publish(&self, snapshot: FakeHeadSnapshot) {
        let epoch = snapshot.epoch();
        *self.snapshot.lock().expect("fake head mutex poisoned") = Arc::new(snapshot);
        let _ = self.epoch.send(epoch);
    }
}

/// How many frozen blocks a slow consumer may fall behind before it is told it
/// lagged. Small on purpose: a test that wants to provoke `Lagged` should not
/// have to send thousands of blocks to do it.
const FROZEN_CAPACITY: usize = 16;

impl ChainHeadFreezeEvents for FakeHead {
    fn subscribe_frozen(&self) -> broadcast::Receiver<ChainHeadBlock> {
        self.frozen.subscribe()
    }
}

impl ChainHeadBlockService for FakeHead {
    type Snapshot = FakeHeadSnapshot;

    fn current(&self) -> Arc<Self::Snapshot> {
        Arc::clone(&self.snapshot.lock().expect("fake head mutex poisoned"))
    }

    fn subscribe_updates(&self) -> watch::Receiver<ChainStateEpoch> {
        self.epoch.subscribe()
    }
}

/// An immutable window over a chain, with any competing branches beside it.
///
/// The two lists are separate because that distinction is the chain head's
/// whole reason for existing: a block can be *retained* without being
/// *canonical*, and a fake that conflated them would make every branch test
/// pass vacuously.
#[derive(Debug, Clone)]
pub struct FakeHeadSnapshot {
    /// The canonical chain, ascending.
    blocks: Vec<ChainHeadBlock>,
    /// Blocks retained on competing branches. Never canonical.
    branches: Vec<ChainHeadBlock>,
    /// The window floor: what every block's work counts from, its own work being zero.
    work_anchor: BlockRef,
    generation: u64,
    spenders: HashMap<Outpoint, SpenderLocation>,
}

/// The anchor-relative total `units` blocks above the anchor, one unit of work each.
fn relative_work(units: u128) -> RelativeChainWork {
    match core::num::NonZeroU128::new(units) {
        Some(units) => RelativeChainWork::ZERO
            .accumulate(SingleBlockWork::new(units))
            .expect("a test total fits"),
        None => RelativeChainWork::ZERO,
    }
}

impl FakeHeadSnapshot {
    /// A view over these blocks, taken as the canonical chain in order.
    ///
    /// Assigns each block its anchor-relative work rather than trusting what
    /// the caller put there. Work is measured from the window floor, whose own
    /// work is zero, so it is a fact about the window and not about a block in
    /// isolation — and one unit per block above the floor makes the rebase
    /// check exact against [`FakeStore`], whose chainwork at `h` is `h + 1`.
    pub fn new(mut blocks: Vec<ChainHeadBlock>) -> Self {
        let work_anchor = blocks
            .first()
            .map(|floor| floor.reference)
            .expect("a fake head window holds at least its floor");

        for (index, block) in blocks.iter_mut().enumerate() {
            block.work = relative_work(index as u128);
        }

        Self {
            blocks,
            work_anchor,
            branches: Vec::new(),
            generation: 0,
            spenders: HashMap::new(),
        }
    }

    /// The same view additionally retaining a block on a competing branch.
    ///
    /// Retained, so `block_by_hash` and `find_fork_point` find it — and never
    /// canonical, so `is_on_best_chain` and `best_block_by_height` do not.
    pub fn with_branch_block(mut self, mut block: ChainHeadBlock) -> Self {
        block.work = self.work_at(block.height());
        self.branches.push(block);
        self
    }

    /// The work a block at this height carries in this window.
    ///
    /// One unit per block above the anchor, matching what
    /// [`new`](Self::new) assigns along the canonical chain, so a competing
    /// block at a height weighs the same as the canonical one it competes
    /// with.
    fn work_at(&self, height: Height) -> RelativeChainWork {
        let above_anchor = u32::from(height).saturating_sub(u32::from(self.work_anchor.height));
        relative_work(u128::from(above_anchor))
    }

    /// A block on a competing branch at `h`, distinguished by `tag`.
    pub fn branch_block(h: u32, tag: u32) -> ChainHeadBlock {
        let mut block = block_at(h);
        block.header.hash = hash_of(tag);
        ChainHeadBlock {
            reference: BlockRef {
                hash: block.header.hash,
                height: block.header.height,
            },
            parent_hash: hash_of(h.saturating_sub(1)),
            // Placeholder, as in `head_from_block`: `with_branch_block`
            // assigns the work once the window it joins is known.
            work: RelativeChainWork::ZERO,
            block,
            tree_roots: empty_tree_roots(),
        }
    }

    /// A window over `floor..=tip` of the chain.
    pub fn covering(floor: u32, tip: u32) -> Self {
        Self::new((floor..=tip).map(head_block_at).collect())
    }

    /// The same view at a later generation.
    pub fn at_generation(mut self, generation: u64) -> Self {
        self.generation = generation;
        self
    }

    /// The same view reporting `outpoint` spent within the window.
    pub fn with_spender(mut self, outpoint: Outpoint, spender: SpenderLocation) -> Self {
        self.spenders.insert(outpoint, spender);
        self
    }

    fn tip_ref(&self) -> Option<BlockRef> {
        self.blocks.last().map(|block| block.reference)
    }
}

impl ChainHeadSnapshot for FakeHeadSnapshot {
    fn work_anchor(&self) -> BlockRef {
        self.work_anchor
    }

    fn best_tip(&self) -> BlockRef {
        self.tip_ref().unwrap_or(BlockRef {
            hash: hash_of(0),
            height: Height::GENESIS,
        })
    }

    fn epoch(&self) -> ChainStateEpoch {
        ChainStateEpoch {
            generation: self.generation,
            best_tip: self.best_tip(),
        }
    }

    fn block_by_hash(&self, hash: &BlockHash) -> Option<&ChainHeadBlock> {
        // Canonical and competing alike: retaining branches is what the chain
        // head is for.
        self.blocks
            .iter()
            .chain(self.branches.iter())
            .find(|block| block.hash() == *hash)
    }

    fn best_block_by_height(&self, height: Height) -> Option<&ChainHeadBlock> {
        self.blocks.iter().find(|block| block.height() == height)
    }

    fn is_on_best_chain(&self, block: BlockRef) -> bool {
        // Only the canonical list. Both halves of the reference matter: a block
        // whose height is canonical but whose hash is not is precisely a
        // competing block.
        self.blocks.iter().any(|held| held.reference == block)
    }

    fn find_fork_point(&self, hash: &BlockHash) -> Option<BlockRef> {
        let block = self.block_by_hash(hash)?;
        if self.is_on_best_chain(block.reference) {
            // A canonical block is its own fork point.
            return Some(block.reference);
        }
        // A competing block forks at its parent, when the window retains it.
        self.blocks
            .iter()
            .find(|held| held.hash() == block.parent_hash)
            .map(|held| held.reference)
    }

    fn chain_tips(&self) -> Vec<ChainTip> {
        self.tip_ref()
            .map(|tip| {
                vec![ChainTip {
                    height: tip.height,
                    hash: tip.hash,
                    branch_len: 0,
                    status: ChainTipStatus::Active,
                }]
            })
            .unwrap_or_default()
    }

    fn best_chain(&self) -> ChainHeadBlockIter<'_> {
        ChainHeadBlockIter::new(self.blocks.iter())
    }

    fn best_chain_blocks(
        &self,
        start: Height,
        end: Height,
    ) -> Result<ChainHeadBlockIter<'_>, ChainHeadError> {
        if start > end {
            return Err(ChainHeadError::InvalidRange { start, end });
        }
        Ok(ChainHeadBlockIter::new(self.blocks.iter().filter(
            move |block| block.height() >= start && block.height() <= end,
        )))
    }
}

impl ChainHeadTransactionService for FakeHeadSnapshot {
    fn transaction_locations(&self, txid: &TransactionId) -> ChainHeadTransactionLocations {
        let best_chain = self.blocks.iter().find_map(|block| {
            block
                .block
                .transactions
                .iter()
                .position(|tx| tx.txid == *txid)
                .map(|index| ChainHeadTxPosition {
                    block: block.reference,
                    tx_index: index as u32,
                })
        });

        // `#[non_exhaustive]`, so built through `Default` — the attribute
        // working as intended: a field added upstream must not silently take a
        // value this fake never considered.
        let mut locations = ChainHeadTransactionLocations::default();
        locations.best_chain = best_chain;
        locations
    }

    fn outpoint_spenders(&self, outpoints: &[Outpoint]) -> Vec<Option<SpenderLocation>> {
        outpoints
            .iter()
            .map(|outpoint| self.spenders.get(outpoint).copied())
            .collect()
    }
}

// ***** The validator *****

/// A question a [`FakeSource`] was asked.
///
/// Recorded so a test can assert *how* a read was answered and not only what it
/// returned — the difference between a by-height and a by-hash passthrough is
/// invisible in the result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceCall {
    /// A parsed block, by height.
    Block(Height),
    /// A parsed block, by hash.
    BlockByHash(BlockHash),
    /// Consensus bytes, by height.
    RawBlock(Height),
    /// Consensus bytes, by hash.
    RawBlockByHash(BlockHash),
    /// A compact projection, by height.
    CompactBlock(Height),
    /// Cumulative chain state at a height.
    BlockVerbose(Height),
    /// Commitment roots, by block hash.
    TreeRoots(BlockHash),
    /// A transaction, by id.
    Transaction(TransactionId),
    /// A treestate, by height.
    Treestate(Height),
    /// A treestate, by block hash.
    TreestateByHash(BlockHash),
    /// Subtree roots for a pool.
    SubtreeRoots(ShieldedPool),
    /// Any transparent-address question.
    Address,
}

/// A validator that knows the whole chain.
///
/// Which is what a validator is — the fakes for the two tiers cover part of a
/// chain, and this one covers all of it. That asymmetry is the point: it is
/// what makes a hole between the tiers fillable.
#[derive(Debug, Clone)]
pub struct FakeSource {
    chain: Arc<Chain>,
    calls: Arc<Mutex<Vec<SourceCall>>>,
    /// Requests in flight right now, and the most ever seen at once.
    ///
    /// The only way to observe the concurrency bound: it is invisible in every
    /// return value, and a view that ignored its permit pool would pass every
    /// other test in this crate while melting a real validator under load.
    in_flight: Arc<std::sync::atomic::AtomicUsize>,
    peak_in_flight: Arc<std::sync::atomic::AtomicUsize>,
}

/// Counts one request for as long as it is in flight.
struct InFlight(Arc<std::sync::atomic::AtomicUsize>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

impl FakeSource {
    /// A validator serving this chain.
    pub fn over(chain: &Chain) -> Self {
        Self {
            chain: Arc::new(chain.clone()),
            calls: Arc::new(Mutex::new(Vec::new())),
            in_flight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            peak_in_flight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// The most requests this validator ever had in flight at once.
    pub fn peak_in_flight(&self) -> usize {
        self.peak_in_flight
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Marks a request in flight until the guard drops.
    fn enter(&self) -> InFlight {
        use std::sync::atomic::Ordering;
        let now = self.in_flight.fetch_add(1, Ordering::AcqRel) + 1;
        self.peak_in_flight.fetch_max(now, Ordering::AcqRel);
        InFlight(Arc::clone(&self.in_flight))
    }

    /// Records a call and marks it in flight.
    ///
    /// Yields to the runtime so concurrent fetches genuinely overlap; without
    /// it every fake request would complete before the next began and the peak
    /// would always read one.
    async fn begin(&self, call: SourceCall) -> InFlight {
        self.record(call);
        let guard = self.enter();
        tokio::task::yield_now().await;
        guard
    }

    /// Every question this validator has been asked, in order.
    pub fn calls(&self) -> Vec<SourceCall> {
        self.calls.lock().expect("call log mutex poisoned").clone()
    }

    fn record(&self, call: SourceCall) {
        self.calls
            .lock()
            .expect("call log mutex poisoned")
            .push(call);
    }

    fn at(&self, height: Height) -> Option<&Block> {
        self.chain.block(u32::from(height))
    }

    fn height_of(&self, hash: BlockHash) -> Option<u32> {
        (0..=self.chain.tip()).find(|h| hash_of(*h) == hash)
    }
}

impl OneShotGetBlock for FakeSource {
    async fn get_block(&self, height: Height) -> Result<Block, QueryError<GetBlockError>> {
        let _in_flight = self.begin(SourceCall::Block(height)).await;
        self.at(height)
            .cloned()
            .ok_or(QueryError::Domain(GetBlockError::HeightNotFound(height)))
    }
}

impl OneShotGetBlockByHash for FakeSource {
    async fn get_block_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Block, QueryError<GetBlockByHashError>> {
        let _in_flight = self.begin(SourceCall::BlockByHash(hash)).await;
        self.height_of(hash)
            .and_then(|h| self.chain.block(h).cloned())
            .ok_or(QueryError::Domain(GetBlockByHashError::NotFound(hash)))
    }
}

impl OneShotGetRawBlock for FakeSource {
    async fn get_raw_block(&self, height: Height) -> Result<Vec<u8>, QueryError<GetBlockError>> {
        let _in_flight = self.begin(SourceCall::RawBlock(height)).await;
        self.at(height)
            .map(|block| <[u8; 32]>::from(block.header.hash).to_vec())
            .ok_or(QueryError::Domain(GetBlockError::HeightNotFound(height)))
    }
}

impl OneShotGetRawBlockByHash for FakeSource {
    async fn get_raw_block_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Vec<u8>, QueryError<GetBlockByHashError>> {
        let _in_flight = self.begin(SourceCall::RawBlockByHash(hash)).await;
        self.height_of(hash)
            .map(|_| <[u8; 32]>::from(hash).to_vec())
            .ok_or(QueryError::Domain(GetBlockByHashError::NotFound(hash)))
    }
}

impl OneShotGetPreIndexCompactBlock for FakeSource {
    async fn get_pre_index_compact_block(
        &self,
        height: Height,
    ) -> Result<PreIndexCompactBlock, QueryError<GetBlockError>> {
        let _in_flight = self.begin(SourceCall::CompactBlock(height)).await;
        self.at(height)
            .map(PreIndexCompactBlock::from)
            .ok_or(QueryError::Domain(GetBlockError::HeightNotFound(height)))
    }
}

impl OneShotGetBlockVerbose for FakeSource {
    async fn get_block_verbose(
        &self,
        height: Height,
    ) -> Result<BlockVerbose, QueryError<GetBlockVerboseError>> {
        let _in_flight = self.begin(SourceCall::BlockVerbose(height)).await;
        self.at(height)
            .map(|_| BlockVerbose {
                confirmations: BlockConfirmations::Confirmed(NonZeroU32::MIN),
                difficulty: 1.0,
                // Zebra does not track cumulative work per height
                // (ZcashFoundation/zebra#7109), so a real validator answers
                // `None` here too — which is why a chain view cannot fill
                // chainwork across a hole.
                chainwork: None,
                chain_supply: None,
                value_pools: Vec::new(),
                tree_sizes: BlockTreeSizes {
                    sapling: TreeSize::ZERO,
                    orchard: TreeSize::ZERO,
                    ironwood: TreeSize::ZERO,
                },
                next_block_hash: None,
            })
            .ok_or(QueryError::Domain(GetBlockVerboseError::HeightNotFound(
                height,
            )))
    }
}

impl OneShotGetCommitmentTreeRoots for FakeSource {
    async fn get_commitment_tree_roots(
        &self,
        block: BlockHash,
    ) -> Result<TreeRoots, QueryError<GetCommitmentTreeRootsError>> {
        let _in_flight = self.begin(SourceCall::TreeRoots(block)).await;
        Ok(empty_tree_roots())
    }
}

impl OneShotGetTransaction for FakeSource {
    async fn get_transaction(
        &self,
        txid: TransactionId,
    ) -> Result<TransactionResponse, QueryError<GetTransactionError>> {
        self.record(SourceCall::Transaction(txid));
        Err(QueryError::Domain(GetTransactionError::NotFound(txid)))
    }
}

impl OneShotGetTreestate for FakeSource {
    async fn get_treestate(
        &self,
        height: Height,
    ) -> Result<Treestate, QueryError<GetTreestateError>> {
        self.record(SourceCall::Treestate(height));
        self.at(height).map(treestate_of).ok_or(QueryError::Domain(
            GetTreestateError::HeightNotFound(height),
        ))
    }
}

impl OneShotGetTreestateByHash for FakeSource {
    async fn get_treestate_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Treestate, QueryError<GetTreestateByHashError>> {
        self.record(SourceCall::TreestateByHash(hash));
        self.height_of(hash)
            .and_then(|h| self.chain.block(h))
            .map(treestate_of)
            .ok_or(QueryError::Domain(GetTreestateByHashError::BlockNotFound(
                hash,
            )))
    }
}

/// The treestate a block leaves behind, as the fake validator reports it.
fn treestate_of(block: &Block) -> Treestate {
    Treestate {
        block_hash: block.header.hash,
        height: block.header.height,
        time: block.header.time,
        sapling: None,
        orchard: None,
        ironwood: None,
    }
}

impl OneShotGetSubtreeRoots for FakeSource {
    async fn get_subtree_roots(
        &self,
        pool: ShieldedPool,
        _start_index: u16,
        _limit: Option<u16>,
    ) -> Result<Vec<SubtreeRoot>, QueryError<GetSubtreeRootsError>> {
        self.record(SourceCall::SubtreeRoots(pool));
        Ok(Vec::new())
    }
}

impl OneShotGetAddressBalance for FakeSource {
    async fn get_address_balance(
        &self,
        _addresses: Vec<String>,
    ) -> Result<AddressBalance, QueryError<GetAddressBalanceError>> {
        self.record(SourceCall::Address);
        Ok(AddressBalance {
            balance: zaino_primitives::types::Zatoshis::ZERO,
            received: zaino_primitives::types::ZatoshisFlowSum::from_summed(0),
        })
    }
}

impl OneShotGetAddressUtxos for FakeSource {
    async fn get_address_utxos(
        &self,
        _addresses: Vec<String>,
    ) -> Result<Vec<Utxo>, QueryError<GetAddressUtxosError>> {
        self.record(SourceCall::Address);
        Ok(Vec::new())
    }
}

impl OneShotGetAddressTxids for FakeSource {
    async fn get_address_txids(
        &self,
        _addresses: Vec<String>,
        _start: Height,
        _end: Height,
    ) -> Result<Vec<TransactionId>, QueryError<GetAddressTxidsError>> {
        self.record(SourceCall::Address);
        Ok(Vec::new())
    }
}

impl OneShotGetAddressDeltas for FakeSource {
    async fn get_address_deltas(
        &self,
        _addresses: Vec<String>,
        _start: Height,
        _end: Height,
    ) -> Result<Vec<AddressDelta>, QueryError<GetAddressDeltasError>> {
        self.record(SourceCall::Address);
        Ok(Vec::new())
    }
}

/// A transaction location, for tests that need one.
pub fn mempool_location() -> TransactionLocation {
    TransactionLocation::Mempool
}

// ***** A store that builds only what a wallet needs *****

/// A store offering the core reads and nothing optional.
///
/// Deliberately implements neither [`SpentOutputIndex`] nor [`TxOutSetIndex`],
/// so a view composed over it does not implement
/// [`SpendRead`](crate::SpendRead) or [`TxOutSetRead`](crate::TxOutSetRead)
/// either — absent at compile time rather than failing at runtime.
///
/// This is the case `zaino-chain-store` split its ports for, and the one an
/// earlier version of this crate could not construct at all: it demanded every
/// index at once, so a wallet-only deployment could not build a chain view.
/// A type that exists only to be *missing* impls is the only way to keep that
/// from regressing.
#[derive(Debug, Clone)]
pub struct MinimalStore(FakeStore);

impl MinimalStore {
    /// A minimal store built to `top`.
    pub fn covering(chain: &Chain, top: u32) -> Self {
        Self(FakeStore::covering(chain, top))
    }
}

impl ChainStoreService for MinimalStore {
    type Reader = MinimalStore;

    fn reader(&self) -> Self::Reader {
        self.clone()
    }

    fn subscribe_watermark(&self) -> watch::Receiver<StoreWatermark> {
        self.0.subscribe_watermark()
    }
}

impl ChainStoreReader for MinimalStore {
    fn watermark(&self) -> StoreWatermark {
        ChainStoreReader::watermark(&self.0)
    }

    fn capabilities(&self) -> StoreCapabilities {
        // What it genuinely has: no spend index, no accumulator.
        StoreCapabilities::new([
            StoreCapability::Core,
            StoreCapability::StoredBlocks,
            StoreCapability::CompactBlocks,
            StoreCapability::Transactions,
        ])
    }

    async fn schema(&self) -> Result<StoreSchema, ChainStoreError> {
        self.0.schema().await
    }

    async fn block_hash(&self, height: Height) -> Result<Option<BlockHash>, ChainStoreError> {
        self.0.block_hash(height).await
    }

    async fn block_height(&self, hash: BlockHash) -> Result<Option<Height>, ChainStoreError> {
        self.0.block_height(hash).await
    }
}

/// The store it wraps, reporting for itself.
impl StatusSource for MinimalStore {
    fn status(&self) -> ComponentStatus {
        StatusSource::status(&self.0)
    }
}

impl StoredBlockRead for MinimalStore {
    async fn blocks_chunk(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<StoredBlock>, ChainStoreError> {
        self.0.blocks_chunk(start, end).await
    }

    async fn blocks_stream(
        &self,
        start: Height,
        end: Height,
    ) -> Result<
        impl Stream<Item = Result<Vec<StoredBlock>, ChainStoreError>> + Send + use<>,
        ChainStoreError,
    > {
        let blocks = self.blocks_chunk(start, end).await?;
        Ok(futures::stream::once(async move { Ok(blocks) }))
    }
}

impl CompactBlockRead for MinimalStore {
    async fn compact_chunk(
        &self,
        start: Height,
        end: Height,
        pools: PoolFilter,
    ) -> Result<Vec<zaino_primitives::types::CompactBlock>, ChainStoreError> {
        self.0.compact_chunk(start, end, pools).await
    }

    async fn compact_stream(
        &self,
        start: Height,
        end: Height,
        pools: PoolFilter,
    ) -> Result<
        impl Stream<Item = Result<Vec<zaino_primitives::types::CompactBlock>, ChainStoreError>>
            + Send
            + use<>,
        ChainStoreError,
    > {
        let blocks = self.compact_chunk(start, end, pools).await?;
        Ok(futures::stream::once(async move { Ok(blocks) }))
    }
}

impl TransactionIndex for MinimalStore {
    async fn tx_position(
        &self,
        txid: &TransactionId,
    ) -> Result<Option<BlockTxPosition>, ChainStoreError> {
        self.0.tx_position(txid).await
    }

    async fn txid_at(
        &self,
        position: BlockTxPosition,
    ) -> Result<Option<TransactionId>, ChainStoreError> {
        self.0.txid_at(position).await
    }
}
