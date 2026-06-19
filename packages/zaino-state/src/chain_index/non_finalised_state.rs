use super::{finalised_state::FinalisedState, source::BlockchainSource, NON_FINALIZED_DEPTH};
use crate::{chain_index::finalization_ceiling, IndexedBlock};

use crate::{
    chain_index::types::{
        self, BlockHash, BlockIndex, BlockMetadata, BlockWithMetadata, Height, TreeRootData,
    },
    error::FinalisedStateError,
    BlockContext, ChainWork,
};
use arc_swap::ArcSwap;
use futures::lock::Mutex;
use primitive_types::U256;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::mpsc;
use tracing::{info, instrument, warn};
use zebra_chain::{parameters::Network, serialization::BytesInDisplayOrder};
use zebra_state::HashOrHeight;

/// Hard cap on how many blocks below the tip the non-finalised state retains in memory.
///
/// [`NonFinalizedState::update`] normally trims everything below the finalised database height,
/// but that height can lag far behind the tip while the finalised DB syncs in the background, and
/// is pinned at `0` in ephemeral mode. Without an independent floor the snapshot would grow by one
/// block per new block indefinitely. This caps retention to a fixed window regardless, a small
/// margin above [`NON_FINALIZED_DEPTH`] so it never trims inside the reorg-possible range.
const MAX_NFS_DEPTH: u32 = NON_FINALIZED_DEPTH + 10;

/// Holds the block cache
#[derive(Debug)]
pub struct NonFinalizedState<Source: BlockchainSource> {
    /// We need access to the validator's best block hash, as well
    /// as a source of blocks
    pub(super) source: Source,
    /// This lock should not be exposed to consumers. Rather,
    /// clone the Arc and offer that. This means we can overwrite the arc
    /// without interfering with readers, who will hold a stale copy
    current: ArcSwap<NonfinalizedBlockCacheSnapshot>,
    /// Used mostly to determine activation heights
    pub(crate) network: Network,
    /// Listener used to detect non-best-chain blocks, if available
    #[allow(clippy::type_complexity)]
    nfs_change_listener: Option<
        Mutex<
            tokio::sync::mpsc::Receiver<(zebra_chain::block::Hash, Arc<zebra_chain::block::Block>)>,
        >,
    >,
}

#[derive(Debug, Clone)]
/// A consistent snapshot of the chain index's non-finalized state.
///
/// The non-finalized state always exists — it is built eagerly at chain-index
/// creation — so a snapshot always carries a [`NonfinalizedBlockCacheSnapshot`].
/// Whether that window has been validated against the finalized chain yet (the
/// finalized DB has reached its seam) is its [`SnapshotAvailability`]: while
/// `Provisional`, reads that need finalized data pass through to the validator.
pub struct ChainIndexSnapshot {
    non_finalized_snapshot: Arc<NonfinalizedBlockCacheSnapshot>,
}

impl ChainIndexSnapshot {
    pub(crate) fn new(non_finalized_snapshot: Arc<NonfinalizedBlockCacheSnapshot>) -> Self {
        Self {
            non_finalized_snapshot,
        }
    }

    /// The non-finalized snapshot. Always present: the NFS is eagerly
    /// constructed and never absent.
    pub(crate) fn get_nfs_snapshot(&self) -> &NonfinalizedBlockCacheSnapshot {
        &self.non_finalized_snapshot
    }

    /// Whether the finalized DB has caught up to this window's seam, and (when
    /// it has) the seam's absolute-chainwork base.
    pub(crate) fn availability(&self) -> SnapshotAvailability {
        self.non_finalized_snapshot.availability
    }

    /// True once a sync pass has validated this window against the finalized
    /// chain (the finalized DB reached the seam). While false, reads needing
    /// finalized data must pass through to the validator.
    pub(crate) fn is_resolved(&self) -> bool {
        matches!(self.availability(), SnapshotAvailability::Reified)
    }

    /// The non-finalized snapshot, but only once it is `Resolved` (validated
    /// against the finalized chain). `None` while `Provisional`, so callers
    /// that need authoritative data fall back (e.g. to the validator) then.
    /// Distinct from [`Self::get_nfs_snapshot`], which is unconditional.
    pub(crate) fn resolved_nfs_snapshot(&self) -> Option<&NonfinalizedBlockCacheSnapshot> {
        self.is_resolved().then(|| self.get_nfs_snapshot())
    }
}

