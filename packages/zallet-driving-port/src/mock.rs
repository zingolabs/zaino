//! Scriptable in-memory implementation of the port.
//!
//! The port's first implementation (decision 8 of the design review):
//! a deterministic chain held in memory, to be scripted — advanced and
//! reorganized — by tests. Snapshots hold an `Arc` of the chain as of
//! their creation, so the pinning guarantee falls out of the structure:
//! a scripted mutation swaps the `Arc`, and live snapshots keep the old
//! one.

use std::ops::Range;
use std::sync::{Arc, RwLock};

use futures::Stream;
use zaino_primitives::types::{
    BlockHash, BlockTime, ConsensusBranchId, Height, TransactionHash, TransparentAddress,
};

use crate::block_id::BlockId;
use crate::block_locator::BlockLocator;
use crate::broadcast_transaction::{BroadcastTransaction, BroadcastTransactionError};
use crate::error::{BackendError, PortError};
use crate::find_fork_point::{FindForkPoint, FindForkPointError};
use crate::get_address_transaction_ids::{GetAddressTransactionIds, GetAddressTransactionIdsError};
use crate::get_address_unspent_outpoints::{
    GetAddressUnspentOutpoints, GetAddressUnspentOutpointsError,
};
use crate::get_health::{GetHealth, GetHealthError, Health};
use crate::get_mined_transaction::{GetMinedTransaction, GetMinedTransactionError};
use crate::get_outpoint_spend_status::{GetOutpointSpendStatus, GetOutpointSpendStatusError};
use crate::get_raw_block::{GetRawBlock, GetRawBlockError};
use crate::get_raw_block_header::{GetRawBlockHeader, GetRawBlockHeaderError};
use crate::get_reported_upgrades::{GetReportedUpgrades, GetReportedUpgradesError};
use crate::get_transaction_status::{GetTransactionStatus, GetTransactionStatusError};
use crate::get_treestate::{GetTreestate, GetTreestateError};
use crate::hash_for_height::{GetHashForHeight, GetHashForHeightError};
use crate::height_for_hash::{GetHeightForHash, GetHeightForHashError};
use crate::mempool_transaction::MempoolTransaction;
use crate::mined_transaction::MinedTransaction;
use crate::outpoint::Outpoint;
use crate::pinned_tip::GetPinnedTip;
use crate::raw::{RawBlock, RawBlockHeader, RawTransaction, RawTreeFrontier};
use crate::reported_upgrade::{ReportedUpgrade, UpgradeStatus};
use crate::shut_down::ShutDown;
use crate::spend_status::SpendStatus;
use crate::stream_raw_blocks::{StreamRawBlocks, StreamRawBlocksError};
use crate::subscribe_to_mempool::SubscribeToMempool;
use crate::subscribe_to_tip_changes::SubscribeToTipChanges;
use crate::take_snapshot::{TakeSnapshot, TakeSnapshotError};
use crate::transaction_status::TransactionStatus;
use crate::treestate_at::TreestateAt;

/// Deterministic hash for a scripted block: derived from the height and
/// a fork tag, so distinct chain branches never collide. The tag byte
/// at the end also keeps genesis distinct from the `BlockHash::ZERO`
/// sentinel.
///
/// Layout: bytes 0..4 the height, bytes 4..8 the fork tag (both
/// little-endian), byte 31 the id-family tag. [`fork_tag_of`] is the
/// inverse for the fork field — keep the two in step.
fn scripted_hash(height: u32, fork: u32) -> BlockHash {
    let mut bytes = [0u8; 32];
    bytes[..4].copy_from_slice(&height.to_le_bytes());
    bytes[4..8].copy_from_slice(&fork.to_le_bytes());
    bytes[31] = 0x5C;
    BlockHash::from(bytes)
}

/// The fork tag a scripted block hash carries — the inverse of the
/// bytes 4..8 field [`scripted_hash`] writes.
fn fork_tag_of(hash: BlockHash) -> u32 {
    let bytes = <[u8; 32]>::from(hash);
    u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]])
}

/// The txid of the one transaction the script mines in `block`,
/// recovered from the block's own hash. All txid derivation goes
/// through here, so the hash layout is read in exactly one place.
fn txid_of_block(block: BlockId) -> TransactionHash {
    scripted_txid(u32::from(block.height), fork_tag_of(block.hash))
}

/// Deterministic header bytes for a scripted block. Not a real
/// consensus serialization — just distinct per block.
fn scripted_header_bytes(block: BlockId) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(40);
    bytes.extend_from_slice(b"hdr:");
    bytes.extend_from_slice(&<[u8; 32]>::from(block.hash));
    bytes.extend_from_slice(&u32::from(block.height).to_le_bytes());
    bytes
}

/// Deterministic block bytes for a scripted block. The header bytes are
/// the prefix, mirroring real consensus serialization — the conformance
/// kit checks that relation.
fn scripted_block_bytes(block: BlockId) -> Vec<u8> {
    let mut bytes = scripted_header_bytes(block);
    bytes.extend_from_slice(b"body");
    bytes
}

/// Deterministic txid of the one scripted transaction each block
/// carries, in [`scripted_hash`]'s layout. The tag byte differs from
/// the block-hash tag, so txids and block hashes never collide.
fn scripted_txid(height: u32, fork: u32) -> TransactionHash {
    let mut bytes = [0u8; 32];
    bytes[..4].copy_from_slice(&height.to_le_bytes());
    bytes[4..8].copy_from_slice(&fork.to_le_bytes());
    bytes[31] = 0x7B;
    TransactionHash::from(bytes)
}

/// Deterministic transaction bytes for a scripted block's transaction.
fn scripted_tx_bytes(block: BlockId) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(36);
    bytes.extend_from_slice(b"tx:");
    bytes.extend_from_slice(&<[u8; 32]>::from(block.hash));
    bytes
}

/// The one consensus branch id every scripted block is mined under.
fn scripted_branch_id() -> ConsensusBranchId {
    ConsensusBranchId::from(0x5C5C_5C5C)
}

/// Deterministic header time for a scripted block.
fn scripted_block_time(height: Height) -> BlockTime {
    1_700_000_000 + u32::from(height) * 75
}

/// Deterministic frontier bytes for one pool's tree as of a block.
fn scripted_frontier_bytes(pool_tag: u8, block: BlockId) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(38);
    bytes.extend_from_slice(b"tree:");
    bytes.push(pool_tag);
    bytes.extend_from_slice(&<[u8; 32]>::from(block.hash));
    bytes
}

/// Deterministic txid of the `sequence`-th queued mempool transaction.
/// The tag byte differs from mined txids and block hashes.
fn scripted_mempool_txid(sequence: u32) -> TransactionHash {
    let mut bytes = [0u8; 32];
    bytes[..4].copy_from_slice(&sequence.to_le_bytes());
    bytes[31] = 0x9D;
    TransactionHash::from(bytes)
}

/// Deterministic transaction bytes for a queued mempool transaction.
fn scripted_mempool_tx_bytes(txid: TransactionHash) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(43);
    bytes.extend_from_slice(b"mempooltx:");
    bytes.extend_from_slice(&<[u8; 32]>::from(txid));
    bytes
}

/// One step of the splitmix64 output function — used to expand the
/// broadcast digest into txid bytes.
fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut mixed = *state;
    mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    mixed ^ (mixed >> 31)
}

/// Deterministic content-derived txid for a broadcast transaction —
/// an FNV-1a digest expanded through splitmix64, standing in for the
/// real double-SHA256. Distinct contents collide only with the
/// negligible probability of a 64-bit digest collision. The tag byte
/// differs from every other scripted id family.
fn broadcast_txid(transaction: &[u8]) -> TransactionHash {
    const FNV_OFFSET_BASIS: u64 = 0xCBF2_9CE4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;
    let mut digest = FNV_OFFSET_BASIS;
    for &byte in transaction {
        digest = (digest ^ u64::from(byte)).wrapping_mul(FNV_PRIME);
    }

    let mut bytes = [0u8; 32];
    let mut state = digest;
    for chunk in bytes.chunks_exact_mut(8).take(3) {
        chunk.copy_from_slice(&splitmix64(&mut state).to_le_bytes());
    }
    bytes[31] = 0xB0;
    TransactionHash::from(bytes)
}

/// The chain as the script currently tells it.
struct ScriptedState {
    /// The best chain, ascending by height.
    blocks: Arc<Vec<BlockId>>,
    /// Blocks orphaned by scripted reorgs.
    orphaned: Arc<Vec<BlockId>>,
    /// The fork tag new blocks are minted on; bumped by every reorg.
    fork: u32,
    /// The mempool, in arrival order. Tags are maintained on every
    /// chain movement, modeling an engine that revalidates instantly.
    mempool: Vec<MempoolTransaction>,
    /// Sequence number of the next queued mempool transaction.
    mempool_sequence: u32,
    /// Whether the port has been shut down.
    shut_down: bool,
}

impl ScriptedState {
    /// Retag the mempool with the current tip — the scripted stand-in
    /// for revalidation after chain movement.
    fn revalidate_mempool(&mut self) {
        if let Some(tip) = self.blocks.last().copied() {
            for entry in &mut self.mempool {
                entry.validated_against = tip;
            }
        }
    }
}

/// A scriptable in-memory chain implementing the port.
pub struct ScriptedChain {
    state: RwLock<ScriptedState>,
    tip_events: tokio::sync::watch::Sender<Option<BlockId>>,
    mempool_events: tokio::sync::watch::Sender<()>,
}

impl ScriptedChain {
    /// The height the script activates Ironwood at: below it the
    /// Ironwood frontier is absent (an empty tree, zcash/zallet#455),
    /// from it on it is present. Sapling and Orchard are active from
    /// genesis.
    pub const IRONWOOD_ACTIVATION: u32 = 5;

    /// A linear chain of `length` blocks on fork tag `0`, heights `0`
    /// (genesis) through `length - 1`. A length of zero models an
    /// engine with no consistent view yet.
    pub fn with_linear_chain(length: u32) -> Self {
        let blocks: Vec<BlockId> = (0..length)
            .map(|height| BlockId {
                height: Height::try_from(height).expect("scripted height within protocol limit"),
                hash: scripted_hash(height, 0),
            })
            .collect();
        let tip = blocks.last().copied();
        Self {
            state: RwLock::new(ScriptedState {
                blocks: Arc::new(blocks),
                orphaned: Arc::new(Vec::new()),
                fork: 0,
                mempool: Vec::new(),
                mempool_sequence: 0,
                shut_down: false,
            }),
            tip_events: tokio::sync::watch::channel(tip).0,
            mempool_events: tokio::sync::watch::channel(()).0,
        }
    }

    /// Script: queue a transaction into the mempool, validated against
    /// the current tip. Returns its txid.
    pub fn queue_mempool_transaction(&self) -> TransactionHash {
        let mut state = self.state.write().expect("scripted chain lock poisoned");
        let tip = *state
            .blocks
            .last()
            .expect("a scripted mempool needs a tip to validate against");
        let txid = scripted_mempool_txid(state.mempool_sequence);
        state.mempool_sequence += 1;
        state.mempool.push(MempoolTransaction {
            raw: RawTransaction::new(scripted_mempool_tx_bytes(txid)),
            txid,
            branch_id: scripted_branch_id(),
            validated_against: tip,
        });
        drop(state);
        self.mempool_events.send_replace(());
        txid
    }

    /// Script: extend the best chain by `count` blocks on the current
    /// fork tag.
    pub fn advance(&self, count: u32) {
        if count == 0 {
            return;
        }
        let mut state = self.state.write().expect("scripted chain lock poisoned");
        let start = state
            .blocks
            .last()
            .map_or(0, |tip| u32::from(tip.height) + 1);
        let fork = state.fork;
        // Clone-on-write: copies only while a snapshot still shares the
        // chain — exactly the copy pinning requires — and appends in
        // place otherwise.
        let blocks = Arc::make_mut(&mut state.blocks);
        for height in start..start + count {
            blocks.push(BlockId {
                height: Height::try_from(height).expect("scripted height within protocol limit"),
                hash: scripted_hash(height, fork),
            });
        }
        state.revalidate_mempool();
        self.tip_events.send_replace(state.blocks.last().copied());
        self.mempool_events.send_replace(());
    }