/// Whether a published snapshot's non-finalized window has been validated
/// against the finalized chain yet.
///
/// This is purely about *absolute cumulative chainwork*: the snapshot always
/// exists and always serves its own block data regardless. While `Provisional`
/// the window carries only relative work; once the finalized DB reaches the
/// seam it is `Resolved` and absolute work is recoverable. Flipped to
/// `Resolved` inside `update`, atomically with the `compare_and_swap` that
/// publishes the snapshot (the value rides in the snapshot, so the flip is not
/// separable from the block contents). It carries no validator/passthrough
/// height — the NFS never passes through; it serves its own data.
#[derive(Debug, Clone, Copy)]
pub(crate) enum SnapshotAvailability {
    /// The finalized DB has not reached the seam: the window's prev-hash
    /// linkage is unvalidated and its blocks carry only relative chainwork.
    Provisional,
    /// The finalized DB has reached the seam: the window is validated against
    /// the finalized chain. (The seam's absolute-chainwork base — needed to
    /// resolve window blocks' *absolute* chainwork = `base + relative` — is
    /// reattached by the resolution-promotion step, #1096, alongside its
    /// reader; it isn't carried yet.)
    Reified,
}

#[derive(Debug, Clone)]
/// A snapshot of the nonfinalized state as it existed when this was created.
pub(crate) struct NonfinalizedBlockCacheSnapshot {
    /// the set of all known blocks < 100 blocks old
    /// this includes all blocks on-chain, as well as
    /// all blocks known to have been on-chain before being
    /// removed by a reorg. Blocks reorged away have no height.
    pub blocks: HashMap<BlockHash, IndexedBlock>,
    /// hashes indexed by height
    /// Hashes in this map are part of the best chain.
    pub heights_to_hashes: HashMap<Height, BlockHash>,
    // Do we need height here?
    /// The highest known block
    // best_tip is a BestTip, which contains
    // a Height, and a BlockHash as named fields.
    pub best_tip: BlockIndex,
    /// Whether the finalized DB has caught up to this window's seam, and (when
    /// it has) the seam's absolute-chainwork base. Set atomically with the
    /// snapshot publish in `update`.
    pub availability: SnapshotAvailability,
}

/// Cumulative work measured *relative to the seam*, header-derived.
///
/// A distinct type from [`ChainWork`] (which is ABSOLUTE) precisely so the two
/// cannot be confused at a call site: passing relative work where absolute is
/// required — or writing it into an [`IndexedBlock`]'s `chainwork` field — is a
/// type error, not merely a naming convention. This is the misattribution
/// guard in the type system.
///
/// Relative work is a sound best-tip ordering within the non-finalized window:
/// under the assumption that no reorg exceeds [`NON_FINALIZED_DEPTH`], the seam
/// is a stable common ancestor of every competing chain, so relative ordering
/// matches absolute ordering.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ProvisionalCumulativeWork(ChainWork);

impl ProvisionalCumulativeWork {
    /// Relative work at the seam: zero by definition (the seam is the base).
    pub(crate) fn seam() -> Self {
        Self(ChainWork::from_u256(U256::zero()))
    }

    /// Accumulate one block's own (header-derived) work onto the running
    /// relative total.
    /// Override the normal Chainwork behaviour of never
    /// adding 0
    pub(crate) fn add_block_work(&self, block_work: &ChainWork) -> Self {
        match self.0 {
            ChainWork::Indexed(_) => Self(self.0.add(block_work)),
            ChainWork::Provisional => Self(*block_work),
        }
    }
}

#[derive(Debug)]
/// Could not connect to a validator
pub enum NodeConnectionError {
    /// The Uri provided was invalid
    BadUri(String),
    /// Could not connect to the zebrad.
    /// This is a network issue.
    ConnectionFailure(reqwest::Error),
    /// The Zebrad provided invalid or corrupt data. Something has gone wrong
    /// and we need to shut down.
    UnrecoverableError(Box<dyn std::error::Error + Send>),
}

#[derive(Debug)]
struct MissingBlockError(String);

impl std::fmt::Display for MissingBlockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "missing block: {}", self.0)
    }
}

impl std::error::Error for MissingBlockError {}