    /// Script: reorganize the chain — orphan the last `depth` blocks
    /// and mine `depth + 1` blocks on a new fork tag, so the new tip
    /// sits one above the old.
    pub fn reorg(&self, depth: u32) {
        let mut state = self.state.write().expect("scripted chain lock poisoned");
        assert!(
            (depth as usize) < state.blocks.len(),
            "a scripted reorg must keep at least genesis"
        );
        state.fork = state
            .fork
            .checked_add(1)
            .expect("scripted reorg count within u32");
        let fork = state.fork;
        let keep = state.blocks.len() - depth as usize;
        // Clone-on-write, as in `advance`: live snapshots keep the old
        // chain (and old orphan set); without them the edit is in place.
        let orphaned_tail = {
            let blocks = Arc::make_mut(&mut state.blocks);
            let orphaned_tail = blocks.split_off(keep);
            let start = blocks.last().map_or(0, |base| u32::from(base.height) + 1);
            for height in start..=start + depth {
                blocks.push(BlockId {
                    height: Height::try_from(height)
                        .expect("scripted height within protocol limit"),
                    hash: scripted_hash(height, fork),
                });
            }
            orphaned_tail
        };
        Arc::make_mut(&mut state.orphaned).extend(orphaned_tail);
        state.revalidate_mempool();
        self.tip_events.send_replace(state.blocks.last().copied());
        self.mempool_events.send_replace(());
    }

    /// The hash the script assigned to `height` on fork tag `fork`.
    pub fn hash_of(height: u32, fork: u32) -> BlockHash {
        scripted_hash(height, fork)
    }

    /// The txid of the one transaction the script mines in the block
    /// at `height` on fork tag `fork`.
    pub fn txid_of(height: u32, fork: u32) -> TransactionHash {
        scripted_txid(height, fork)
    }

    /// The one transparent address the script pays.
    ///
    /// Every scripted transaction creates one output (index 0) at this
    /// address, and the transaction at each height spends the previous
    /// height's output — a spend chain leaving only the tip's output
    /// unspent.
    pub fn address() -> TransparentAddress {
        TransparentAddress::new("t1ScriptedChainAddress".into())
    }
}

/// A pinned view of a [`ScriptedChain`].
///
/// Holds the chain — and the orphan set — as of snapshot creation;
/// scripted mutations after that point never reach it.
#[derive(Clone)]
pub struct ScriptedSnapshot {
    blocks: Arc<Vec<BlockId>>,
    orphaned: Arc<Vec<BlockId>>,
}

impl TakeSnapshot for ScriptedChain {
    type Snapshot = ScriptedSnapshot;

    fn take_snapshot(
        &self,
    ) -> impl std::future::Future<Output = Result<ScriptedSnapshot, PortError<TakeSnapshotError>>> + Send
    {
        let result = {
            let state = self.state.read().expect("scripted chain lock poisoned");
            if state.shut_down {
                Err(BackendError::fatal("the port is shut down").into())
            } else if state.blocks.is_empty() {
                Err(PortError::Domain(TakeSnapshotError::NotReady))
            } else {
                Ok(ScriptedSnapshot {
                    blocks: Arc::clone(&state.blocks),
                    orphaned: Arc::clone(&state.orphaned),
                })
            }
        };
        std::future::ready(result)
    }
}

impl GetReportedUpgrades for ScriptedChain {
    fn get_reported_upgrades(
        &self,
    ) -> impl std::future::Future<
        Output = Result<Vec<ReportedUpgrade>, PortError<GetReportedUpgradesError>>,
    > + Send {
        // The scripted schedule: one upgrade in force from genesis,
        // and Ironwood at its scripted activation — Pending until the
        // tip reaches it.
        let result = (|| {
            let tip_height = {
                let state = self.state.read().expect("scripted chain lock poisoned");
                if state.shut_down {
                    return Err(BackendError::fatal("the port is shut down").into());
                }
                state.blocks.last().map(|tip| u32::from(tip.height))
            };
            let status_at = |activation: u32| match tip_height {
                Some(tip) if tip >= activation => UpgradeStatus::Active,
                _ => UpgradeStatus::Pending,
            };
            Ok(vec![
                ReportedUpgrade {
                    branch_id: scripted_branch_id(),
                    name: "Scripted".into(),
                    activation_height: Height::GENESIS,
                    status: status_at(0),
                },
                ReportedUpgrade {
                    branch_id: ConsensusBranchId::from(0x6D6D_6D6D),
                    name: "ScriptedIronwood".into(),
                    activation_height: Height::try_from(Self::IRONWOOD_ACTIVATION)
                        .expect("scripted activation within protocol limit"),
                    status: status_at(Self::IRONWOOD_ACTIVATION),
                },
            ])
        })();
        std::future::ready(result)
    }
}

impl GetHealth for ScriptedChain {
    fn get_health(
        &self,
    ) -> impl std::future::Future<Output = Result<Health, PortError<GetHealthError>>> + Send {
        let result = {
            let state = self.state.read().expect("scripted chain lock poisoned");
            if state.shut_down {
                Err(BackendError::fatal("the port is shut down").into())
            } else if state.blocks.is_empty() {
                Ok(Health::Starting)
            } else {
                Ok(Health::Ready)
            }
        };
        std::future::ready(result)
    }
}

impl ShutDown for ScriptedChain {
    fn shut_down(&self) -> impl std::future::Future<Output = ()> + Send {
        {
            let mut state = self.state.write().expect("scripted chain lock poisoned");
            state.shut_down = true;
        }
        // Wake both subscriptions so they observe the flag and end.
        self.tip_events.send_replace(None);
        self.mempool_events.send_replace(());
        std::future::ready(())
    }
}

impl BroadcastTransaction for ScriptedChain {
    fn broadcast_transaction(
        &self,
        transaction: RawTransaction,
    ) -> impl std::future::Future<
        Output = Result<TransactionHash, PortError<BroadcastTransactionError>>,
    > + Send {
        // Scripted validation: empty bytes are malformed; bytes
        // starting with "reject:" are rejected; duplicates are
        // rejected; anything else is accepted into the mempool. The
        // shutdown check comes first: a dead port answers every
        // operation with the fatal backend error the ShutDown contract
        // promises, never a domain rejection.
        let result = (|| {
            let mut state = self.state.write().expect("scripted chain lock poisoned");
            if state.shut_down {
                return Err(BackendError::fatal("the port is shut down").into());
            }
            if transaction.as_slice().is_empty() {
                return Err(PortError::Domain(BroadcastTransactionError::Malformed));
            }
            if transaction.as_slice().starts_with(b"reject:") {
                return Err(PortError::Domain(BroadcastTransactionError::Rejected {
                    reason: "scripted rejection".into(),
                }));
            }
            let Some(tip) = state.blocks.last().copied() else {
                return Err(BackendError::transient("the scripted chain has no tip yet").into());
            };
            let txid = broadcast_txid(transaction.as_slice());
            if state.mempool.iter().any(|entry| entry.txid == txid) {
                return Err(PortError::Domain(BroadcastTransactionError::Rejected {
                    reason: "already in the mempool".into(),
                }));
            }
            state.mempool.push(MempoolTransaction {
                raw: transaction,
                txid,
                branch_id: scripted_branch_id(),
                validated_against: tip,
            });
            drop(state);
            self.mempool_events.send_replace(());
            Ok(txid)
        })();
        std::future::ready(result)
    }
}

impl SubscribeToMempool for ScriptedChain {
    fn subscribe_to_mempool(&self) -> impl Stream<Item = MempoolTransaction> + Send {
        let receiver = self.mempool_events.subscribe();
        futures::stream::unfold(
            (&self.state, receiver, 0usize),
            |(state, mut receiver, delivered)| async move {
                loop {
                    let (shut_down, next) = {
                        let current = state.read().expect("scripted chain lock poisoned");
                        (current.shut_down, current.mempool.get(delivered).cloned())
                    };
                    if shut_down {
                        return None;
                    }
                    if let Some(entry) = next {
                        return Some((entry, (state, receiver, delivered + 1)));
                    }
                    if receiver.changed().await.is_err() {
                        return None;
                    }
                }
            },
        )
    }
}

impl SubscribeToTipChanges for ScriptedChain {
    fn subscribe_to_tip_changes(&self) -> impl Stream<Item = BlockId> + Send {
        let receiver = self.tip_events.subscribe();
        futures::stream::unfold(
            (&self.state, receiver, false),
            |(state, mut receiver, delivered_initial)| async move {
                let is_shut_down = || {
                    state
                        .read()
                        .expect("scripted chain lock poisoned")
                        .shut_down
                };
                // The shutdown flag is checked only AFTER each
                // `borrow_and_update`: `shut_down` sets the flag and
                // then bumps the channel, so a check taken after the
                // read cannot miss a shutdown whose wake the read just
                // consumed — checking before the read could, leaving
                // `changed()` waiting for a version that never comes.
                if !delivered_initial {
                    let current = *receiver.borrow_and_update();
                    if is_shut_down() {
                        return None;
                    }
                    if let Some(tip) = current {
                        return Some((tip, (state, receiver, true)));
                    }
                }
                loop {
                    if receiver.changed().await.is_err() {
                        return None;
                    }
                    let current = *receiver.borrow_and_update();
                    if is_shut_down() {
                        return None;
                    }
                    if let Some(tip) = current {
                        return Some((tip, (state, receiver, true)));
                    }
                }
            },
        )
    }
}

impl GetPinnedTip for ScriptedSnapshot {
    fn get_pinned_tip(&self) -> BlockId {
        *self
            .blocks
            .last()
            .expect("scripted snapshot is never empty")
    }
}

impl ScriptedSnapshot {
    /// The pinned block at `height`, in O(1): the pinned chain is dense
    /// from genesis (every constructor and scripted mutation keeps it
    /// so), so a height is its own index.
    fn block_at(&self, height: Height) -> Option<BlockId> {
        self.blocks.get(u32::from(height) as usize).copied()
    }
}

impl GetHashForHeight for ScriptedSnapshot {
    fn get_hash_for_height(
        &self,
        height: Height,
    ) -> impl std::future::Future<Output = Result<Option<BlockHash>, PortError<GetHashForHeightError>>>
           + Send {
        std::future::ready(Ok(self.block_at(height).map(|block| block.hash)))
    }
}

impl GetHeightForHash for ScriptedSnapshot {
    fn get_height_for_hash(
        &self,
        hash: BlockHash,
    ) -> impl std::future::Future<Output = Result<Option<Height>, PortError<GetHeightForHashError>>> + Send
    {
        let found = self
            .blocks
            .iter()
            .find(|block| block.hash == hash)
            .map(|block| block.height);
        std::future::ready(Ok(found))
    }
}

impl FindForkPoint for ScriptedSnapshot {
    fn find_fork_point(
        &self,
        locator: &BlockLocator,
    ) -> impl std::future::Future<Output = Result<Option<BlockId>, PortError<FindForkPointError>>> + Send
    {
        // Contract-exact: the match highest on the pinned chain, judged
        // by hash, at the chain's height — independent of the heights
        // the locator claims.
        let found = locator
            .hashes()
            .filter_map(|hash| self.blocks.iter().find(|block| block.hash == hash))
            .max_by_key(|block| block.height)
            .copied();
        std::future::ready(Ok(found))
    }
}

impl GetRawBlock for ScriptedSnapshot {
    fn get_raw_block(
        &self,
        height: Height,
    ) -> impl std::future::Future<Output = Result<Option<RawBlock>, PortError<GetRawBlockError>>> + Send
    {
        let found = self
            .block_at(height)
            .map(|block| RawBlock::new(scripted_block_bytes(block)));
        std::future::ready(Ok(found))
    }
}

impl GetRawBlockHeader for ScriptedSnapshot {
    fn get_raw_block_header(
        &self,
        height: Height,
    ) -> impl std::future::Future<
        Output = Result<Option<RawBlockHeader>, PortError<GetRawBlockHeaderError>>,
    > + Send {
        let found = self
            .block_at(height)
            .map(|block| RawBlockHeader::new(scripted_header_bytes(block)));
        std::future::ready(Ok(found))
    }
}

impl StreamRawBlocks for ScriptedSnapshot {
    fn stream_raw_blocks(
        &self,
        range: Range<Height>,
    ) -> impl Stream<Item = Result<(BlockId, RawBlock), PortError<StreamRawBlocksError>>> + Send
    {
        // Lazy: the dense height layout locates the sub-range in O(1),
        // and each block's bytes are built only when the item is
        // polled — no whole-range materialization up front.
        let blocks = Arc::clone(&self.blocks);
        let start = u32::from(range.start) as usize;
        let end = (u32::from(range.end) as usize).min(blocks.len());
        futures::stream::iter((start..end).map(move |index| {
            let block = blocks[index];
            Ok((block, RawBlock::new(scripted_block_bytes(block))))
        }))
    }
}