#[derive(Debug, thiserror::Error)]
/// An error occurred during sync of the NonFinalized State.
pub enum SyncError {
    /// The backing validator node returned corrupt, invalid, or incomplete data.
    #[error("failed to connect to validator: {0:?}")]
    ValidatorConnectionError(NodeConnectionError),
    /// The blockchain source returned a transient error (e.g. node temporarily
    /// unreachable). The sync loop should retry.
    #[error("transient source error: {0}")]
    ErrorFromSource(Box<dyn std::error::Error + Send>),
    /// The channel used to store new blocks has been closed. This should only happen
    /// during shutdown.
    #[error("staging channel closed. Shutdown in progress")]
    StagingChannelClosed,
    /// Sync has been called multiple times in parallel, or another process has
    /// written to the block snapshot.
    #[error("multiple sync processes running")]
    CompetingSyncProcess,
    /// Sync attempted a reorg, and something went wrong.
    #[error("reorg failed: {0}")]
    ReorgFailure(String),
    /// UnrecoverableFinalizedStateError
    #[error("error reading nonfinalized state")]
    CannotReadFinalizedState(#[from] FinalisedStateError),
}

impl From<UpdateError> for SyncError {
    fn from(value: UpdateError) -> Self {
        match value {
            UpdateError::ReceiverDisconnected => SyncError::StagingChannelClosed,
            UpdateError::StaleSnapshot => SyncError::CompetingSyncProcess,
            UpdateError::FinalizedStateCorruption => SyncError::CannotReadFinalizedState(
                FinalisedStateError::Custom("mystery update failure".to_string()),
            ),
            UpdateError::DatabaseHole => {
                SyncError::ReorgFailure(String::from("could not determine best chain"))
            }
            UpdateError::ValidatorConnectionError(e) => SyncError::ValidatorConnectionError(
                NodeConnectionError::UnrecoverableError(Box::new(MissingBlockError(e.to_string()))),
            ),
        }
    }
}

#[derive(thiserror::Error, Debug)]
#[error("data from validator invalid: {0}")]
struct InvalidData(String);

#[derive(Debug, thiserror::Error)]
/// An error occured during initial creation of the NonFinalizedState
pub enum InitError {
    #[error("zebra returned invalid data: {0}")]
    /// the connected node returned garbage data
    InvalidNodeData(Box<dyn std::error::Error + Send + Sync + 'static>),
    #[error(transparent)]
    /// The mempool state failed to initialize
    MempoolInitialzationError(#[from] crate::error::MempoolError),
    #[error(transparent)]
    /// The finalized state failed to initialize
    FinalisedStateInitialzationError(#[from] FinalisedStateError),
    /// The non-finalized state failed to initialize
    #[error("initial block not on best chain")]
    NonFinalizedStateInitError(#[from] SyncError),
}

impl NonfinalizedBlockCacheSnapshot {
    async fn init_from_blockchain_source(
        source: &impl BlockchainSource,
        network: Network,
    ) -> Result<Self, SyncError> {
        let missing_block_error = |target_block| {
            SyncError::ValidatorConnectionError(NodeConnectionError::UnrecoverableError(Box::new(
                MissingBlockError(format!("source missing block: {target_block}")),
            )))
        };
        let tip_height = source
            .get_best_block_height()
            .await
            .map_err(|e| SyncError::ErrorFromSource(Box::new(e)))?
            .ok_or(missing_block_error("no chaintip"))?;
        let start_height = finalization_ceiling(tip_height.0);
        let start_block = source
            .get_block(HashOrHeight::Height(start_height.into()))
            .await
            .map_err(|e| SyncError::ErrorFromSource(Box::new(e)))?
            .ok_or(missing_block_error(
                "100 blocks below self-reported chaintip height",
            ))?;

        let indexed_block = start_block
            .to_indexed_block(source, network.clone())
            .await?;

        let mut blocks = HashMap::new();
        let mut heights_to_hashes = HashMap::new();

        let block_index = BlockIndex {
            height: indexed_block.height(),
            hash: *indexed_block.hash(),
        };

        blocks.insert(block_index.hash, indexed_block);
        heights_to_hashes.insert(block_index.height, block_index.hash);

        Ok(Self {
            blocks,
            heights_to_hashes,
            best_tip: block_index,
            // Newly seeded from the seam block: the finalized DB has not yet
            // caught up to it. `update` flips this to `Resolved` once it has.
            availability: SnapshotAvailability::Provisional,
        })
    }

    fn add_block_new_chaintip(&mut self, block: IndexedBlock) {
        self.best_tip = BlockIndex {
            height: block.height(),
            hash: *block.hash(),
        };
        self.add_block(block)
    }

    fn get_block_by_hash_bytes_in_serialized_order(&self, hash: [u8; 32]) -> Option<&IndexedBlock> {
        self.blocks
            .values()
            .find(|block| block.hash_bytes_serialized_order() == hash)
    }

    fn remove_finalized_blocks(&mut self, finalized_height: Height) {
        let top_block_hash = match self
            .heights_to_hashes
            .iter()
            .max_by_key(|(height, _hash)| *height)
        {
            Some((_height, hash)) => *hash,
            // We have no blocks. There's nothing to remove
            None => return,
        };
        // Keep the last finalized block. This means we don't have to check
        // the finalized state when the entire non-finalized state is reorged away.
        // If all blocks are below the finalized height, keep the highest anyway,
        // so we don't need to re-connect the the finalized state to get chainwork, etc.
        self.blocks.retain(|_hash, block| {
            block.height() >= finalized_height || block.hash() == &top_block_hash
        });
        self.heights_to_hashes
            .retain(|height, hash| height >= &finalized_height || hash == &top_block_hash);
    }

    fn add_block(&mut self, block: IndexedBlock) {
        self.heights_to_hashes.insert(block.height(), *block.hash());
        self.blocks.insert(*block.hash(), block);
    }
}

impl<Source: BlockchainSource> NonFinalizedState<Source> {
    /// Create a nonfinalized state, in a coherent initial state
    ///
    /// TODO: Currently, we can't initate without an snapshot, we need to create a cache
    /// of at least one block. Should this be tied to the instantiation of the data structure
    /// itself?
    #[instrument(name = "NonFinalizedState::initialize", skip(source), fields(network = %network))]
    pub async fn initialize(source: Source, network: Network) -> Result<Self, InitError> {
        info!(network = %network, "Initializing non-finalized state");

        let snapshot =
            NonfinalizedBlockCacheSnapshot::init_from_blockchain_source(&source, network.clone())
                .await?;

        // Set up optional listener
        let nfs_change_listener = Self::setup_listener(&source).await;

        Ok(Self {
            source,
            current: ArcSwap::new(Arc::new(snapshot)),
            network,
            nfs_change_listener,
        })
    }

    /// Set up the optional non-finalized change listener
    async fn setup_listener(
        source: &Source,
    ) -> Option<
        Mutex<
            tokio::sync::mpsc::Receiver<(zebra_chain::block::Hash, Arc<zebra_chain::block::Block>)>,
        >,
    > {
        source
            .nonfinalized_listener()
            .await
            .ok()
            .flatten()
            .map(Mutex::new)
    }

    /// Sync to the iter-committed `chain_height`, trimming to the finalised
    /// tip.
    ///
    /// `chain_height` is the worker's snapshot of the source's best block
    /// height at the start of this iter (the same value `fs.sync_to_height`
    /// was called against). NFS extension is bounded by that height, so a
    /// source advance mid-iter — the validator producing new blocks while
    /// the worker's NFS-sync loop is still running — is deferred to iter
    /// N+1, which will read a fresh `chain_height` and trim the published
    /// snapshot with the correct finalised floor. Closes #1126.
    #[instrument(name = "NonFinalizedState::sync", skip(self, finalized_db))]
    pub(super) async fn sync(
        &self,
        finalized_db: Arc<FinalisedState<Source>>,
        chain_height: Height,
    ) -> Result<(), SyncError> {
        // The NFS is validator-sourced and *leads* the finalized DB: it holds
        // only the non-finalized window `[ceiling, tip]`, anchored at the
        // finalization ceiling (`chain_height - NON_FINALIZED_DEPTH`). Whenever
        // the current floor sits below the ceiling — at cold start (seeded from
        // genesis) or after a deep rollback — re-anchor at the ceiling so the
        // NFS never walks below it (in particular, never from genesis).
        //
        // The seam (ceiling) block comes from the finalized DB if it has already
        // reached the ceiling, otherwise straight from the source: the NFS does
        // not wait for the finalized DB to catch up.
        let anchor_height = super::finalization_ceiling(chain_height.0);
        let mut initial_state = self.get_snapshot();
        if initial_state.best_tip.height < anchor_height {
            self.current.swap(Arc::new(
                NonfinalizedBlockCacheSnapshot::init_from_blockchain_source(
                    &self.source,
                    self.network.clone(),
                )
                .await
                .expect("todo error handling"),
            ));
            initial_state = self.get_snapshot();
        }
        let mut working_snapshot = initial_state.as_ref().clone();

        // currently this only gets main-chain blocks
        // once readstateservice supports serving sidechain data, this
        // must be rewritten to match
        //
        // see https://github.com/ZcashFoundation/zebra/issues/9541

        while let Some(block) = self
            .source
            .get_block(HashOrHeight::Height(zebra_chain::block::Height(
                u32::from(working_snapshot.best_tip.height) + 1,
            )))
            .await
            .map_err(|e| {
                // TODO: Check error. Determine what kind of error to return, this may be recoverable
                SyncError::ValidatorConnectionError(NodeConnectionError::UnrecoverableError(
                    Box::new(e),
                ))
            })?
        {
            // Bail before applying any block that lies above the iter's
            // committed `chain_height`. The speculative `get_block` above
            // can return a block that wasn't yet on the source when the
            // worker committed (the mid-iter source-advance race in
            // #1126); applying it would silently widen this iter's
            // publish past its iter-start `fs.sync_to_height` floor.
            if u32::from(working_snapshot.best_tip.height) + 1 > u32::from(chain_height) {
                break;
            }
            let parent_hash = BlockHash::from(block.header.previous_block_hash);
            if parent_hash == working_snapshot.best_tip.hash {
                // Normal chain progression
                let chainblock =
                    block_to_indexed_block(&block, &self.source, self.network.clone()).await?;
                info!(
                    height = (working_snapshot.best_tip.height + 1).0,
                    hash = %chainblock.hash(),
                    "Syncing block"
                );
                working_snapshot.add_block_new_chaintip(chainblock);
            } else {
                self.handle_reorg(&mut working_snapshot, block.as_ref(), 0)
                    .await?;
                // There's been a reorg. The fresh block is the new chaintip
                // we need to work backwards from it and update heights_to_hashes
                // with it and all its parents.
            }
            if initial_state.best_tip.height + NON_FINALIZED_DEPTH
                < working_snapshot.best_tip.height
            {
                self.update(finalized_db.clone(), initial_state, working_snapshot)
                    .await?;
                initial_state = self.current.load_full();
                working_snapshot = initial_state.as_ref().clone();
            }
        }
        // Handle non-finalized change listener
        self.handle_nfs_change_listener(&mut working_snapshot)
            .await?;

        self.update(finalized_db.clone(), initial_state, working_snapshot)
            .await?;

        Ok(())
    }

    /// Handle a blockchain reorg by finding the common ancestor
    async fn handle_reorg(
        &self,
        working_snapshot: &mut NonfinalizedBlockCacheSnapshot,
        block: &impl Block,
        recursion_count: u8,
    ) -> Result<IndexedBlock, SyncError> {
        // We should never recurse back more than ~100 blocks, assuming
        // a complete reorg of the entire nonfinalized state.
        // 110 adds a likely unneeded safety margin
        if recursion_count > 110 {
            return Err(SyncError::ReorgFailure(
                "reorg handling recursed beyond reason".to_string(),
            ));
        }
        let prev_block = match working_snapshot
            .get_block_by_hash_bytes_in_serialized_order(block.prev_hash_bytes_serialized_order())
            .cloned()
        {
            Some(prev_block) => {
                if !working_snapshot
                    .heights_to_hashes
                    .values()
                    .any(|hash| hash == prev_block.hash())
                {
                    Box::pin(self.handle_reorg(working_snapshot, &prev_block, recursion_count + 1))
                        .await?
                } else {
                    prev_block
                }
            }
            None => {
                let prev_block = self
                    .source
                    .get_block(HashOrHeight::Hash(
                        zebra_chain::block::Hash::from_bytes_in_serialized_order(
                            block.prev_hash_bytes_serialized_order(),
                        ),
                    ))
                    .await
                    .map_err(|e| {
                        SyncError::ValidatorConnectionError(
                            NodeConnectionError::UnrecoverableError(Box::new(e)),
                        )
                    })?
                    .ok_or(SyncError::ValidatorConnectionError(
                        NodeConnectionError::UnrecoverableError(Box::new(MissingBlockError(
                            "zebrad missing block in best chain".to_string(),
                        ))),
                    ))?;
                Box::pin(self.handle_reorg(working_snapshot, &*prev_block, recursion_count + 1))
                    .await?
            }
        };
        let indexed_block = block
            .to_indexed_block(&self.source, self.network.clone())
            .await?;
        working_snapshot.add_block_new_chaintip(indexed_block.clone());
        Ok(indexed_block)
    }

    /// Handle non-finalized change listener events
    async fn handle_nfs_change_listener(
        &self,
        working_snapshot: &mut NonfinalizedBlockCacheSnapshot,
    ) -> Result<(), SyncError> {
        let Some(ref listener) = self.nfs_change_listener else {
            return Ok(());
        };

        let Some(mut listener) = listener.try_lock() else {
            warn!("Error fetching non-finalized change listener");
            return Err(SyncError::CompetingSyncProcess);
        };

        // The NFS holds only the non-finalized window `[seam, tip]`. The seam is
        // the lowest height it tracks; listener blocks below it are finalized
        // and not the NFS's concern. Processing one would walk its ancestry off
        // the bottom of the window — recursing past genesis in `add_nonbest_block`
        // and erroring with `MissingBlockError`.
        let seam = super::finalization_ceiling(working_snapshot.best_tip.height.0);
        loop {
            match listener.try_recv() {
                Ok((hash, block)) => {
                    if block.coinbase_height().is_some_and(|h| Height(h.0) < seam) {
                        continue;
                    }
                    if !self
                        .current
                        .load()
                        .blocks
                        .contains_key(&types::BlockHash(hash.0))
                    {
                        self.add_nonbest_block(working_snapshot, &*block).await?;
                    }
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(e @ mpsc::error::TryRecvError::Disconnected) => {
                    return Err(SyncError::ValidatorConnectionError(
                        NodeConnectionError::UnrecoverableError(Box::new(e)),
                    ))
                }
            }
        }
        Ok(())
    }

    /// Add all blocks from the staging area, and save a new cache snapshot, trimming block below the finalised tip.
    pub(super) async fn update(
        &self,
        finalized_db: Arc<FinalisedState<Source>>,
        initial_state: Arc<NonfinalizedBlockCacheSnapshot>,
        mut new_snapshot: NonfinalizedBlockCacheSnapshot,
    ) -> Result<(), UpdateError> {
        let finalized_height = finalized_db
            .to_reader()
            .db_height()
            .await
            .map_err(|_e| UpdateError::FinalizedStateCorruption)?
            .unwrap_or(Height(0));

        if new_snapshot
            .heights_to_hashes
            .contains_key(&finalized_height)
        {
            new_snapshot.availability = SnapshotAvailability::Reified;
        }

        let seam = match new_snapshot.availability {
            SnapshotAvailability::Provisional => {
                // While provisional, we keep the blocks we know to be finalized
                super::finalization_ceiling(new_snapshot.best_tip.height.0)
            }
            // Once we're reified, we keep blocks until the FS syncs them,
            // to ensure we never allow a gap
            SnapshotAvailability::Reified => finalized_height,
        };

        new_snapshot.remove_finalized_blocks(seam);
        let best_block = &new_snapshot
            .blocks
            .values()
            .max_by_key(|block| block.provisional_cumulative_work(&new_snapshot))
            .cloned()
            .expect("empty snapshot impossible");
        self.handle_reorg(&mut new_snapshot, best_block, 0)
            .await
            .map_err(|_e| UpdateError::DatabaseHole)?;

        // Resolve availability atomically with the publish below. The finalized
        // DB has caught up iff the trimmed window still contains a block at the
        // FS tip (the seam overlaps); when it does, the FS holds that block's
        // absolute cumulative work, which is the base for the window's
        // relative work. Until then the window floats free of the finalized
        // chain — Provisional.
        new_snapshot.availability = if new_snapshot
            .heights_to_hashes
            .contains_key(&finalized_height)
        {
            // Seam overlap: the finalized DB has reached the window's floor.
            // (The seam's absolute-chainwork base is fetched by the resolution
            // promotion, #1096, when its reader exists.)
            SnapshotAvailability::Reified
        } else {
            SnapshotAvailability::Provisional
        };

        // Need to get best hash at some point in this process
        let stored = self
            .current
            .compare_and_swap(&initial_state, Arc::new(new_snapshot));

        if Arc::ptr_eq(&stored, &initial_state) {
            let stale_best_tip = initial_state.best_tip;
            let new_best_tip = stored.best_tip;

            // Log chain tip change
            if new_best_tip != stale_best_tip {
                if new_best_tip.height > stale_best_tip.height {
                    info!(
                        old_height = stale_best_tip.height.0,
                        new_height = new_best_tip.height.0,
                        old_hash = %stale_best_tip.hash,
                        new_hash = %new_best_tip.hash,
                        "Non-finalized tip advanced"
                    );
                } else if new_best_tip.height == stale_best_tip.height
                    && new_best_tip.hash != stale_best_tip.hash
                {
                    info!(
                        height = new_best_tip.height.0,
                        old_hash = %stale_best_tip.hash,
                        new_hash = %new_best_tip.hash,
                        "Non-finalized tip reorg"
                    );
                } else if new_best_tip.height < stale_best_tip.height {
                    info!(
                        old_height = stale_best_tip.height.0,
                        new_height = new_best_tip.height.0,
                        old_hash = %stale_best_tip.hash,
                        new_hash = %new_best_tip.hash,
                        "Non-finalized tip rollback"
                    );
                }
            }
            Ok(())
        } else {
            Err(UpdateError::StaleSnapshot)
        }
    }

    /// Get a snapshot of the block cache
    pub(super) fn get_snapshot(&self) -> Arc<NonfinalizedBlockCacheSnapshot> {
        self.current.load_full()
    }

    async fn add_nonbest_block(
        &self,
        working_snapshot: &mut NonfinalizedBlockCacheSnapshot,
        block: &impl Block,
    ) -> Result<IndexedBlock, SyncError> {
        match working_snapshot
            .get_block_by_hash_bytes_in_serialized_order(block.prev_hash_bytes_serialized_order())
            .cloned()
        {
            Some(block) => block,
            None => {
                let prev_block = self
                    .source
                    .get_block(HashOrHeight::Hash(
                        zebra_chain::block::Hash::from_bytes_in_serialized_order(
                            block.prev_hash_bytes_serialized_order(),
                        ),
                    ))
                    .await
                    .map_err(|e| {
                        SyncError::ValidatorConnectionError(
                            NodeConnectionError::UnrecoverableError(Box::new(e)),
                        )
                    })?
                    .ok_or(SyncError::ValidatorConnectionError(
                        NodeConnectionError::UnrecoverableError(Box::new(MissingBlockError(
                            "zebrad missing block".to_string(),
                        ))),
                    ))?;
                Box::pin(self.add_nonbest_block(working_snapshot, &*prev_block)).await?
            }
        };
        let provisional_block = block
            .to_indexed_block(&self.source, self.network.clone())
            .await?;
        working_snapshot
            .blocks
            .insert(*provisional_block.hash(), provisional_block.clone());
        Ok(provisional_block)
    }

    // async fn reify(
    //     &self,
    //     new_snapshot: &mut NonfinalizedBlockCacheSnapshot,

    //     finalized_db: &ZainoDB,
    // ) -> Result<(), String> {
    //     let Some((seam_height, seam_hash)) = new_snapshot
    //         .heights_to_hashes
    //         .iter()
    //         .min_by_key(|(height, _hash)| **height)
    //     else {
    //         return Err("tried to reify empty snapshot".to_string());
    //     };
    //     let Some(seam_block) = finalized_db
    //         .backend_for_cap(CapabilityRequest::IndexedBlockExt)
    //         .map_err(|e| format!("backend can't serve indexed blocks: {e}"))?
    //         .get_chain_block(*seam_height)
    //         .await
    //         .map_err(|e| format!("backend error: {e}"))?
    //     else {
    //         return Err("backend missing block below known block".to_string());
    //     };

    //     let mut reify_children_of = HashSet::new();
    //     *new_snapshot.blocks.get_mut(seam_hash).expect("todo") =
    //         seam_block.to_provisional_block(&self).await.expect("todo");
    //     reify_children_of.insert(*seam_block.hash());

    //     while !reify_children_of.is_empty() {
    //         let to_reify: HashMap<BlockHash, IndexedBlock> = new_snapshot
    //             .blocks
    //             .iter()
    //             .filter(|(_hash, block)| reify_children_of.contains(block.parent_hash()))
    //             .map(|(hash, block)| (*hash, block.clone()))
    //             .collect();
    //         for (hash, block) in to_reify.iter() {
    //             let mut block = block.clone();
    //             block.chainwork = block.chainwork.add(
    //                 &new_snapshot
    //                     .blocks
    //                     .get(block.parent_hash())
    //                     .expect("todo")
    //                     .chainwork,
    //             );
    //             new_snapshot.blocks.insert(*hash, block);
    //         }
    //         reify_children_of = to_reify.into_keys().collect();
    //     }

    //     new_snapshot.availability = SnapshotAvailability::Reified;
    //     Ok(())
    // }
}

/// Build an [`IndexedBlock`] from a source block
/// Chainwork will always be provisional
async fn block_to_indexed_block(
    block: &zebra_chain::block::Block,
    source: &impl BlockchainSource,
    network: Network,
) -> Result<IndexedBlock, SyncError> {
    let tree_roots = get_tree_roots_from_source(block.hash().into(), source)
        .await
        .map_err(|e| {
            SyncError::ValidatorConnectionError(NodeConnectionError::UnrecoverableError(Box::new(
                InvalidData(format!("{}", e)),
            )))
        })?;

    indexed_block_from_parts(block, &tree_roots, network).map_err(|e| {
        SyncError::ValidatorConnectionError(NodeConnectionError::UnrecoverableError(Box::new(
            InvalidData(e),
        )))
    })
}
/// Assemble an [`IndexedBlock`] from already-fetched parts. Reuses the
/// shared `BlockWithMetadata` extractors (`extract_block_data`,
/// `extract_transactions`, `create_commitment_tree_data`, `block_work`);
fn indexed_block_from_parts(
    block: &zebra_chain::block::Block,
    tree_roots: &TreeRootData,
    network: Network,
) -> Result<IndexedBlock, String> {
    let (sapling_root, sapling_size, orchard_root, orchard_size) =
        tree_roots.clone().extract_with_defaults();

    let metadata = BlockMetadata::new(
        sapling_root,
        sapling_size as u32,
        orchard_root,
        orchard_size as u32,
        ChainWork::from_u256(U256::zero()),
        network,
    );
    let block_with_metadata = BlockWithMetadata::new(block, metadata);

    let data = block_with_metadata.extract_block_data()?;
    let transactions = block_with_metadata.extract_transactions()?;
    let commitment_tree_data = block_with_metadata.create_commitment_tree_data();
    let chainwork = ChainwChainWork::Provisional;

    let hash = BlockHash::from(block.hash());
    let parent_hash = BlockHash::from(block.header.previous_block_hash);
    let height = block
        .coinbase_height()
        .map(|height| Height(height.0))
        .ok_or_else(|| String::from("Any valid block has a coinbase height"))?;

    Ok(IndexedBlock {
        context: BlockContext {
            index: BlockIndex { height, hash },
            parent_hash,
            chainwork,
        },
        data,
        transactions,
        commitment_tree_data,
    })
}

/// Get commitment tree roots from the blockchain source
async fn get_tree_roots_from_source(
    block_hash: BlockHash,
    source: &impl BlockchainSource,
) -> Result<TreeRootData, super::source::BlockchainSourceError> {
    let (sapling_root_and_len, orchard_root_and_len) =
        source.get_commitment_tree_roots(block_hash).await?;

    Ok(TreeRootData {
        sapling: sapling_root_and_len,
        orchard: orchard_root_and_len,
    })
}
/// Errors that occur during a snapshot update
pub enum UpdateError {
    /// The block reciever disconnected. This should only happen during shutdown.
    ReceiverDisconnected,
    /// The snapshot was already updated by a different process, between when this update started
    /// and when it completed.
    StaleSnapshot,

    /// Something has gone unrecoverably wrong in the finalized
    /// state. A full rebuild is likely needed
    FinalizedStateCorruption,

    /// A block in the snapshot is missing
    DatabaseHole,

    /// Failed to connect to the backing validator
    ValidatorConnectionError(Box<dyn std::error::Error>),
}

trait Block {
    fn hash_bytes_serialized_order(&self) -> [u8; 32];
    async fn to_indexed_block<Source: BlockchainSource>(
        &self,
        source: &Source,
        network: Network,
    ) -> Result<IndexedBlock, SyncError>;
    fn prev_hash_bytes_serialized_order(&self) -> [u8; 32];
}

impl Block for IndexedBlock {
    fn hash_bytes_serialized_order(&self) -> [u8; 32] {
        self.hash().0
    }

    fn prev_hash_bytes_serialized_order(&self) -> [u8; 32] {
        self.context.parent_hash().0
    }

    async fn to_indexed_block<Source: BlockchainSource>(
        &self,
        _source: &Source,
        _network: Network,
    ) -> Result<IndexedBlock, SyncError> {
        Ok(self.clone())
    }
}

impl Block for zebra_chain::block::Block {
    fn hash_bytes_serialized_order(&self) -> [u8; 32] {
        self.hash().bytes_in_serialized_order()
    }

    fn prev_hash_bytes_serialized_order(&self) -> [u8; 32] {
        self.header.previous_block_hash.bytes_in_serialized_order()
    }
    async fn to_indexed_block<Source: BlockchainSource>(
        &self,
        source: &Source,
        network: Network,
    ) -> Result<IndexedBlock, SyncError> {
        block_to_indexed_block(self, source, network).await
    }
}