impl ScriptedSnapshot {
    /// The block whose one scripted transaction has `txid`, if any.
    ///
    /// Each scripted block mines exactly one transaction, whose txid
    /// derives from the block's height and fork tag, so the scan
    /// recomputes and compares.
    fn block_with_txid(&self, txid: TransactionHash) -> Option<BlockId> {
        self.blocks
            .iter()
            .find(|block| txid_of_block(**block) == txid)
            .copied()
    }
}

impl GetMinedTransaction for ScriptedSnapshot {
    fn get_mined_transaction(
        &self,
        txid: TransactionHash,
    ) -> impl std::future::Future<
        Output = Result<Option<MinedTransaction>, PortError<GetMinedTransactionError>>,
    > + Send {
        let found = self.block_with_txid(txid).map(|block| MinedTransaction {
            raw: RawTransaction::new(scripted_tx_bytes(block)),
            branch_id: scripted_branch_id(),
            mined_at: block,
            block_time: scripted_block_time(block.height),
        });
        std::future::ready(Ok(found))
    }
}

impl GetTransactionStatus for ScriptedSnapshot {
    fn get_transaction_status(
        &self,
        txid: TransactionHash,
    ) -> impl std::future::Future<
        Output = Result<TransactionStatus, PortError<GetTransactionStatusError>>,
    > + Send {
        let orphaned = self
            .orphaned
            .iter()
            .any(|block| txid_of_block(*block) == txid);
        let status = match self.block_with_txid(txid) {
            Some(block) => TransactionStatus::MinedAt(block),
            None if orphaned => TransactionStatus::NotInBestChain,
            None => TransactionStatus::Unknown,
        };
        std::future::ready(Ok(status))
    }
}

impl GetTreestate for ScriptedSnapshot {
    fn get_treestate(
        &self,
        height: Height,
    ) -> impl std::future::Future<Output = Result<Option<TreestateAt>, PortError<GetTreestateError>>>
           + Send {
        let found = self.block_at(height).map(|block| TreestateAt {
            at: block,
            sapling: Some(RawTreeFrontier::new(scripted_frontier_bytes(b's', block))),
            orchard: Some(RawTreeFrontier::new(scripted_frontier_bytes(b'o', block))),
            ironwood: (u32::from(height) >= ScriptedChain::IRONWOOD_ACTIVATION)
                .then(|| RawTreeFrontier::new(scripted_frontier_bytes(b'i', block))),
        });
        std::future::ready(Ok(found))
    }
}

impl GetAddressUnspentOutpoints for ScriptedSnapshot {
    fn get_address_unspent_outpoints(
        &self,
        address: &TransparentAddress,
        range: Range<Height>,
    ) -> impl std::future::Future<
        Output = Result<Vec<Outpoint>, PortError<GetAddressUnspentOutpointsError>>,
    > + Send {
        // The spend chain leaves exactly the tip's output unspent; it
        // answers only when the tip's creating height lies in `range`.
        let unspent = if *address == ScriptedChain::address() {
            self.blocks
                .last()
                .filter(|tip| range.contains(&tip.height))
                .map(|tip| Outpoint {
                    txid: txid_of_block(*tip),
                    index: 0,
                })
                .into_iter()
                .collect()
        } else {
            Vec::new()
        };
        std::future::ready(Ok(unspent))
    }
}

impl GetAddressTransactionIds for ScriptedSnapshot {
    fn get_address_transaction_ids(
        &self,
        address: &TransparentAddress,
        range: Range<Height>,
    ) -> impl std::future::Future<
        Output = Result<Vec<TransactionHash>, PortError<GetAddressTransactionIdsError>>,
    > + Send {
        // Every scripted transaction involves the scripted address.
        let txids = if *address == ScriptedChain::address() {
            self.blocks
                .iter()
                .filter(|block| range.contains(&block.height))
                .map(|block| txid_of_block(*block))
                .collect()
        } else {
            Vec::new()
        };
        std::future::ready(Ok(txids))
    }
}

impl GetOutpointSpendStatus for ScriptedSnapshot {
    fn get_outpoint_spend_status(
        &self,
        outpoint: Outpoint,
    ) -> impl std::future::Future<
        Output = Result<Option<SpendStatus>, PortError<GetOutpointSpendStatusError>>,
    > + Send {
        // Each scripted transaction has exactly one output (index 0),
        // spent by the next block's transaction; the tip's is unspent.
        let status = (outpoint.index == 0)
            .then(|| {
                self.blocks
                    .iter()
                    .position(|block| txid_of_block(*block) == outpoint.txid)
                    .map(|position| match self.blocks.get(position + 1) {
                        Some(spender) => SpendStatus::SpentBy(txid_of_block(*spender)),
                        None => SpendStatus::Unspent,
                    })
            })
            .flatten();
        std::future::ready(Ok(status))
    }
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;

    use super::*;
    use crate::conformance;

    #[tokio::test]
    async fn passes_the_conformance_kit() {
        let chain = ScriptedChain::with_linear_chain(10);
        conformance::snapshot_is_self_consistent(&chain).await;
        conformance::absent_blocks_read_none(&chain).await;
        conformance::clones_share_the_pinned_view(&chain).await;
        conformance::fork_point_of_the_tip_locator_is_the_tip(&chain).await;
        conformance::fork_point_prefers_the_highest_match(&chain).await;
        conformance::fork_point_skips_entries_off_the_chain(&chain).await;
        conformance::fork_point_is_none_when_no_entry_matches(&chain).await;
        conformance::stream_covers_the_pinned_range(&chain).await;
        conformance::stream_clamps_to_the_pinned_view(&chain).await;
        conformance::raw_reads_agree_with_the_stream(&chain).await;
        conformance::absent_payloads_read_none(&chain).await;
        conformance::unknown_transactions_read_none_and_unknown(&chain).await;
        conformance::treestates_exist_at_every_in_view_height(&chain).await;
        conformance::absent_treestates_read_none(&chain).await;
        conformance::unknown_outpoints_read_none(&chain).await;
        conformance::mined_transactions_agree_across_capabilities(
            &chain,
            ScriptedChain::txid_of(5, 0),
        )
        .await;
        conformance::subscription_yields_the_current_tip_first(&chain).await;
        conformance::tip_events_follow_chain_movement(&chain, async {
            chain.advance(1);
        })
        .await;
        // After the movement above, the tip sits at height 10 on fork
        // 0; the handles name the tip's transaction, which the reorg
        // below orphans — the pinned view must go on serving it.
        conformance::snapshots_stay_pinned_across_chain_movement(
            &chain,
            async {
                chain.reorg(2);
            },
            &ScriptedChain::address(),
            Outpoint {
                txid: ScriptedChain::txid_of(10, 0),
                index: 0,
            },
            ScriptedChain::txid_of(10, 0),
        )
        .await;
        conformance::tip_events_coalesce_to_the_latest(&chain, || async {
            chain.advance(1);
        })
        .await;
        conformance::mempool_deliveries_are_tagged_with_the_current_tip(&chain, async {
            chain.queue_mempool_transaction()
        })
        .await;
        conformance::mempool_stream_survives_tip_changes(
            &chain,
            async {
                chain.advance(1);
            },
            async { chain.queue_mempool_transaction() },
        )
        .await;
        conformance::broadcasts_reach_the_mempool(
            &chain,
            RawTransaction::new(b"scripted-broadcast".to_vec()),
        )
        .await;
        conformance::mempool_subscriptions_deliver_prior_contents(
            &chain,
            RawTransaction::new(b"scripted-prior-contents".to_vec()),
        )
        .await;
        conformance::malformed_broadcasts_are_rejected(&chain).await;
        conformance::rejected_broadcasts_are_domain_answers(
            &chain,
            RawTransaction::new(b"reject:scripted".to_vec()),
        )
        .await;
        conformance::reported_upgrades_agree_with_the_tip(&chain).await;
        conformance::ready_ports_report_ready(&chain).await;
    }

    /// The shutdown conformance case, on a dedicated instance because
    /// it spends the port.
    #[tokio::test]
    async fn passes_the_shutdown_conformance_case() {
        let chain = ScriptedChain::with_linear_chain(5);
        conformance::shutdown_ends_the_port(&chain).await;
    }

    /// The scripted upgrade schedule flips from pending to active as
    /// the chain crosses the activation height, and a chain with no
    /// view yet reports Starting. Mock-only exact values.
    #[tokio::test]
    async fn upgrades_activate_and_health_starts() {
        let chain = ScriptedChain::with_linear_chain(3);
        let before = chain
            .get_reported_upgrades()
            .await
            .expect("reporting succeeds");
        assert_eq!(
            before[1].status,
            UpgradeStatus::Pending,
            "below its activation, ScriptedIronwood is pending"
        );

        chain.advance(5);
        let after = chain
            .get_reported_upgrades()
            .await
            .expect("reporting succeeds");
        assert_eq!(
            after[1].status,
            UpgradeStatus::Active,
            "past its activation, ScriptedIronwood is active"
        );

        let unready = ScriptedChain::with_linear_chain(0);
        let health = unready.get_health().await.expect("health succeeds");
        assert_eq!(health, Health::Starting);
    }

    /// Scripted broadcast rejections the generic kit cannot force: a
    /// duplicate submission and an unready engine.
    #[tokio::test]
    async fn broadcast_rejects_duplicates_and_unready_chains() {
        let chain = ScriptedChain::with_linear_chain(5);
        let bytes = RawTransaction::new(b"a-transaction".to_vec());

        chain
            .broadcast_transaction(bytes.clone())
            .await
            .expect("the first broadcast is accepted");
        let duplicate = chain.broadcast_transaction(bytes).await;
        assert!(
            matches!(
                duplicate,
                Err(PortError::Domain(
                    BroadcastTransactionError::Rejected { .. }
                ))
            ),
            "a duplicate broadcast is rejected, got: {duplicate:?}"
        );

        let unready = ScriptedChain::with_linear_chain(0);
        let result = unready
            .broadcast_transaction(RawTransaction::new(b"too-early".to_vec()))
            .await;
        assert!(
            result.as_ref().is_err_and(|error| error.is_transient()),
            "an unready engine fails transiently, got: {result:?}"
        );
    }

    /// Revalidation after a reorg, observably: a transaction queued
    /// against the old tip is delivered to a later subscriber tagged
    /// with the new tip. Mock-only — the generic kit cannot observe
    /// the same entry under two tags without controlling delivery
    /// order.
    #[tokio::test]
    async fn mempool_entries_are_revalidated_against_the_new_tip() {
        let chain = ScriptedChain::with_linear_chain(10);

        let txid = chain.queue_mempool_transaction();
        let mut before = std::pin::pin!(chain.subscribe_to_mempool());
        let entry = before.next().await.expect("current contents first");
        assert_eq!(entry.txid, txid);
        assert_eq!(
            entry.validated_against.hash,
            ScriptedChain::hash_of(9, 0),
            "queued against the old tip"
        );

        chain.reorg(3);

        let mut after = std::pin::pin!(chain.subscribe_to_mempool());
        let revalidated = after.next().await.expect("current contents first");
        assert_eq!(revalidated.txid, txid, "the entry survived the reorg");
        assert_eq!(
            revalidated.validated_against.hash,
            ScriptedChain::hash_of(10, 1),
            "now validated against the new tip"
        );
    }

    /// A fresh subscription delivers the current mempool contents in
    /// arrival order before any new arrivals.
    #[tokio::test]
    async fn mempool_subscription_delivers_current_contents_first() {
        let chain = ScriptedChain::with_linear_chain(5);
        let first = chain.queue_mempool_transaction();
        let second = chain.queue_mempool_transaction();

        let mut mempool = std::pin::pin!(chain.subscribe_to_mempool());
        let delivered_first = mempool.next().await.expect("contents delivered");
        let delivered_second = mempool.next().await.expect("contents delivered");
        assert_eq!(delivered_first.txid, first);
        assert_eq!(delivered_second.txid, second);

        let third = chain.queue_mempool_transaction();
        let delivered_third = mempool.next().await.expect("arrival delivered");
        assert_eq!(delivered_third.txid, third);
    }

    /// Pinning through the capabilities the generic kit cannot reach
    /// (they need known txids and addresses): a snapshot taken before
    /// a reorg still answers from its own era — the later-orphaned
    /// transaction is still mined, the old tip's output is still the
    /// unspent one — while a fresh snapshot answers from the new era.
    #[tokio::test]
    async fn pinned_views_predate_the_reorg() {
        let chain = ScriptedChain::with_linear_chain(10);
        let old = chain.take_snapshot().await.expect("chain is ready");

        chain.reorg(3);
        let new = chain.take_snapshot().await.expect("chain is ready");

        let txid = ScriptedChain::txid_of(8, 0);
        let old_status = old
            .get_transaction_status(txid)
            .await
            .expect("a status read succeeds");
        assert_eq!(
            old_status,
            TransactionStatus::MinedAt(BlockId {
                height: Height::try_from(8).expect("valid height"),
                hash: ScriptedChain::hash_of(8, 0),
            }),
            "the pinned view predates the reorg: the transaction is still mined"
        );
        let new_status = new
            .get_transaction_status(txid)
            .await
            .expect("a status read succeeds");
        assert_eq!(
            new_status,
            TransactionStatus::NotInBestChain,
            "the fresh view sees the same transaction orphaned"
        );

        let address = ScriptedChain::address();
        let old_range = Height::GENESIS..Height::try_from(10).expect("valid height");
        let old_unspent = old
            .get_address_unspent_outpoints(&address, old_range)
            .await
            .expect("an address query succeeds");
        assert_eq!(
            old_unspent,
            vec![Outpoint {
                txid: ScriptedChain::txid_of(9, 0),
                index: 0,
            }],
            "the pinned view's unspent output is the old tip's"
        );
        let new_range = Height::GENESIS..Height::try_from(11).expect("valid height");
        let new_unspent = new
            .get_address_unspent_outpoints(&address, new_range)
            .await
            .expect("an address query succeeds");
        assert_eq!(
            new_unspent,
            vec![Outpoint {
                txid: ScriptedChain::txid_of(10, 1),
                index: 0,
            }],
            "the fresh view's unspent output is the new tip's"
        );
    }

    /// A scripted reorg end to end: the tip event carries the new-fork
    /// tip, old-branch transactions become NotInBestChain, new-branch
    /// and shared-prefix transactions stay mined. Mock-only — the
    /// generic kit cannot script a reorg.
    #[tokio::test]
    async fn reorg_moves_the_tip_and_orphans_transactions() {
        let chain = ScriptedChain::with_linear_chain(10);
        let mut events = std::pin::pin!(chain.subscribe_to_tip_changes());
        let initial = events.next().await.expect("initial tip");
        assert_eq!(initial.hash, ScriptedChain::hash_of(9, 0));

        chain.reorg(3);

        let new_tip = events.next().await.expect("reorg surfaces as a tip event");
        assert_eq!(
            new_tip,
            BlockId {
                height: Height::try_from(10).expect("valid height"),
                hash: ScriptedChain::hash_of(10, 1),
            },
            "the new tip sits one above the old, on the new fork"
        );

        let snapshot = chain.take_snapshot().await.expect("chain is ready");
        let orphaned = snapshot
            .get_transaction_status(ScriptedChain::txid_of(8, 0))
            .await
            .expect("a status read succeeds");
        assert_eq!(
            orphaned,
            TransactionStatus::NotInBestChain,
            "an old-branch transaction is orphaned"
        );

        let replacement = snapshot
            .get_transaction_status(ScriptedChain::txid_of(8, 1))
            .await
            .expect("a status read succeeds");
        assert!(
            matches!(replacement, TransactionStatus::MinedAt(_)),
            "the new-branch transaction at the same height is mined"
        );

        let shared = snapshot
            .get_transaction_status(ScriptedChain::txid_of(3, 0))
            .await
            .expect("a status read succeeds");
        assert!(
            matches!(shared, TransactionStatus::MinedAt(_)),
            "a shared-prefix transaction stays mined"
        );
    }

    /// Tip events coalesce: a subscriber that missed intermediate
    /// movements receives the latest tip, not a backlog.
    #[tokio::test]
    async fn tip_events_coalesce_to_the_latest() {
        let chain = ScriptedChain::with_linear_chain(5);
        let mut events = std::pin::pin!(chain.subscribe_to_tip_changes());
        let _initial = events.next().await.expect("initial tip");

        chain.advance(1);
        chain.advance(1);
        chain.advance(1);

        let delivered = events.next().await.expect("a tip event arrives");
        assert_eq!(
            delivered.hash,
            ScriptedChain::hash_of(7, 0),
            "the subscriber sees the latest tip, not the first missed one"
        );
    }

    /// An unready chain delivers its first tip once one exists.
    #[tokio::test]
    async fn subscription_on_an_unready_chain_yields_the_first_tip() {
        let chain = ScriptedChain::with_linear_chain(0);
        let mut events = std::pin::pin!(chain.subscribe_to_tip_changes());

        chain.advance(1);

        let first = events.next().await.expect("the first tip arrives");
        assert_eq!(first.hash, ScriptedChain::hash_of(0, 0));
    }

    /// The scripted spend chain end to end: every non-tip output is
    /// spent by its successor, the tip's output is unspent, and the
    /// address queries agree. Mock-only — the generic kit cannot know
    /// an implementation's addresses or txids.
    #[tokio::test]
    async fn serves_the_scripted_spend_chain() {
        let chain = ScriptedChain::with_linear_chain(10);
        let snapshot = chain.take_snapshot().await.expect("chain is ready");
        let address = ScriptedChain::address();

        let full_range = Height::GENESIS..Height::try_from(10).expect("valid height");
        let unspent = snapshot
            .get_address_unspent_outpoints(&address, full_range.clone())
            .await
            .expect("an address query succeeds");
        assert_eq!(
            unspent,
            vec![Outpoint {
                txid: ScriptedChain::txid_of(9, 0),
                index: 0,
            }],
            "only the tip's output is unspent"
        );

        let spent = snapshot
            .get_outpoint_spend_status(Outpoint {
                txid: ScriptedChain::txid_of(3, 0),
                index: 0,
            })
            .await
            .expect("a spend-status read succeeds");
        assert_eq!(
            spent,
            Some(SpendStatus::SpentBy(ScriptedChain::txid_of(4, 0))),
            "a non-tip output is spent by its successor"
        );

        let tip_status = snapshot
            .get_outpoint_spend_status(Outpoint {
                txid: ScriptedChain::txid_of(9, 0),
                index: 0,
            })
            .await
            .expect("a spend-status read succeeds");
        assert_eq!(tip_status, Some(SpendStatus::Unspent));

        let txids = snapshot
            .get_address_transaction_ids(
                &address,
                Height::try_from(2).expect("valid")..Height::try_from(5).expect("valid"),
            )
            .await
            .expect("an address-txids query succeeds");
        assert_eq!(
            txids,
            vec![
                ScriptedChain::txid_of(2, 0),
                ScriptedChain::txid_of(3, 0),
                ScriptedChain::txid_of(4, 0),
            ],
            "the half-open range yields the involved txids ascending"
        );

        let elsewhere = snapshot
            .get_address_unspent_outpoints(
                &TransparentAddress::new("t1SomewhereElse".into()),
                full_range,
            )
            .await
            .expect("an unknown-address query succeeds");
        assert!(elsewhere.is_empty(), "an unseen address has no outpoints");
    }

    /// The zcash/zallet#455 semantic in action: below the scripted
    /// Ironwood activation the pool's frontier is absent — an empty
    /// tree, not an error — and present from activation on. Mock-only:
    /// the generic kit cannot force a pool to be pre-activation.
    #[tokio::test]
    async fn ironwood_frontier_absent_below_activation() {
        let chain = ScriptedChain::with_linear_chain(10);
        let snapshot = chain.take_snapshot().await.expect("chain is ready");

        let before = snapshot
            .get_treestate(
                Height::try_from(ScriptedChain::IRONWOOD_ACTIVATION - 1).expect("valid height"),
            )
            .await
            .expect("an in-view treestate read succeeds")
            .expect("every in-view height has a treestate");
        assert_eq!(before.ironwood, None, "empty tree before activation");
        assert!(before.sapling.is_some(), "sapling active from genesis");
        assert!(before.orchard.is_some(), "orchard active from genesis");

        let after = snapshot
            .get_treestate(
                Height::try_from(ScriptedChain::IRONWOOD_ACTIVATION).expect("valid height"),
            )
            .await
            .expect("an in-view treestate read succeeds")
            .expect("every in-view height has a treestate");
        assert!(after.ironwood.is_some(), "frontier present from activation");
    }

    /// The positive transaction paths, which the generic kit cannot
    /// exercise: it has no way to learn a real txid from raw block
    /// bytes.
    #[tokio::test]
    async fn serves_scripted_transactions() {
        let chain = ScriptedChain::with_linear_chain(10);
        let snapshot = chain.take_snapshot().await.expect("chain is ready");
        let txid = ScriptedChain::txid_of(3, 0);

        let mined = snapshot
            .get_mined_transaction(txid)
            .await
            .expect("a mined-transaction read succeeds")
            .expect("the scripted transaction is mined");
        let expected_block = BlockId {
            height: Height::try_from(3).expect("valid height"),
            hash: ScriptedChain::hash_of(3, 0),
        };
        assert_eq!(mined.mined_at, expected_block);
        assert_eq!(mined.branch_id, scripted_branch_id());
        assert_eq!(mined.block_time, scripted_block_time(expected_block.height));
        assert!(!mined.raw.as_slice().is_empty());

        let status = snapshot
            .get_transaction_status(txid)
            .await
            .expect("a status read succeeds");
        assert_eq!(status, TransactionStatus::MinedAt(expected_block));
    }

    /// The contract judges membership by hash: a locator entry claiming
    /// a wrong height for a real hash still matches, at the chain's
    /// height. Mock-only — the kit can't fabricate dishonest locators
    /// without knowing the implementation's hashes.
    #[tokio::test]
    async fn fork_point_ignores_claimed_heights() {
        let chain = ScriptedChain::with_linear_chain(10);
        let snapshot = chain.take_snapshot().await.expect("chain is ready");

        let dishonest = BlockId {
            height: Height::try_from(7).expect("valid height"),
            hash: ScriptedChain::hash_of(3, 0),
        };
        let locator = BlockLocator::new(vec![dishonest]).expect("well-formed");
        let fork = snapshot
            .find_fork_point(&locator)
            .await
            .expect("fork-point detection succeeds");
        assert_eq!(
            fork,
            Some(BlockId {
                height: Height::try_from(3).expect("valid height"),
                hash: ScriptedChain::hash_of(3, 0),
            }),
            "the match carries the chain's height, not the locator's claim"
        );
    }

    #[tokio::test]
    async fn empty_chain_is_not_ready() {
        let chain = ScriptedChain::with_linear_chain(0);
        let result = chain.take_snapshot().await;
        assert!(matches!(
            result,
            Err(PortError::Domain(TakeSnapshotError::NotReady))
        ));
    }

    #[tokio::test]
    async fn serves_the_scripted_blocks() {
        let chain = ScriptedChain::with_linear_chain(3);
        let snapshot = chain.take_snapshot().await.expect("chain is ready");

        let tip = snapshot.get_pinned_tip();
        assert_eq!(u32::from(tip.height), 2);
        assert_eq!(tip.hash, ScriptedChain::hash_of(2, 0));

        let genesis_height = Height::GENESIS;
        let genesis = snapshot
            .get_hash_for_height(genesis_height)
            .await
            .expect("lookup succeeds");
        assert_eq!(genesis, Some(ScriptedChain::hash_of(0, 0)));
    }

    #[test]
    fn fork_tags_never_collide() {
        assert_ne!(scripted_hash(5, 0), scripted_hash(5, 1));
        assert_ne!(scripted_hash(0, 0), BlockHash::ZERO);
    }

    /// Contents that collided under the retired XOR fold (a trailing
    /// byte whose position offset cancelled it) must broadcast as two
    /// distinct transactions, not a false duplicate rejection.
    #[tokio::test]
    async fn broadcast_txids_distinguish_xor_colliding_contents() {
        let chain = ScriptedChain::with_linear_chain(5);
        chain
            .broadcast_transaction(RawTransaction::new(b"a".to_vec()))
            .await
            .expect("the first broadcast is accepted");
        chain
            .broadcast_transaction(RawTransaction::new(b"a\xff".to_vec()))
            .await
            .expect("distinct contents must be accepted, not rejected as a duplicate");
    }

    /// More scripted reorgs than a byte can count: the fork tag must
    /// neither overflow nor wrap around into orphaned branches' hashes.
    #[test]
    fn scripted_reorgs_beyond_a_byte_of_forks() {
        let chain = ScriptedChain::with_linear_chain(2);
        for _ in 0..300 {
            chain.reorg(1);
        }
    }
}
