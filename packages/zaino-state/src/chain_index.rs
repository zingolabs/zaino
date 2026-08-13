//! Holds Zaino's local chain index.
//!
//! Components:
//! - Mempool: Holds mempool transactions
//! - NonFinalisedState: Holds block data for the top `OPERATIONAL_NFS_DEPTH` blocks of all chains.
//! - FinalisedState: Holds block data for the remainder of the best chain.
//!
//! - Chain: Holds chain / block structs used internally by the ChainIndex.
//!   - Holds fields required to:
//!     - a. Serve CompactBlock data dirctly.
//!     - b. Build trasparent tx indexes efficiently
//!   - NOTE: Full transaction and block data is served from the backend finalizer.

use crate::chain_index::non_finalised_state::ChainIndexSnapshot;
use crate::chain_index::source::GetTransactionLocation;
use crate::chain_index::types::db::metadata::MempoolInfo;
use crate::chain_index::types::helpers::{BlockMetadata, BlockWithMetadata, TreeRootData};
use crate::chain_index::types::BlockIndex;
use crate::chain_index::types::{BestChainLocation, NonBestChainLocation};
use crate::error::{ChainIndexError, ChainIndexErrorKind, FinalisedStateError};
#[cfg(feature = "prometheus")]
use crate::metric_names::*;
use crate::{CompactBlockStream, NonFinalizedState, SyncError, TxOutCompact};
use crate::{IndexedBlock, Outpoint, TransactionHash};
use std::collections::HashSet;
use std::str::FromStr;
use std::{sync::Arc, time::Duration};
use zaino_primitives::types::TxOutSetInfo;
use zaino_status::{NamedAtomicStatus, Status, StatusType};

use arc_swap::ArcSwapOption;
use futures::Stream;
use non_finalised_state::NonfinalizedBlockCacheSnapshot;
use source::BlockchainSource;
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;
use tracing::{info, instrument};
use zaino_consensus::validate_raw_transaction_hex;
use zaino_mempool::ports::TipAwareMempool as _;
use zaino_primitives::types::rpc::{
    AddressDeltas, AddressDeltasRequest, BlockDeltas, BlockHeaderVerbose, BlockSubsidy, MiningInfo,
    NodeInfo, PeerInfo,
};
use zaino_proto::proto::utils::{prune_compact_block, PoolTypeFilter};
use zebra_chain::parameters::ConsensusBranchId;
pub use zebra_chain::parameters::Network as ZebraNetwork;
use zebra_chain::serialization::ZcashSerialize;
use zebra_rpc::{
    client::{GetAddressBalanceRequest, GetAddressTxIdsRequest},
    methods::GetBlock,
};
use zebra_state::HashOrHeight;

pub mod encoding;
/// All state below [`OPERATIONAL_NFS_DEPTH`] blocks of the best-known chain tip.
pub mod finalised_state;
mod mempool;

/// How long the mempool may stay frozen before the sync loop says so.
///
/// A freeze is the normal shape of a tip transition and clears in well under a
/// second. This is two orders of magnitude above that, so it fires only when the
/// validator tip and Zaino's have genuinely stopped agreeing — a state in which
/// tip-coherent reads have been failing the whole time, and which otherwise
/// leaves no trace in the log.
const COHERENCE_FREEZE_ESCALATION: std::time::Duration = std::time::Duration::from_secs(120);

/// Bridge `zaino-state`'s legacy txid type to the domain one the mempool speaks.
///
/// Both are the same 32 bytes in the same order; the mempool subsystem was built
/// on `zaino-primitives` and this crate has not finished moving. One function
/// rather than an inline `From` at each call site so the conversion disappears
/// in a single edit once it has.
fn types_txid_to_domain(txid: &types::TransactionHash) -> zaino_primitives::types::TransactionId {
    zaino_primitives::types::TransactionId::from(<[u8; 32]>::from(*txid))
}
mod network_adoption;
/// State within [`OPERATIONAL_NFS_DEPTH`] blocks of the best-known chain tip;
/// stored separately as it may be reorged.
pub mod non_finalised_state;
/// ChainIndex's driven port onto the validator. Temporary scaffolding — see
/// the module docs.
pub mod source;
pub mod source_ports;
/// Common types used by the rest of this module
pub mod types;
pub mod validator_source;

#[cfg(test)]
mod tests;

/// Distance (in blocks) between the best-known chain tip and the highest block that
/// zaino treats as part of the finalised DB — the finalised / non-finalised seam.
///
/// Sourced from the workspace's single source of truth,
/// [`zaino_consensus`]. Production uses the real
/// [`MAX_NONFINALISED_DEPTH`]. The tractable [`FAST_TEST_MAX_NONFINALISED_DEPTH`]
/// (= depth / 10) is selected for in-crate unit tests (`cfg(test)`) *and* for
/// cross-crate live tests that enable the `fast-test-seam` feature — so short mock
/// fixtures and small live chains still exercise a *moving* finalised seam. At the
/// real depth `finalized_height_floor` saturates to genesis for those fixtures and
/// the eviction/seam invariants become untestable (see zingolabs/zaino#1288). Both
/// arms derive from the same upstream reorg bound, so neither is a hard-coded literal.
///
/// [`MAX_NONFINALISED_DEPTH`]: zaino_consensus::MAX_NONFINALISED_DEPTH
/// [`FAST_TEST_MAX_NONFINALISED_DEPTH`]: zaino_consensus::FAST_TEST_MAX_NONFINALISED_DEPTH
#[cfg(not(any(test, feature = "fast-test-seam")))]
pub(crate) const OPERATIONAL_NFS_DEPTH: u32 = zaino_consensus::MAX_NONFINALISED_DEPTH;
#[cfg(any(test, feature = "fast-test-seam"))]
pub(crate) const OPERATIONAL_NFS_DEPTH: u32 = zaino_consensus::FAST_TEST_MAX_NONFINALISED_DEPTH;

/// Lower bound on zaino's finalized-DB tip, derived from the current
/// best-known chain tip.
///
/// After a chain-shortening reorg this floor can move backwards while
/// the on-disk `finalized_height` does not — finalized blocks are
/// never evicted. Callers comparing this floor against
/// `finalized_height` should account for the asymmetry
/// (see zingolabs/zaino#1128).
pub(crate) fn finalized_height_floor(chain_tip: u32) -> crate::Height {
    crate::Height(chain_tip.saturating_sub(OPERATIONAL_NFS_DEPTH))
}

/// Current wall-clock time as a Unix timestamp in fractional seconds, for
/// "event happened at" gauges. Falls back to `0.0` if the clock is before the
/// Unix epoch (never in practice).
#[cfg(feature = "prometheus")]
pub(crate) fn unix_now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Builds a zcashd-compatible `getchaintips` response from the local non-finalized snapshot.
///
/// zcashd enumerates block-tree leaves, always includes the active tip, and reports
/// inactive fully-known branches as `valid-fork`. Zaino's non-finalized cache stores
/// full blocks, not headers-only or invalid candidates, so those are the only statuses
/// this conversion can currently emit.
pub(crate) fn chain_tips_from_nonfinalized_snapshot(
    snapshot: &NonfinalizedBlockCacheSnapshot,
) -> Vec<zaino_primitives::types::rpc::ChainTip> {
    use zaino_primitives::types::rpc::{ChainTip, ChainTipStatus};

    let parent_hashes = snapshot
        .blocks
        .values()
        .map(|block| *block.context.parent_hash())
        .collect::<HashSet<_>>();

    let mut tip_hashes = snapshot
        .blocks
        .keys()
        .filter(|hash| !parent_hashes.contains(hash))
        .copied()
        .collect::<HashSet<_>>();
    tip_hashes.insert(snapshot.best_tip.hash);

    let mut tips = tip_hashes
        .into_iter()
        .filter_map(|hash| snapshot.blocks.get(&hash))
        .map(|block| {
            let is_active_tip = block.hash() == &snapshot.best_tip.hash;
            let status = if is_active_tip {
                ChainTipStatus::Active
            } else {
                ChainTipStatus::ValidFork
            };
            let branch_len = if is_active_tip {
                0
            } else {
                branch_len_to_active_chain(snapshot, block)
            };

            // Every height in the index passed `Height::try_from` when the block
            // was ingested, so an out-of-range value here means the index itself
            // is corrupt. Propagating would make a pure snapshot read fallible
            // and ripple through its callers for a state that cannot arise.
            let height = zaino_primitives::types::Height::try_from(u32::from(block.height()))
                .expect("an indexed block's height is within the protocol maximum");

            ChainTip {
                height,
                hash: zaino_primitives::types::BlockHash::from(block.hash().0),
                branch_len,
                status,
            }
        })
        .collect::<Vec<_>>();

    // Descending height, then ascending hash. The tie-break compares
    // *display-order* bytes, which is the ordering the hex strings this
    // previously sorted on would produce — hashes are byte-reversed for display,
    // so sorting internal bytes would silently reorder equal-height tips.
    tips.sort_by(|left, right| {
        let display_order = |hash: zaino_primitives::types::BlockHash| {
            let mut bytes = <[u8; 32]>::from(hash);
            bytes.reverse();
            bytes
        };
        right
            .height
            .cmp(&left.height)
            .then_with(|| display_order(left.hash).cmp(&display_order(right.hash)))
    });
    tips
}

fn branch_len_to_active_chain(
    snapshot: &NonfinalizedBlockCacheSnapshot,
    block: &IndexedBlock,
) -> u32 {
    let mut branch_len = 0;
    let mut current = block;

    loop {
        if snapshot.heights_to_hashes.get(&current.height()) == Some(current.hash()) {
            return branch_len;
        }

        branch_len += 1;

        let parent_hash = current.context.parent_hash();
        let Some(parent) = snapshot.blocks.get(parent_hash) else {
            return branch_len;
        };
        current = parent;
    }
}

/// The interface to the chain index.
///
/// `ChainIndex` provides a unified interface for querying blockchain data from different
/// backend sources. It combines access to both finalized state (older than `OPERATIONAL_NFS_DEPTH` blocks) and
/// non-finalized state (recent blocks that may still be reorganized).
///
/// # Implementation
///
/// The primary implementation is [`NodeBackedChainIndex`], which can be backed by either:
/// - Direct read access to a zebrad database via `ReadStateService` (preferred)
/// - A JSON-RPC connection to a validator node (zcashd, zebrad, or another zainod)
///
/// # Constructing one
///
/// Both backends are selected by config and built through
/// [`NodeBackedIndexerService`](crate::NodeBackedIndexerService), which
/// resolves the connection type, probes the validator, adopts its activation
/// schedule and waits for the initial sync:
///
/// ```no_run
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// use zaino_state::{
///     LightWalletService, NodeBackedIndexerService, NodeBackedIndexerServiceConfig, ZcashService,
/// };
///
/// // `ValidatorConnectionType::Rpc` reaches the validator over JSON-RPC only;
/// // `Direct` additionally reads its state database, and is preferred where
/// // available.
/// let config = NodeBackedIndexerServiceConfig::default();
/// let service = NodeBackedIndexerService::spawn(config).await?;
/// # Ok(())
/// # }
/// ```
///
/// Consumers then query through the service's subscriber, which implements this
/// trait. A snapshot pins the non-finalised state so a sequence of queries sees
/// one consistent chain:
///
/// ```no_run
/// # async fn example(
/// #     subscriber: impl zaino_state::ChainIndex,
/// # ) -> Result<(), Box<dyn std::error::Error>> {
/// use zaino_state::ChainIndex as _;
///
/// let snapshot = subscriber.snapshot_nonfinalized_state().await?;
/// let tip = subscriber.best_chaintip(&snapshot).await?;
/// # Ok(())
/// # }
/// ```
///
/// When a call asks for info (e.g. a block), Zaino selects sources in this order:
#[doc = simple_mermaid::mermaid!("chain_index_passthrough.mmd")]
///
/// This trait holds the core methods required by the embedded wallet consumer
/// (zallet). RPC-server-only methods live on the [`ChainIndexRpcExt`] extension
/// trait.
///
/// TODO: The core/extension split is a provisional first pass. It should be refined
/// into finer capability-based traits (zallet / lwd / block-explorer) in a follow-up
/// PR.
pub trait ChainIndex {
    /// A snapshot of the nonfinalized state, needed for atomic access
    type Snapshot: NonFinalizedSnapshot;

    /// How it can fail
    type Error;

    // ********** Utility methods **********

    /// Takes a snapshot of the non_finalized state. All NFS-interfacing query
    /// methods take a snapshot. The query will check the index
    /// it existed at the moment the snapshot was taken.
    fn snapshot_nonfinalized_state(
        &self,
    ) -> impl std::future::Future<Output = Result<Self::Snapshot, Self::Error>>;

    // ********** Block methods **********

    /// Returns Some(Height) for the given block hash *if* it is currently in the best chain.
    ///
    /// Returns None if the specified block is not in the best chain or is not found.
    fn get_block_height(
        &self,
        snapshot: &Self::Snapshot,
        hash: types::BlockHash,
    ) -> impl std::future::Future<Output = Result<Option<types::Height>, Self::Error>>;

    /// Returns Some(BlockHash) for the given block height.in the best chain.
    ///
    /// Returns None if the specified block height is above the best chain tip.
    fn get_block_hash(
        &self,
        snapshot: &Self::Snapshot,
        hash: types::Height,
    ) -> impl std::future::Future<Output = Result<Option<types::BlockHash>, Self::Error>>;

    /// Returns Some(IndexedBlock) for the given block hash.
    ///
    /// Returns None if the specified block is not found.
    ///
    /// **NOTE: This Method is currently not "passthrough aware", cumulative
    /// chain work must be made optional to enable this.**
    fn get_indexed_block_by_hash(
        &self,
        snapshot: &Self::Snapshot,
        target_hash: &types::BlockHash,
    ) -> impl std::future::Future<Output = Result<Option<IndexedBlock>, Self::Error>>;

    /// Returns Some(IndexedBlock) for the given block height.in the best chain.
    ///
    /// Returns None if the specified block height is above the best chain tip.
    ///
    /// **NOTE: This Method is currently not "passthrough aware", cumulative
    /// chain work must be made optional to enable this.**
    fn get_indexed_block_by_height(
        &self,
        snapshot: &Self::Snapshot,
        target_height: &types::Height,
    ) -> impl std::future::Future<Output = Result<Option<IndexedBlock>, Self::Error>>;

    /// Given inclusive start and end heights, stream all blocks
    /// between the given heights.
    /// Returns None if the specified end height
    /// is greater than the snapshot's tip
    #[allow(clippy::type_complexity)]
    fn get_block_range(
        &self,
        snapshot: &Self::Snapshot,
        start: types::Height,
        end: Option<types::Height>,
    ) -> Option<impl futures::Stream<Item = Result<Vec<u8>, Self::Error>>>;

    // ********** Transaction methods **********

    /// given a transaction id, returns the transaction, along with
    /// its consensus branch ID if available
    #[allow(clippy::type_complexity)]
    fn get_raw_transaction(
        &self,
        snapshot: &Self::Snapshot,
        txid: &types::TransactionHash,
    ) -> impl std::future::Future<Output = Result<Option<(Vec<u8>, Option<u32>)>, Self::Error>>;

    /// Given a transaction ID, returns all known hashes and heights of blocks
    /// containing that transaction.
    ///
    /// Also returns if the transaction is in the mempool (and whether that mempool is
    /// in-sync with the provided snapshot)
    #[allow(clippy::type_complexity)]
    fn get_transaction_status(
        &self,
        snapshot: &Self::Snapshot,
        txid: &types::TransactionHash,
    ) -> impl std::future::Future<
        Output = Result<(Option<BestChainLocation>, HashSet<NonBestChainLocation>), Self::Error>,
    >;

    /// Returns all txids currently in the mempool.
    fn get_mempool_txids(
        &self,
    ) -> impl std::future::Future<Output = Result<Vec<types::TransactionHash>, Self::Error>>;

    /// Returns all transactions currently in the mempool, minus those matching
    /// `exclude_list`.
    ///
    /// Each entry of `exclude_list` is a raw txid *suffix* in the client's byte
    /// order, exactly as it arrives on the wire — no hex, no reversal. Returning
    /// entries rather than bytes lets the caller reach the shared `Bytes` buffer
    /// without a copy, and gives it the entry height it needs to serve one.
    ///
    /// Rejects a list that is too long, or a suffix too short to identify a
    /// transaction, rather than clamping: silently truncating would serve
    /// transactions the caller believes it excluded.
    fn get_mempool_transactions(
        &self,
        exclude_list: Vec<Vec<u8>>,
    ) -> impl std::future::Future<
        Output = Result<Vec<std::sync::Arc<zaino_mempool::MempoolEntry>>, Self::Error>,
    >;

    /// Returns a stream of mempool transactions, ending the stream when the chain tip block hash
    /// changes (a new block is mined or a reorg occurs).
    ///
    /// If a snapshot is given and the chain tip has changed from the given spanshot, returns None.
    #[allow(clippy::type_complexity)]
    fn get_mempool_stream(
        &self,
        snapshot: Option<&Self::Snapshot>,
    ) -> Option<impl futures::Stream<Item = Result<bytes::Bytes, Self::Error>>>;

    // ********** Chain methods **********

    /// Get the tip of the best chain, according to the snapshot
    fn best_chaintip(
        &self,
        nonfinalized_snapshot: &Self::Snapshot,
    ) -> impl std::future::Future<Output = Result<BlockIndex, Self::Error>>;

    /// Finds the newest ancestor of the given block on the main
    /// chain, or the block itself if it is on the main chain.
    fn find_fork_point(
        &self,
        snapshot: &Self::Snapshot,
        hash: &types::BlockHash,
    ) -> impl std::future::Future<Output = Result<Option<(types::BlockHash, types::Height)>, Self::Error>>;

    /// Returns the block commitment tree data by hash.
    ///
    /// The hash must exist in the non-finalized snapshot or finalized database
    /// before the request is proxied to the backing validator.
    #[allow(clippy::type_complexity)]
    fn get_treestate(
        &self,
        hash: &types::BlockHash,
    ) -> impl std::future::Future<
        Output = Result<
            (
                Option<source::PoolTreestate>,
                Option<source::PoolTreestate>,
                Option<source::PoolTreestate>,
            ),
            Self::Error,
        >,
    >;

    /// Returns the subtree roots
    fn get_subtree_roots(
        &self,
        pool: ShieldedPool,
        start_index: u16,
        max_entries: Option<u16>,
    ) -> impl std::future::Future<Output = Result<Vec<([u8; 32], u32)>, Self::Error>>;

    // ********** Transparent address history methods **********

    /// Returns the total transparent balance for the given addresses.
    fn get_address_balance(
        &self,
        address_strings: GetAddressBalanceRequest,
    ) -> impl std::future::Future<Output = Result<zaino_primitives::types::AddressBalance, Self::Error>>;

    /// Returns the transaction ids made by the given transparent addresses.
    fn get_address_txids(
        &self,
        request: GetAddressTxIdsRequest,
    ) -> impl std::future::Future<Output = Result<Vec<types::TransactionHash>, Self::Error>>;

    /// Returns all unspent transparent outputs for the given addresses.
    fn get_address_utxos(
        &self,
        address_strings: GetAddressBalanceRequest,
    ) -> impl std::future::Future<Output = Result<Vec<zaino_primitives::types::Utxo>, Self::Error>>;

    /// For each outpoint, returns the txid of the transaction that spent it on the best
    /// chain, or `None` if the outpoint is unspent or unknown.
    ///
    /// The output is aligned with the input by index: `result[i]` corresponds to
    /// `outpoints[i]`. An outpoint is spent at most once on the best chain. `scope` selects
    /// how far the search reaches: [`ChainScope::FullChain`] searches the non-finalised best
    /// chain first then the finalised index; [`ChainScope::Finalised`] searches only the
    /// finalised index, yielding reorg-stable results.
    ///
    /// [`ChainScope::FullChain`]: types::ChainScope::FullChain
    /// [`ChainScope::Finalised`]: types::ChainScope::Finalised
    fn get_outpoint_spenders(
        &self,
        snapshot: &Self::Snapshot,
        outpoints: Vec<types::Outpoint>,
        scope: types::ChainScope,
    ) -> impl std::future::Future<Output = Result<Vec<Option<types::TransactionHash>>, Self::Error>>;
}

/// RPC-server extension methods layered on top of [`ChainIndex`].
///
/// The core [`ChainIndex`] trait holds the subset required by the embedded wallet
/// consumer (zallet). This extension holds the additional functionality required by
/// the gRPC (lightwalletd) and JSON-RPC servers: compact-block serving, mempool
/// metadata, address deltas, and the block-explorer / node-passthrough RPCs.
///
/// TODO: This two-way core/extension split is a provisional first pass. It should be
/// refined into finer capability-based traits (zallet / lwd / block-explorer) in a
/// follow-up PR, at which point methods will be redistributed to their narrowest
/// capability.
pub trait ChainIndexRpcExt: ChainIndex {
    // ********** Block methods **********

    /// Returns the *compact* block for the given height.
    ///
    /// Returns `None` if the specified `height` is greater than the snapshot's tip.
    ///
    /// ## Pool filtering
    ///
    /// - `pool_types` controls which per-transaction components are populated.
    /// - Transactions that contain no elements in any requested pool are omitted from `vtx`.
    ///   The original transaction index is preserved in `CompactTx.index`.
    /// - `PoolTypeFilter::default()` preserves the legacy behaviour (only Sapling and Orchard
    ///   components are populated).
    #[allow(clippy::type_complexity)]
    fn get_compact_block(
        &self,
        nonfinalized_snapshot: &Self::Snapshot,
        height: types::Height,
        pool_types: PoolTypeFilter,
    ) -> impl std::future::Future<
        Output = Result<Option<zaino_proto::proto::compact_formats::CompactBlock>, Self::Error>,
    >;

    /// Streams *compact* blocks for an inclusive height range.
    ///
    /// Returns `None` if the requested range is entirely above the snapshot's tip.
    ///
    /// - The stream covers `[start_height, end_height]` (inclusive).
    /// - If `start_height <= end_height` the stream is ascending; otherwise it is descending.
    ///
    /// ## Pool filtering
    ///
    /// - `pool_types` controls which per-transaction components are populated.
    /// - Transactions that contain no elements in any requested pool are omitted from `vtx`.
    ///   The original transaction index is preserved in `CompactTx.index`.
    /// - `PoolTypeFilter::default()` preserves the legacy behaviour (only Sapling and Orchard
    ///   components are populated).
    #[allow(clippy::type_complexity)]
    fn get_compact_block_stream(
        &self,
        nonfinalized_snapshot: &Self::Snapshot,
        start_height: types::Height,
        end_height: types::Height,
        pool_types: PoolTypeFilter,
    ) -> impl std::future::Future<Output = Result<Option<CompactBlockStream>, Self::Error>>;

    /// Returns the `getblock`-shaped block for the given hash-or-height string.
    ///
    /// `verbosity` follows the zcashd `getblock` convention (0 = raw, 1 = object with
    /// txids, 2 = object with full transaction data).
    ///
    /// zcashd reference: [`getblock`](https://zcash.github.io/rpc/getblock.html)
    fn z_get_block(
        &self,
        hash_or_height: String,
        verbosity: Option<u8>,
    ) -> impl std::future::Future<Output = Result<GetBlock, Self::Error>>;

    /// Returns the `getblockheader`-shaped header for the given block hash.
    ///
    /// zcashd reference: [`getblockheader`](https://zcash.github.io/rpc/getblockheader.html)
    fn get_block_header(
        &self,
        hash: String,
    ) -> impl std::future::Future<Output = Result<BlockHeaderVerbose, Self::Error>>;

    /// Returns the raw serialised header of the block with the given hash.
    ///
    /// The non-verbose half of `getblockheader`; see
    /// [`BlockchainSource::get_raw_block_header`].
    fn get_raw_block_header(
        &self,
        hash: String,
    ) -> impl std::future::Future<Output = Result<Vec<u8>, Self::Error>>;

    /// Returns the `getblockdeltas`-shaped transparent input/output deltas for the block
    /// with the given hash.
    ///
    /// zcashd reference: [`getblockdeltas`](https://zcash.github.io/rpc/getblockdeltas.html)
    fn get_block_deltas(
        &self,
        hash: String,
    ) -> impl std::future::Future<Output = Result<BlockDeltas, Self::Error>>;

    /// Returns the proof-of-work difficulty of the best chain as a multiple of the
    /// minimum difficulty.
    ///
    /// zcashd reference: [`getdifficulty`](https://zcash.github.io/rpc/getdifficulty.html)
    fn get_difficulty(&self) -> impl std::future::Future<Output = Result<f64, Self::Error>>;

    // ********** Node-passthrough methods **********
    //
    // No local-index equivalent; always delegate to the backing validator.

    /// Returns the `getinfo` response.
    fn get_info(&self) -> impl std::future::Future<Output = Result<NodeInfo, Self::Error>>;

    /// Returns the `getblockchaininfo` response.
    fn get_blockchain_info(
        &self,
    ) -> impl std::future::Future<Output = Result<zaino_primitives::types::BlockchainInfo, Self::Error>>;

    /// Returns the `getpeerinfo` response.
    fn get_peer_info(
        &self,
    ) -> impl std::future::Future<Output = Result<Vec<PeerInfo>, Self::Error>>;

    /// Returns the `getblocksubsidy` response at the given height.
    fn get_block_subsidy(
        &self,
        height: u32,
    ) -> impl std::future::Future<Output = Result<BlockSubsidy, Self::Error>>;

    /// Returns the `getmininginfo` response.
    fn get_mining_info(&self)
        -> impl std::future::Future<Output = Result<MiningInfo, Self::Error>>;

    /// Returns the `gettxout` response for the given outpoint.
    fn get_tx_out(
        &self,
        txid: String,
        n: u32,
        include_mempool: Option<bool>,
    ) -> impl std::future::Future<
        Output = Result<Option<zaino_primitives::types::rpc::TxOut>, Self::Error>,
    >;

    /// Returns the `getspentinfo` response for the given request.
    fn get_spent_info(
        &self,
        outpoint: zaino_primitives::types::rpc::SpentOutpoint,
    ) -> impl std::future::Future<Output = Result<zaino_primitives::types::rpc::SpentInfo, Self::Error>>;

    /// Returns the `getnetworksolps` response.
    fn get_network_sol_ps(
        &self,
        blocks: Option<i32>,
        height: Option<i32>,
    ) -> impl std::future::Future<Output = Result<u64, Self::Error>>;

    /// Submits a raw transaction to the network (`sendrawtransaction`).
    fn send_raw_transaction(
        &self,
        raw_transaction_hex: String,
    ) -> impl std::future::Future<Output = Result<zaino_primitives::types::TransactionId, Self::Error>>;

    /// Returns the full `z_gettreestate` response for the given hash-or-height, via the
    /// backing validator (node-passthrough fallback for treestates not locally serviceable).
    fn get_treestate_by_id(
        &self,
        hash_or_height: String,
    ) -> impl std::future::Future<Output = Result<zaino_primitives::types::Treestate, Self::Error>>;

    // ********** Transparent address history methods **********

    /// Returns all changes for the given transparent addresses.
    fn get_address_deltas(
        &self,
        params: AddressDeltasRequest,
    ) -> impl std::future::Future<Output = Result<AddressDeltas, Self::Error>>;

    // ********** Metadata methods **********

    /// Returns Information about the mempool state:
    /// - size: Current tx count
    /// - bytes: Sum of all tx sizes
    /// - usage: Total memory usage for the mempool
    fn get_mempool_info(&self) -> impl std::future::Future<Output = MempoolInfo>;

    /// Returns the full `gettxoutsetinfo` response, folding the non-finalised state on top of
    /// the finalised txout-set accumulator.
    ///
    /// Returns `None` while the indexer is still syncing the finalised state (the
    /// accumulator's spent-index invariants are not yet established). The wire
    /// layer renders that as zcashd's empty object.
    fn get_tx_out_set_info(
        &self,
    ) -> impl std::future::Future<Output = Result<Option<TxOutSetInfo>, Self::Error>>;
}

/// The combined index. Contains a view of the mempool, and the full
/// chain state, both finalized and non-finalized, to allow queries over
/// the entire chain at once.
///
/// This is the primary implementation backing [`ChainIndex`] and replaces the functionality
/// previously provided by `FetchService` and `StateService`. It can be backed by either:
/// - A zebra `ReadStateService` for direct database access (preferred for performance)
/// - A JSON-RPC connection to any validator node (zcashd, zebrad, or another zainod)
///
/// To use the [`ChainIndex`] trait methods, call [`subscriber()`](NodeBackedChainIndex::subscriber)
/// to get a [`NodeBackedChainIndexSubscriber`] which implements the trait.
///
/// # Construction
///
/// Use [`NodeBackedChainIndex::new()`] with:
/// - A source implementing [`BlockchainSource`] — in production
///   [`ZebraValidatorSource`](crate::chain_index::validator_source::ZebraValidatorSource),
///   built by `spawn_rpc` or `spawn_direct` from the backend config
/// - A [`ChainIndexConfig`](crate::ChainIndexConfig) containing cache and database settings
///
/// ```no_run
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// use zaino_state::chain_index::validator_source::ZebraValidatorSource;
/// use zaino_state::{ChainIndexConfig, NodeBackedChainIndex};
///
/// # let common = unimplemented!();
/// // `spawn_rpc` reaches the validator over JSON-RPC only; `spawn_direct`
/// // additionally reads its state database, and is preferred where available.
/// // Both adopt the validator's activation schedule at first contact.
/// let (source, _node_info, network) = ZebraValidatorSource::spawn_rpc(common).await?;
///
/// let chain_index =
///     NodeBackedChainIndex::new(source, ChainIndexConfig::from_backend_config(common, network))
///         .await?;
/// let subscriber = chain_index.subscriber().await;
/// # Ok(())
/// # }
/// ```
///
/// Most consumers should not build one directly:
/// [`NodeBackedIndexerService`](crate::NodeBackedIndexerService) does all of the
/// above from config, and additionally waits for the initial sync to complete.
///
/// # Current Features
///
/// - Full mempool support including streaming and filtering
/// - Unified access to finalized and non-finalized blockchain state
/// - Automatic synchronization between state layers
/// - Snapshot-based consistency for queries
#[derive(Debug)]
pub struct NodeBackedChainIndex<
    Source: BlockchainSource = crate::chain_index::validator_source::ZebraValidatorSource,
> {
    /// The tip-agnostic mempool: always live, never frozen.
    mempool: std::sync::Arc<mempool::ChainIndexMempool<Source>>,
    /// The tip-aware view over it, for the reads that place a transaction
    /// relative to a chain tip.
    coherence: std::sync::Arc<mempool::ChainIndexCoherence<Source>>,
    /// Fired by the sync loop after each non-finalized publication, so the
    /// coherence layer thaws on the block rather than on its next poll tick.
    /// Without it every block is followed by a freeze lasting a full tick.
    nfs_epoch_signal: tokio::sync::watch::Sender<()>,
    /// Fired by the sync loop when the chain height moves, giving the mempool a
    /// push path the source does not have. A hint only — see
    /// [`MempoolSourceAdapter`](mempool::MempoolSourceAdapter).
    block_wake_signal: tokio::sync::watch::Sender<()>,
    non_finalized_state: Arc<ArcSwapOption<crate::NonFinalizedState<Source>>>,
    finalized_db: std::sync::Arc<finalised_state::FinalisedState<Source>>,
    sync_loop_handle: Option<tokio::task::JoinHandle<Result<(), SyncError>>>,
    status: NamedAtomicStatus,
    network: ZebraNetwork,
    source: Source,
    sync_timings: SyncTimings,
    /// Signals the sync worker to exit cooperatively. `shutdown()` fires
    /// `cancel_token.cancel()` *before* tearing down `finalized_db`, so the
    /// worker wakes from any in-flight `tokio::time::sleep` and returns
    /// `Ok(())` instead of cycling through the failure-escalation ladder
    /// once `fs.*` calls start failing. Closes the race tracked in #1098.
    cancel_token: CancellationToken,
}

/// Timing parameters for the ChainIndex sync loop.
///
/// [`SyncTimings::default()`] produces production values (500 ms inter-iteration
/// sleep, 250 ms initial backoff doubling up to 8 s, 10 consecutive failures
/// before escalating to [`StatusType::CriticalError`] — ~40 s total window).
/// [`SyncTimings::fast()`] shrinks each duration by 10× so backoff-dependent
/// unit tests finish in ~4 s instead of ~40 s.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SyncTimings {
    pub(crate) interval: Duration,
    pub(crate) initial_backoff: Duration,
    pub(crate) max_backoff: Duration,
    pub(crate) max_consecutive_failures: u32,
}

impl Default for SyncTimings {
    fn default() -> Self {
        Self {
            interval: Duration::from_millis(500),
            initial_backoff: Duration::from_millis(250),
            max_backoff: Duration::from_secs(8),
            max_consecutive_failures: 10,
        }
    }
}

#[cfg(test)]
impl SyncTimings {
    /// 10× faster than [`Self::default`] — test-only.
    pub(crate) const fn fast() -> Self {
        Self {
            interval: Duration::from_millis(50),
            initial_backoff: Duration::from_millis(25),
            max_backoff: Duration::from_millis(800),
            max_consecutive_failures: 10,
        }
    }

    /// Upper bound on the cumulative sleep the sync loop performs before
    /// escalating to [`StatusType::CriticalError`] under persistent failure.
    ///
    /// Sums backoff delays for failures `1..max_consecutive_failures` — the
    /// final failure sets CriticalError without sleeping.
    pub(crate) fn max_backoff_window(&self) -> Duration {
        let mut total = Duration::ZERO;
        let mut current = self.initial_backoff;
        for _ in 0..self.max_consecutive_failures.saturating_sub(1) {
            total += current;
            current = (current * 2).min(self.max_backoff);
        }
        total
    }
}

impl<Source: BlockchainSource> NodeBackedChainIndex<Source> {
    /// Creates a new chainindex from a connection to a validator.
    ///
    /// In production `Source` is
    /// [`ZebraValidatorSource`](crate::chain_index::validator_source::ZebraValidatorSource),
    /// which routes each query to Zebra's read-state service or its JSON-RPC
    /// interface according to which can answer it.
    pub async fn new(
        source: Source,
        config: crate::config::ChainIndexConfig,
    ) -> Result<Self, crate::InitError> {
        Self::new_with_sync_timings(source, config, SyncTimings::default()).await
    }

    /// Like [`Self::new`] but overrides the sync-loop timings. Intended for
    /// tests that exercise the backoff path and need a faster schedule.
    pub(crate) async fn new_with_sync_timings(
        source: Source,
        config: crate::config::ChainIndexConfig,
        sync_timings: SyncTimings,
    ) -> Result<Self, crate::InitError> {
        let finalized_db =
            Arc::new(finalised_state::FinalisedState::spawn(config.clone(), source.clone()).await?);

        // Built before the mempool services so the epoch observer can share the
        // very handle the sync loop publishes into. A separate handle would let
        // the two drift, and the coherence layer would be freezing against a tip
        // nobody was being served.
        let non_finalized_state = Arc::new(ArcSwapOption::empty());

        let (nfs_epoch_signal, nfs_epoch_wake) = tokio::sync::watch::channel(());
        let (block_wake_signal, block_wake) = tokio::sync::watch::channel(());

        let cancel_token = CancellationToken::new();

        let mempool = zaino_mempool_service::MempoolService::spawn(
            mempool::MempoolSourceAdapter::new(source.clone(), block_wake),
            config.mempool.clone(),
            cancel_token.child_token(),
        );

        let coherence = zaino_mempool_service::CoherenceService::spawn(
            mempool.subscriber(),
            mempool::NfsEpochAdapter::new(non_finalized_state.clone(), nfs_epoch_wake),
            // Cloned rather than rebuilt: `MempoolConfig` shares its
            // `max_cost_bytes` cell across clones, so an operator changing the
            // bound moves both services at once.
            config.mempool.clone(),
            cancel_token.child_token(),
        );

        let mut chain_index = Self {
            mempool,
            coherence,
            nfs_epoch_signal,
            block_wake_signal,
            non_finalized_state,
            finalized_db,
            sync_loop_handle: None,
            status: NamedAtomicStatus::new("ChainIndex", StatusType::Spawning),
            network: config.network.clone(),
            source,
            sync_timings,
            cancel_token,
        };
        chain_index.sync_loop_handle = Some(chain_index.start_sync_loop());

        Ok(chain_index)
    }

    /// Creates a [`NodeBackedChainIndexSubscriber`] from self,
    /// a clone-safe, drop-safe, read-only view onto the running indexer.
    pub fn subscriber(&self) -> NodeBackedChainIndexSubscriber<Source> {
        NodeBackedChainIndexSubscriber {
            mempool: self.mempool.subscriber(),
            coherence: self.coherence.subscriber(),
            non_finalized_state: self.non_finalized_state.clone(),
            finalized_state: self.finalized_db.to_reader(),
            status: self.status.clone(),
            network: self.network.clone(),
            source: self.source.clone(),
        }
    }

    /// Shut down the sync process, for a cleaner drop.
    /// An error indicates a failure to cleanly shutdown. Dropping the
    /// chain index should still stop everything.
    ///
    /// Order matters: `cancel_token.cancel()` runs *before* `fs.shutdown()`
    /// so the sync worker wakes from its post-iter sleep and exits via the
    /// cancellation arm. If we tore down `fs` first, the worker's next
    /// `fs.sync_to_height` call would fail, the failure path would
    /// `tokio::time::sleep(current_backoff)`, and only the cancellation
    /// arm on *that* sleep would release the worker — which is exactly
    /// the design we have. Cancelling first just removes the wasted
    /// failure-path round trip.
    pub async fn shutdown(&self) -> Result<(), FinalisedStateError> {
        // The synchronous teardown (cancellation, mempool close, source
        // release) runs before the fallible DB shutdown so a DB error cannot
        // skip it — the source's Zebra syncer task must not outlive the index.
        self.shutdown_sync_best_effort();
        self.finalized_db.shutdown().await
    }

    /// Synchronous best-effort teardown for contexts that cannot run async
    /// work (a `Drop` on a current-thread runtime, or on a thread with no
    /// runtime at all): cancels the sync worker, closes the mempool, and
    /// releases source-owned background work. The finalised DB's async
    /// [`shutdown`](Self::shutdown) step is skipped; the DB's own `Drop`
    /// remains responsible for releasing its resources.
    pub(crate) fn shutdown_sync_best_effort(&self) {
        self.cancel_token.cancel();
        self.status.store(StatusType::Closing);
        self.mempool.close();
        self.source.shutdown();
    }

    /// How long tip-coherent mempool reads have been frozen, or `None` if live.
    ///
    /// See [`NodeBackedChainIndexSubscriber::mempool_coherence_health`].
    pub fn mempool_coherence_health(&self) -> Option<std::time::Duration> {
        self.coherence.subscriber().frozen_for()
    }

    /// Displays the status of the chain_index
    pub fn status(&self) -> StatusType {
        let finalized_status = self.finalized_db.status();
        let mempool_status = self.mempool.status();
        let combined_status = self
            .status
            .load()
            .combine(finalized_status)
            .combine(mempool_status);
        self.status.store(combined_status);
        combined_status
    }

    #[instrument(name = "ChainIndex::start_sync_loop", skip(self))]
    pub(super) fn start_sync_loop(&self) -> tokio::task::JoinHandle<Result<(), SyncError>> {
        info!("Starting ChainIndex sync loop");
        let nfs = self.non_finalized_state.clone();
        let fs = self.finalized_db.clone();
        let status = self.status.clone();
        let source = self.source.clone();
        let nfs_epoch_signal = self.nfs_epoch_signal.clone();
        let block_wake_signal = self.block_wake_signal.clone();
        let coherence = self.coherence.subscriber();
        let network = self.network.clone();
        let timings = self.sync_timings;
        let cancel_token = self.cancel_token.clone();

        tokio::task::spawn(async move {
            let status = status.clone();
            let source = source.clone();
            // Subscribe once to source-change notifications (mockchain
            // sources fire on `mine_blocks`; real validators return None
            // and the worker falls back to its interval timer).
            let mut change_rx = source.subscribe_to_blocks_received();
            let mut consecutive_failures: u32 = 0;
            let mut current_backoff = timings.initial_backoff;
            // Fires the mempool's block wake only when the height actually
            // moves. Firing every iteration would make the wake a second timer
            // rather than a block signal, and the mempool would poll at the sync
            // loop's cadence instead of its own.
            let mut last_woken_height: Option<u32> = None;
            #[cfg(feature = "prometheus")]
            let mut has_reached_tip = false;

            loop {
                let source = source.clone();
                let network = network.clone();
                if cancel_token.is_cancelled() {
                    return Ok(());
                }

                status.store(StatusType::Syncing);
                #[cfg(feature = "prometheus")]
                let iteration_start = std::time::Instant::now();

                // Race the iter body against cancellation: any await inside
                // — `source.get_best_block_height`, `fs.sync_to_height`,
                // `non_finalized_state.sync` — is a checkpoint that can
                // short-circuit to `Ok(())` when `cancel_token.cancel()`
                // fires. All in-flight ops drop cleanly (LMDB writes are
                // per-block atomic, ArcSwap CAS is single-tick, local
                // `Vec`s/`HashMap`s are scoped to the dropped future). Lets
                // tests drop the indexer without calling `shutdown()` and
                // still have the worker exit promptly via the `Drop` impl
                // below.
                let sync_result: Result<(), SyncError> = tokio::select! {
                    biased;
                    _ = cancel_token.cancelled() => return Ok(()),
                    r = async {
                    fn source_error(error: impl std::error::Error + Send + 'static) -> SyncError {
                        SyncError::ErrorFromSource(Box::new(error))
                    }

                    let chain_height = source
                        .clone()
                        .get_best_block_height()
                        .await
                        .map_err(source_error)?
                        .ok_or_else(|| {
                            source_error(std::io::Error::other(
                                "node returned no best block height",
                            ))
                        })?;
                    #[cfg(feature = "prometheus")]
                    metrics::gauge!("zaino.chain.tip_height").set(chain_height.0 as f64);
                    let finalised_height = finalized_height_floor(chain_height.0);
                    #[cfg(feature = "prometheus")]
                    {
                        metrics::gauge!(CHAIN_TIP_HEIGHT).set(chain_height.0 as f64);
                        metrics::gauge!(SYNC_LAG_BLOCKS)
                            .set((chain_height.0 - finalised_height.0) as f64);
                    }

                    fs.sync_to_height(finalised_height, &source)
                        .await
                        .map_err(source_error)?;

                    let intermediate_nfs_for_scoping = nfs.load();
                    let non_finalized_state = match *intermediate_nfs_for_scoping {
                        Some(ref nfs) => nfs,
                        None => {
                            // Anchor the non-finalised state at `finalised_height`
                            // (= chain tip − OPERATIONAL_NFS_DEPTH), never at genesis: a missing
                            // anchor used to fall through to genesis and then re-anchor up to the
                            // lagging finalised tip, grinding millions of blocks one at a time
                            // (#1261). `resolve_anchor_block` serves the anchor from the finalised
                            // DB / passthrough or builds it from the validator.
                            let anchor = NonFinalizedState::resolve_anchor_block(
                                &source,
                                &fs.to_reader(),
                                &network,
                                finalised_height,
                            )
                            .await?;
                            nfs.store(Some(Arc::new(
                                NonFinalizedState::initialize(source, network, Some(anchor))
                                    .await
                                    .map_err(source_error)?,
                            )));
                            &nfs.load_full().expect("just set to Some")
                        }
                    };

                    // Sync nfs to the iter-committed `chain_height`, trimming
                    // blocks to finalized tip. Passing `chain_height` rather
                    // than letting NFS extend until `get_block` returns None
                    // bounds the iter against mid-iter source advances (#1126).
                    non_finalized_state
                        .sync(fs.clone(), chain_height.into())
                        .await?;
                    std::mem::drop(intermediate_nfs_for_scoping);

                    // Unconditional: the epoch is bumped only on a real tip
                    // change (see `NonfinalizedBlockCacheSnapshot::generation`),
                    // so the coherence layer re-reads a cheap unchanged value on
                    // the quiet iterations and thaws on the block rather than on
                    // its next tick. Both signals are hints — a lost send costs
                    // latency, never correctness — so the result is discarded.
                    let _ = nfs_epoch_signal.send(());

                    if last_woken_height != Some(chain_height.0) {
                        last_woken_height = Some(chain_height.0);
                        let _ = block_wake_signal.send(());
                    }

                    // A freeze is normal and brief — it is how a tip transition
                    // is meant to look. A freeze that outlives
                    // `COHERENCE_FREEZE_ESCALATION` is not: the validator tip and
                    // Zaino's have stopped agreeing, and tip-coherent reads have
                    // been failing that whole time with nothing in the log to say
                    // so. Reported here rather than from the coherence layer
                    // because this loop is the thing that would have to fix it.
                    let frozen_for = coherence.frozen_for();
                    #[cfg(feature = "prometheus")]
                    metrics::gauge!(MEMPOOL_COHERENCE_FROZEN_SECONDS)
                        .set(frozen_for.map_or(0.0, |d| d.as_secs_f64()));
                    if let Some(frozen_for) = frozen_for {
                        if frozen_for >= COHERENCE_FREEZE_ESCALATION {
                            tracing::warn!(
                                frozen_for_secs = frozen_for.as_secs(),
                                "mempool coherence has been frozen far longer than a tip \
                                 transition should take; tip-coherent reads are unavailable"
                            );
                        }
                    }

                    Ok(())
                    } => r,
                };

                match sync_result {
                    Ok(()) => {
                        consecutive_failures = 0;
                        current_backoff = timings.initial_backoff;
                        status.store(StatusType::Ready);
                        #[cfg(feature = "prometheus")]
                        {
                            metrics::counter!(SYNC_ITERATIONS_TOTAL).increment(1);
                            metrics::histogram!(SYNC_ITERATION_DURATION_SECONDS)
                                .record(iteration_start.elapsed().as_secs_f64());
                            if !has_reached_tip {
                                has_reached_tip = true;
                                metrics::gauge!(SYNC_HAS_REACHED_TIP).set(1.0);
                                metrics::gauge!(SYNC_REACHED_TIP_AT).set(unix_now_secs());
                            }
                        }
                        // Race the post-success wait against cancellation
                        // and a source-change notification. `shutdown()`'s
                        // `cancel_token.cancel()` releases this immediately
                        // so the next top-of-loop check exits the worker;
                        // a source change wakes the worker before the full
                        // `timings.interval` elapses, so newly-mined
                        // blocks land in the next iter without waiting on
                        // the timer.
                        tokio::select! {
                            biased;
                            _ = cancel_token.cancelled() => return Ok(()),
                            _ = source::wait_or_source_change(
                                change_rx.as_mut(),
                                timings.interval,
                            ) => {}
                        }
                    }
                    Err(e) => {
                        consecutive_failures += 1;
                        #[cfg(feature = "prometheus")]
                        {
                            metrics::counter!(SYNC_ITERATIONS_TOTAL).increment(1);
                            metrics::histogram!(SYNC_ITERATION_DURATION_SECONDS)
                                .record(iteration_start.elapsed().as_secs_f64());
                        }
                        if consecutive_failures >= timings.max_consecutive_failures {
                            #[cfg(feature = "prometheus")]
                            metrics::counter!(SYNC_ERRORS_TOTAL, "severity" => "critical")
                                .increment(1);
                            tracing::error!(
                                consecutive_failures,
                                ?e,
                                "sync loop failed, giving up"
                            );
                            status.store(StatusType::CriticalError);
                            return Err(e);
                        }
                        tracing::warn!(
                            consecutive_failures,
                            max = timings.max_consecutive_failures,
                            backoff = ?current_backoff,
                            ?e,
                            "sync loop iteration failed, retrying"
                        );
                        status.store(StatusType::RecoverableError);
                        #[cfg(feature = "prometheus")]
                        metrics::counter!(SYNC_ERRORS_TOTAL, "severity" => "recoverable")
                            .increment(1);
                        // Race the failure-path backoff sleep against
                        // cancellation. Without this, `shutdown()` after
                        // `fs.shutdown()` would force the worker through
                        // the full ~40 s `max_consecutive_failures`
                        // backoff ladder before exiting (#1098).
                        tokio::select! {
                            biased;
                            _ = cancel_token.cancelled() => return Ok(()),
                            _ = tokio::time::sleep(current_backoff) => {}
                        }
                        current_backoff = (current_backoff * 2).min(timings.max_backoff);
                    }
                }
            }
        })
    }
}

impl<Source: BlockchainSource> Drop for NodeBackedChainIndex<Source> {
    /// Cooperative cancellation on drop: signals the sync worker (and any
    /// other futures racing against `cancel_token.cancelled()`) to exit
    /// promptly when the indexer goes out of scope.
    ///
    /// Tests that drop the indexer without calling [`Self::shutdown`] —
    /// which is most of them — used to rely on the harness sleeping in its
    /// post-iter poll long enough that the worker was parked at its sync
    /// loop's interval-sleep before runtime teardown raced a mid-iter LMDB
    /// write. With body-level cancellation in the worker (`tokio::select!`
    /// on `cancel_token.cancelled()` wrapping the iter body), the worker
    /// exits at its next await checkpoint instead.
    fn drop(&mut self) {
        // The full synchronous teardown, not just the cancellation: the
        // source release in particular must run here, or the `State`
        // connector's Zebra syncer task outlives an index that is dropped
        // without an explicit `shutdown()` call.
        self.shutdown_sync_best_effort();
    }
}

/// A clone-safe *read-only* view onto a running [`NodeBackedChainIndex`].
///
/// Designed for concurrent efficiency.
///
/// [`NodeBackedChainIndexSubscriber`] can safely be cloned and dropped freely.
#[derive(Clone, Debug)]
pub struct NodeBackedChainIndexSubscriber<
    Source: BlockchainSource = crate::chain_index::validator_source::ZebraValidatorSource,
> {
    /// The live set: answers regardless of what the tip is doing.
    mempool: zaino_mempool_service::MempoolSubscriber,
    /// The tip-coherent view over it, consulted by the reads that place a
    /// transaction relative to a tip.
    coherence: zaino_mempool_service::CoherentSubscriber,
    non_finalized_state: Arc<ArcSwapOption<crate::NonFinalizedState<Source>>>,
    finalized_state: finalised_state::reader::DbReader<Source>,
    status: NamedAtomicStatus,
    network: ZebraNetwork,
    source: Source,
}

async fn compact_block_from_source<Source: BlockchainSource>(
    source: &Source,
    network: ZebraNetwork,
    height: types::Height,
    pool_types: &PoolTypeFilter,
) -> Result<Option<zaino_proto::proto::compact_formats::CompactBlock>, ChainIndexError> {
    let Some(block) = source
        .get_block(HashOrHeight::Height(zebra_chain::block::Height(height.0)))
        .await
        .map_err(ChainIndexError::backing_validator)?
    else {
        return Ok(None);
    };

    let block_height = block
        .coinbase_height()
        .map(|height| types::Height(height.0))
        .ok_or_else(|| {
            ChainIndexError::backing_validator(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "validator returned a block without a height",
            ))
        })?;
    if block_height != height {
        return Err(ChainIndexError::backing_validator(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "validator returned block at height {}, expected {}",
                block_height.0, height.0
            ),
        )));
    }

    let tree_roots = source
        .get_commitment_tree_roots(types::BlockHash::from(block.hash()))
        .await
        .map_err(ChainIndexError::backing_validator)?;
    let (sapling_root, sapling_size, orchard_root, orchard_size, ironwood) =
        TreeRootData::new(tree_roots.0, tree_roots.1, tree_roots.2).extract_with_defaults();

    let metadata = BlockMetadata {
        sapling_root,
        sapling_size: sapling_size.try_into().map_err(|_| {
            ChainIndexError::backing_validator(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "sapling commitment tree size overflow",
            ))
        })?,
        orchard_root,
        orchard_size: orchard_size.try_into().map_err(|_| {
            ChainIndexError::backing_validator(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "orchard commitment tree size overflow",
            ))
        })?,
        ironwood: ironwood
            .map(|(root, size)| {
                Ok::<_, ChainIndexError>((
                    root,
                    size.try_into().map_err(|_| {
                        ChainIndexError::backing_validator(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "ironwood commitment tree size overflow",
                        ))
                    })?,
                ))
            })
            .transpose()?,
        // parent chainwork unknown — single-block construction
        parent_chainwork: None,
        network,
    };
    let indexed_block =
        IndexedBlock::try_from(BlockWithMetadata::new(&block, metadata)).map_err(|error| {
            ChainIndexError::backing_validator(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                error,
            ))
        })?;

    Ok(Some(prune_compact_block(
        indexed_block.to_compact_block(),
        pool_types,
    )))
}

impl<Source: BlockchainSource> NodeBackedChainIndexSubscriber<Source> {
    pub(crate) fn source(&self) -> &Source {
        &self.source
    }

    /// The indexer's mempool subscriber.
    ///
    /// Test-only escape hatch. Production code goes through the `ChainIndex`
    /// mempool API.
    #[cfg(feature = "test_dependencies")]
    pub(crate) fn mempool_subscriber(&self) -> &zaino_mempool_service::MempoolSubscriber {
        &self.mempool
    }

    /// How long tip-coherent mempool reads have been frozen, or `None` if live.
    ///
    /// A brief non-`None` value is the normal shape of a tip transition. A
    /// sustained one means the validator tip and Zaino's have stopped agreeing,
    /// and every tip-coherent read is failing — which is why this is exposed
    /// rather than left to the metric alone.
    pub fn mempool_coherence_health(&self) -> Option<std::time::Duration> {
        self.coherence.frozen_for()
    }

    /// Returns the combined status of all chain index components.
    pub fn combined_status(&self) -> StatusType {
        let finalized_status = self.finalized_state.status();
        let mempool_status = self.mempool.status();
        let combined_status = self
            .status
            .load()
            .combine(finalized_status)
            .combine(mempool_status);
        self.status.store(combined_status);
        combined_status
    }

    /// Returns the number of transparent outputs of `txid` that are currently unspent in the
    /// finalised state. Returns 0 if `txid` is not indexed by the finalised state.
    ///
    /// Used by `get_tx_out_set_info` to seed the per-transaction unspent counter for prev
    /// transactions first encountered as a non-finalised-state spend.
    async fn count_finalised_unspent_outputs(
        &self,
        txid: TransactionHash,
    ) -> Result<u64, ChainIndexError> {
        let Some(tx_location) = self
            .finalized_state
            .get_tx_location(&txid)
            .await
            .map_err(|e| ChainIndexError::internal(e.to_string()))?
        else {
            return Ok(0);
        };

        let Some(transparent) = self
            .finalized_state
            .get_transparent(tx_location)
            .await
            .map_err(|e| ChainIndexError::internal(e.to_string()))?
        else {
            return Ok(0);
        };

        // Skip unspendable outputs (matches `is_unspendable_tx_out` semantics used by the
        // accumulator). NonStandard outputs are never in the UTXO set, so they must not count
        // toward a transaction's "remaining unspent" tally.
        use crate::chain_index::types::db::metadata::is_unspendable_tx_out;
        let outpoints: Vec<Outpoint> = transparent
            .outputs()
            .iter()
            .enumerate()
            .filter(|(_, out)| !is_unspendable_tx_out(out))
            .map(|(i, _)| Outpoint::new(txid.0, i as u32))
            .collect();

        if outpoints.is_empty() {
            return Ok(0);
        }

        let spenders = self
            .finalized_state
            .get_outpoint_spenders(outpoints)
            .await
            .map_err(|e| ChainIndexError::internal(e.to_string()))?;

        Ok(spenders.into_iter().filter(|s| s.is_none()).count() as u64)
    }

    async fn get_fullblock_bytes_from_node(
        &self,
        id: HashOrHeight,
    ) -> Result<Option<Vec<u8>>, ChainIndexError> {
        self.source()
            .get_block(id)
            .await
            .map_err(ChainIndexError::backing_validator)?
            .map(|bk| {
                bk.zcash_serialize_to_vec()
                    .map_err(ChainIndexError::backing_validator)
            })
            .transpose()
    }

    async fn get_compact_block_from_node(
        &self,
        height: types::Height,
        pool_types: &PoolTypeFilter,
    ) -> Result<Option<zaino_proto::proto::compact_formats::CompactBlock>, ChainIndexError> {
        compact_block_from_source(self.source(), self.network.clone(), height, pool_types).await
    }

    async fn get_indexed_block_height(
        &self,
        snapshot: &NonfinalizedBlockCacheSnapshot,
        hash: types::BlockHash,
    ) -> Result<Option<types::Height>, ChainIndexError> {
        // ChainIndex step 2:
        match snapshot.blocks.get(&hash).cloned() {
            Some(block) => Ok(snapshot
                // ChainIndex step 3:
                .heights_to_hashes
                .values()
                .find(|h| **h == hash)
                // Canonical height is None for blocks not on the best chain
                .map(|_| block.context.index.height)),
            None => self
                // ChainIndex step 4:
                .finalized_state
                .get_block_height(hash)
                .await
                .map_err(|e| ChainIndexError::database_hole(hash, Some(Box::new(e)))),
        }
    }

    /**
    Searches finalized and non-finalized chains for any blocks containing the transaction.
    Ordered with finalized blocks first.

    WARNING: there might be multiple chains, each containing a block with the transaction.
    */
    async fn blocks_containing_transaction<'snapshot, 'self_lt, 'iter>(
        &'self_lt self,
        snapshot: &'snapshot NonfinalizedBlockCacheSnapshot,
        txid: [u8; 32],
    ) -> Result<impl Iterator<Item = IndexedBlock> + use<'iter, Source>, FinalisedStateError>
    where
        'snapshot: 'iter,
        'self_lt: 'iter,
    {
        let finalized_blocks_containing_transaction = match self
            .finalized_state
            .get_tx_location(&types::TransactionHash(txid))
            .await?
        {
            Some(tx_location) => {
                self.finalized_state
                    .get_chain_block_by_height(crate::Height(tx_location.block_height()))
                    .await?
            }

            None => None,
        }
        .into_iter();
        let non_finalized_blocks_containing_transaction =
            snapshot.blocks.values().filter_map(move |block| {
                block.transactions().iter().find_map(|transaction| {
                    if transaction.txid().0 == txid {
                        Some(block.clone())
                    } else {
                        None
                    }
                })
            });
        Ok(finalized_blocks_containing_transaction
            .chain(non_finalized_blocks_containing_transaction))
    }

    async fn get_block_height_passthrough(
        &self,
        max_serviceable_height: &types::Height,
        hash: types::BlockHash,
    ) -> Result<Option<types::Height>, ChainIndexError> {
        //ChainIndex step 5:
        match self
            .source()
            .get_block(HashOrHeight::Hash(hash.into()))
            .await
        {
            Ok(Some(block)) => {
                // At this point, we know that
                // the block is in the VALIDATOR.
                match block.coinbase_height() {
                    None => {
                        // the block is in the VALIDATOR. but doesnt have a height. That would imply a bug.
                        Err(ChainIndexError::validator_data_error_block_coinbase_height_missing())
                    }
                    Some(height) => {
                        // The VALIDATOR returned a block with a height.
                        // However, there is as of yet no guaranteed the Block is FINALIZED
                        if height <= *max_serviceable_height {
                            Ok(Some(types::Height::from(height)))
                        } else {
                            // non-finalized block
                            // no passthrough
                            Ok(None)
                        }
                    }
                }
            }
            Ok(None) => {
                // the block is neither in the INDEXER nor VALIDATOR
                Ok(None)
            }
            Err(e) => Err(ChainIndexError::backing_validator(e)),
        }
    }

    /// Returns true when the block hash is present in the local chain index.
    ///
    /// During finalized-state sync, a hash is considered known when it is in
    /// the finalized database or the backing validator can serve it as a
    /// finalized block.
    pub(crate) async fn block_hash_known_for_treestate(
        &self,
        snapshot: &ChainIndexSnapshot,
        hash: &types::BlockHash,
    ) -> Result<bool, ChainIndexError> {
        match snapshot {
            ChainIndexSnapshot::NonFinalizedStateExists {
                non_finalized_snapshot,
            } => {
                if non_finalized_snapshot.blocks.contains_key(hash) {
                    return Ok(true);
                }
                Ok(self
                    .finalized_state
                    .get_block_height(*hash)
                    .await?
                    .is_some())
            }
            ChainIndexSnapshot::StillSyncingFinalizedState {
                validator_finalized_height,
            } => {
                if self
                    .finalized_state
                    .get_block_height(*hash)
                    .await?
                    .is_some()
                {
                    return Ok(true);
                }
                Ok(self
                    .get_block_height_passthrough(validator_finalized_height, *hash)
                    .await?
                    .is_some())
            }
        }
    }

    /// Returns true when the hash-or-height string refers to a block known to
    /// the local chain index.
    pub(crate) async fn hash_or_height_known_for_treestate(
        &self,
        snapshot: &ChainIndexSnapshot,
        hash_or_height: &str,
    ) -> Result<bool, ChainIndexError> {
        let hash_or_height = HashOrHeight::from_str(hash_or_height).map_err(|error| {
            ChainIndexError::internal(format!("invalid hash or height: {error}"))
        })?;
        match hash_or_height {
            HashOrHeight::Hash(hash) => {
                self.block_hash_known_for_treestate(snapshot, &types::BlockHash::from(hash))
                    .await
            }
            HashOrHeight::Height(height) => {
                match self
                    .get_block_hash(snapshot, types::Height::from(height))
                    .await?
                {
                    Some(hash) => self.block_hash_known_for_treestate(snapshot, &hash).await,
                    None => Ok(false),
                }
            }
        }
    }
}

impl<Source: BlockchainSource> Status for NodeBackedChainIndexSubscriber<Source> {
    fn status(&self) -> StatusType {
        self.combined_status()
    }
}

impl<Source: BlockchainSource> ChainIndex for NodeBackedChainIndexSubscriber<Source> {
    type Snapshot = ChainIndexSnapshot;
    type Error = ChainIndexError;

    // ********** Utility methods **********

    /// Takes a snapshot of the non_finalized state. All NFS-interfacing query
    /// methods take a snapshot. The query will check the index
    /// it existed at the moment the snapshot was taken.
    async fn snapshot_nonfinalized_state(&self) -> Result<Self::Snapshot, Self::Error> {
        match self.non_finalized_state.load().as_ref() {
            Some(non_finalised_state) => Ok(ChainIndexSnapshot::NonFinalizedStateExists {
                non_finalized_snapshot: non_finalised_state.get_snapshot(),
            }),
            None => {
                let height = self
                    .source
                    .get_best_block_height()
                    .await
                    .map_err(ChainIndexError::backing_validator)?
                    .ok_or(ChainIndexError::database_hole(
                        "validator has no best block",
                        None,
                    ))?;
                let validator_finalized_height = finalized_height_floor(height.0);
                Ok(ChainIndexSnapshot::StillSyncingFinalizedState {
                    validator_finalized_height,
                })
            }
        }
    }

    // ********** Block methods **********

    /// Returns Some(Height) for the given block hash *if* it is currently in the best chain.
    ///
    /// Returns None if the specified block is not in the best chain or is not found.
    ///
    /// Used for hash based block lookup (random access).
    async fn get_block_height(
        &self,
        snapshot: &Self::Snapshot,
        hash: types::BlockHash,
    ) -> Result<Option<types::Height>, Self::Error> {
        // ChainIndex step 1: Skip
        // mempool blocks have no canon height
        // todo: possible efficiency boost by checking mempool for a negative?

        // ChainIndex steps 2-4:
        match snapshot {
            ChainIndexSnapshot::NonFinalizedStateExists {
                non_finalized_snapshot,
            } => {
                self.get_indexed_block_height(non_finalized_snapshot, hash)
                    .await
            }
            ChainIndexSnapshot::StillSyncingFinalizedState {
                validator_finalized_height,
            } => {
                self.get_block_height_passthrough(validator_finalized_height, hash)
                    .await
            } // ChainIndex step 5
        }
    }

    /// Returns Some(BlockHash) for the given block height.in the best chain.
    ///
    /// Returns None if the specified block height is above the best chain tip.
    async fn get_block_hash(
        &self,
        snapshot: &Self::Snapshot,
        height: types::Height,
    ) -> Result<Option<types::BlockHash>, Self::Error> {
        // First check non-finalised state.
        match snapshot {
            ChainIndexSnapshot::NonFinalizedStateExists {
                non_finalized_snapshot,
            } => match non_finalized_snapshot
                .heights_to_hashes
                .get(&height)
                .copied()
            {
                Some(block_hash) => Ok(Some(block_hash)),
                // If not found check finalised state.
                None => self
                    .finalized_state
                    .get_block_hash(height)
                    .await
                    .map_err(Into::into),
            },

            ChainIndexSnapshot::StillSyncingFinalizedState {
                validator_finalized_height,
            } => {
                if height <= *validator_finalized_height {
                    // If still syncing try to fetch from backing validator (*passthrough*).
                    //
                    // Note this requires fetching the full block from the backing node.
                    match self
                        .source()
                        .get_block(HashOrHeight::Height(height.into()))
                        .await
                        .map_err(ChainIndexError::backing_validator)?
                    {
                        Some(block) => Ok(Some(block.hash().into())),
                        None => Ok(None),
                    }
                } else {
                    // The requested block is non-finalized
                    // We can't safely serve it via passthrough
                    Ok(None)
                }
            }
        }
    }

    /// Returns Some(IndexedBlock) for the given block hash.
    ///
    /// Returns None if the specified block is not found.
    ///
    /// **NOTE: This Method is currently not "passthrough aware", cumulative
    /// chain work must be made optional to enable this.**
    async fn get_indexed_block_by_hash(
        &self,
        snapshot: &Self::Snapshot,
        target_hash: &types::BlockHash,
    ) -> Result<Option<IndexedBlock>, Self::Error> {
        match snapshot.get_chainblock_by_hash(target_hash) {
            Some(block) => Ok(Some(block.clone())),
            None => match self.get_block_height(snapshot, *target_hash).await {
                Ok(Some(height)) => Ok(self
                    .finalized_state
                    .get_chain_block_by_height(height)
                    .await?),
                Ok(None) => Ok(None),
                Err(e) => Err(e),
            },
        }
    }

    /// Returns Some(IndexedBlock) for the given block height.in the best chain.
    ///
    /// Returns None if the specified block height is above the best chain tip.
    ///
    /// **NOTE: This Method is currently not "passthrough aware", cumulative
    /// chain work must be made optional to enable this.**
    async fn get_indexed_block_by_height(
        &self,
        snapshot: &Self::Snapshot,
        target_height: &types::Height,
    ) -> Result<Option<IndexedBlock>, Self::Error> {
        match snapshot.get_chainblock_by_height(target_height) {
            Some(block) => Ok(Some(block.clone())),
            None => Ok(self
                .finalized_state
                .get_chain_block_by_height(*target_height)
                .await?),
        }
    }

    /// Given inclusive start and end heights, stream all blocks
    /// between the given heights.
    /// Returns None if the specified start height
    /// is greater than the snapshot's tip and greater
    /// than the validator's finalized height (`OPERATIONAL_NFS_DEPTH` blocks below tip)
    fn get_block_range(
        &self,
        snapshot: &Self::Snapshot,
        start: types::Height,
        end: std::option::Option<types::Height>,
    ) -> Option<impl Stream<Item = Result<Vec<u8>, Self::Error>>> {
        // ChainIndex step 1: Skip
        // mempool blocks have no canon height

        // The lower of the end of the provided range, and the highest block we can serve
        let end = end
            .unwrap_or(*snapshot.max_serviceable_height())
            .min(*snapshot.max_serviceable_height());
        // Serve as high as we can, or to the provided end if it's lower
        if start <= *snapshot.max_serviceable_height().min(&end) {
            Some(
                futures::stream::iter((start.0)..=(end.0)).then(move |height| async move {
                    // For blocks above validator_finalized_height, it's not reorg-safe to get blocks by height. It is reorg-safe to get blocks by hash. What we need to do in this case is use our snapshot index to look up the hash at a given height, and then get that hash from the validator.
                    // This is why we now look in the index.
                    match self
                        .finalized_state
                        .get_block_hash(types::Height(height))
                        .await
                    {
                        Ok(Some(hash)) => {
                            return self
                                .get_fullblock_bytes_from_node(HashOrHeight::Hash(hash.into()))
                                .await?
                                .ok_or(ChainIndexError::database_hole(hash, None))
                        }
                        Err(e) => Err(ChainIndexError {
                            kind: ChainIndexErrorKind::InternalServerError,
                            message: "".to_string(),
                            source: Some(Box::new(e)),
                        }),
                        Ok(None) => {
                            match snapshot.get_chainblock_by_height(&types::Height(height)) {
                                Some(block) => {
                                    return self
                                        .get_fullblock_bytes_from_node(HashOrHeight::Hash(
                                            (*block.hash()).into(),
                                        ))
                                        .await?
                                        .ok_or(ChainIndexError::database_hole(block.hash(), None))
                                }
                                None => self
                                    // usually getting by height is not reorg-safe, but here, height is known to be below or equal to validator_finalized_height.
                                    .get_fullblock_bytes_from_node(HashOrHeight::Height(
                                        zebra_chain::block::Height(height),
                                    ))
                                    .await?
                                    .ok_or(ChainIndexError::database_hole(height, None)),
                            }
                        }
                    }
                }),
            )
        } else {
            None
        }
    }

    // ********** Transaction methods **********

    /// given a transaction id, returns the transaction
    /// and the consensus branch ID for the block the transaction
    /// is in
    async fn get_raw_transaction(
        &self,
        snapshot: &Self::Snapshot,
        txid: &types::TransactionHash,
    ) -> Result<Option<(Vec<u8>, Option<u32>)>, Self::Error> {
        // ChainIndex step 1: the mempool, but only if it is coherent with the
        // snapshot this caller is reading against.
        //
        // An unmined transaction's consensus branch id is derived from the
        // height it *would* be mined at, so serving it against a stale snapshot
        // would attach the wrong branch id — and a caller cannot tell a wrong
        // branch id from a right one. `Unavailable` says "ask again with a fresh
        // snapshot", which is recoverable; the wrong answer is not.
        let coherent = self.coherence.coherent_snapshot();
        let coherent_here = snapshot
            .nfs_epoch()
            .filter(|epoch| coherent.is_valid_for_snapshot(*epoch));

        if let Some(epoch) = coherent_here {
            if let Some(entry) = coherent.get(&types_txid_to_domain(txid)) {
                // The branch id an unmined transaction would be validated under
                // is the one at the *next* height, derived from the caller's own
                // tip — not from wherever the mempool happens to be.
                let branch_id = ConsensusBranchId::current(
                    &self.network,
                    zebra_chain::block::Height(u32::from(epoch.best_tip.height) + 1),
                )
                .map(u32::from);

                return Ok(Some((entry.wire_bytes().to_vec(), branch_id)));
            }
        } else if self.mempool.contains_txid(&types_txid_to_domain(txid)) {
            // The transaction *is* in the mempool, but this caller's view of the
            // chain has moved on from the one the mempool is coherent with.
            return Err(ChainIndexError::unavailable(
                "mempool is not coherent with the requested snapshot; retry with a fresh snapshot",
            ));
        }

        let Some((transaction, location)) = self
            .source()
            .get_transaction(*txid)
            .await
            .map_err(ChainIndexError::backing_validator)?
        else {
            return Ok(None);
        };
        // as the reorg process cannot modify a transaction
        // it's safe to serve nonfinalized state directly here
        let height = match location {
            GetTransactionLocation::BestChain(height) => height,
            GetTransactionLocation::NonbestChain => {
                // if the tranasction isn't on the best chain
                // check our indexes. We need to find out the height from our index
                // to determine the consensus branch ID
                let Some(non_finalized_snapshot) = snapshot.get_nfs_snapshot() else {
                    // If we don't have a block containing the transaction
                    // locally and the transaction's not on the validator's
                    // best chain, we can't determine its consensus branch ID
                    return Ok(None);
                };

                match self
                    .blocks_containing_transaction(non_finalized_snapshot, txid.0)
                    .await?
                    .next()
                {
                    Some(block) => block.context.index.height.into(),
                    // As above Ok(None)
                    None => return Ok(None),
                }
            }
            // We've already checked the mempool. Should be unreachable?
            // todo: error here?
            GetTransactionLocation::Mempool => return Ok(None),
        };

        Ok(Some((
            zebra_chain::transaction::SerializedTransaction::from(transaction)
                .as_ref()
                .to_vec(),
            ConsensusBranchId::current(&self.network, height).map(u32::from),
        )))
    }

    /// Given a transaction ID, returns all known blocks containing this transaction
    ///
    /// If the transaction is in the mempool, it will be in the `BestChainLocation`
    /// if the mempool and snapshot are up-to-date, and the `NonBestChainLocation` set
    /// if the snapshot is out-of-date compared to the mempool
    async fn get_transaction_status(
        &self,
        snapshot: &Self::Snapshot,
        txid: &types::TransactionHash,
    ) -> Result<(Option<BestChainLocation>, HashSet<NonBestChainLocation>), ChainIndexError> {
        match snapshot {
            ChainIndexSnapshot::NonFinalizedStateExists {
                non_finalized_snapshot,
            } => {
                let blocks_containing_transaction = self
                    .blocks_containing_transaction(non_finalized_snapshot, txid.0)
                    .await?
                    .collect::<Vec<_>>();
                let Some(start_of_nonfinalized) =
                    non_finalized_snapshot.heights_to_hashes.keys().min()
                else {
                    return Err(ChainIndexError::database_hole("no blocks", None));
                };
                let mut best_chain_block = blocks_containing_transaction
                    .iter()
                    .find(|block| {
                        non_finalized_snapshot
                            .heights_to_hashes
                            .get(&block.height())
                            == Some(block.hash())
                            || block.height() < *start_of_nonfinalized
                        // this block is either in the best chain ``heights_to_hashes`` or finalized.
                    })
                    .map(|block| BestChainLocation::Block(*block.hash(), block.height()));
                let mut non_best_chain_blocks: HashSet<NonBestChainLocation> =
                    blocks_containing_transaction
                        .iter()
                        .filter(|block| {
                            non_finalized_snapshot
                                .heights_to_hashes
                                .get(&block.height())
                                != Some(block.hash())
                                && block.height() >= *start_of_nonfinalized
                        })
                        .map(|block| NonBestChainLocation::Block(*block.hash(), block.height()))
                        .collect();
                let domain_txid = types_txid_to_domain(txid);
                let in_mempool = self.mempool.contains_txid(&domain_txid);
                if in_mempool {
                    let coherent = self.coherence.coherent_snapshot();
                    if coherent.is_valid_for_snapshot(non_finalized_snapshot.epoch()) {
                        if best_chain_block.is_some() {
                            return Err(ChainIndexError {
                        kind: ChainIndexErrorKind::InvalidSnapshot,
                        message:
                            "Best chain and up-to-date mempool both contain the same transaction"
                                .to_string(),
                        source: None,
                    });
                        } else {
                            best_chain_block = Some(BestChainLocation::Mempool(
                                non_finalized_snapshot.best_tip.height + 1,
                            ));
                        }
                    } else {
                        // the best chain and the mempool have divergent tip hashes
                        // get a new snapshot and use it to find the height of the mempool
                        if let ChainIndexSnapshot::NonFinalizedStateExists {
                            non_finalized_snapshot: new_snapshot,
                        } = self.snapshot_nonfinalized_state().await?
                        {
                            // The mempool is coherent with some *other* tip than
                            // this caller's. Report the height it would be mined
                            // at under that tip, if we still hold that block.
                            let target_height = self
                                .coherence
                                .coherent_snapshot()
                                .valid_for
                                .and_then(|epoch| {
                                    new_snapshot.blocks.iter().find_map(|(hash, block)| {
                                        (<[u8; 32]>::from(*hash)
                                            == <[u8; 32]>::from(epoch.best_tip.hash))
                                        .then(|| block.height() + 1)
                                    })
                                });
                            non_best_chain_blocks
                                .insert(NonBestChainLocation::Mempool(target_height));
                        }
                    }
                }
                Ok((best_chain_block, non_best_chain_blocks))
            }
            ChainIndexSnapshot::StillSyncingFinalizedState {
                validator_finalized_height,
            } => {
                if let Some((_transaction, GetTransactionLocation::BestChain(height))) = self
                    .source()
                    .get_transaction(*txid)
                    .await
                    .map_err(ChainIndexError::backing_validator)?
                {
                    if height <= *validator_finalized_height {
                        if let Some(block) = self
                            .source()
                            .get_block(HashOrHeight::Height(height))
                            .await
                            .map_err(ChainIndexError::backing_validator)?
                        {
                            return Ok((
                                Some(BestChainLocation::Block(block.hash().into(), height.into())),
                                HashSet::new(),
                            ));
                        }
                    }
                }
                Ok((None, HashSet::new()))
            }
        }
    }

    /// Returns all txids currently in the mempool.
    async fn get_mempool_txids(&self) -> Result<Vec<types::TransactionHash>, Self::Error> {
        // The tip-agnostic set: this read is a listing, not a placement, so it
        // stays live across a tip transition.
        Ok(self
            .mempool
            .get_txids()
            .iter()
            .map(|txid| types::TransactionHash::from(<[u8; 32]>::from(*txid)))
            .collect())
    }

    /// Returns all transactions currently in the mempool, filtered by `exclude_list`.
    ///
    /// The `exclude_list` may contain shortened transaction ID hex prefixes (client-endian).
    /// The transaction IDs in the Exclude list can be shortened to any number of bytes to make the request
    /// more bandwidth-efficient; if two or more transactions in the mempool
    /// match a shortened txid, they are all sent (none is excluded). Transactions
    /// in the exclude list that don't exist in the mempool are ignored.
    async fn get_mempool_transactions(
        &self,
        exclude_list: Vec<Vec<u8>>,
    ) -> Result<Vec<std::sync::Arc<zaino_mempool::MempoolEntry>>, Self::Error> {
        // Validated rather than clamped: an over-long list or an unusably short
        // suffix is a caller mistake, and silently truncating it would serve
        // transactions the caller believes it excluded.
        let suffixes = self
            .mempool
            .validate_exclude_suffixes(&exclude_list)
            .map_err(|e| ChainIndexError::invalid_argument(e.to_string()))?;

        Ok(self.mempool.get_filtered_entries(&suffixes))
    }

    /// Returns a stream of mempool transactions, ending the stream when the chain tip block hash
    /// changes (a new block is mined or a reorg occurs).
    ///
    /// If a snapshot is given and the chain tip has changed from the given spanshot, returns None.
    fn get_mempool_stream(
        &self,
        snapshot: Option<&Self::Snapshot>,
    ) -> Option<impl futures::Stream<Item = Result<bytes::Bytes, Self::Error>>> {
        let expected_epoch = match snapshot {
            // Still syncing the finalized state means the chain tip is ahead of
            // this snapshot; there is no epoch to be coherent with.
            Some(ChainIndexSnapshot::StillSyncingFinalizedState { .. }) => return None,
            Some(snapshot) => snapshot.nfs_epoch(),
            None => None,
        };

        // One ready-made loop rather than the mpsc relay this used to run: the
        // coherence layer already owns "stream until the tip moves", including
        // the `Lagged` signal, so relaying it here only added a task, a channel,
        // and a place for the two notions of "ended" to drift apart.
        let stream = self
            .coherence
            .stream_transactions_until_tip_change(expected_epoch)?;

        // Qualified: this module has more than one `map` in scope on a stream.
        Some(futures::StreamExt::map(stream, |item| {
            item.map_err(|e| {
                // A lag is not a normal end. Reporting it as one would have the
                // client believe it had received the whole mempool.
                ChainIndexError::unavailable(format!("mempool stream ended early: {e}"))
            })
        }))
    }

    // ********** Chain methods **********

    /// For a given block,
    /// find its newest main-chain ancestor,
    /// or the block itself if it is on the main-chain.
    /// Returns Ok(None) if no fork point found. This is not an error,
    /// as zaino does not guarentee knowledge of all sidechain data.
    async fn find_fork_point(
        &self,
        snapshot: &Self::Snapshot,
        hash: &types::BlockHash,
    ) -> Result<Option<(types::BlockHash, types::Height)>, Self::Error> {
        // ChainIndex step 1: Skip
        // mempool blocks have no canon height, guaranteed to return None
        // todo: possible efficiency boost by checking mempool for a negative?

        match snapshot {
            ChainIndexSnapshot::NonFinalizedStateExists {
                non_finalized_snapshot,
            } => {
                match non_finalized_snapshot.get_chainblock_by_hash(hash) {
                    Some(block) => {
                        // At this point, we know that
                        // The block is non-FINALIZED in the INDEXER
                        // ChainIndex step 3:
                        if non_finalized_snapshot
                            .heights_to_hashes
                            .get(&block.height())
                            == Some(block.hash())
                        {
                            // The block is in the best chain.
                            Ok(Some((*block.hash(), block.height())))
                        } else {
                            // Otherwise, it's non-best chain! Grab its parent, and recurse
                            Box::pin(self.find_fork_point(snapshot, &block.context.parent_hash))
                                .await
                            // gotta pin recursive async functions to prevent infinite-sized
                            // Future-implementing types
                        }
                    }
                    None => {
                        // At this point, we know that
                        // the block is NOT non-FINALIZED in the INDEXER.
                        // as the non finalzed state is known to be populated,
                        // we now check the finalized state
                        match self.finalized_state.get_block_height(*hash).await {
                            Ok(Some(height)) => {
                                // the block is FINALIZED in the INDEXER
                                Ok(Some((*hash, height)))
                            }
                            Err(e) => Err(ChainIndexError::database_hole(hash, Some(Box::new(e)))),
                            Ok(None) => Ok(None),
                        }
                    }
                }
            }
            ChainIndexSnapshot::StillSyncingFinalizedState {
                validator_finalized_height,
            } => {
                // We're not fully synced, so we pass through.
                // Now, we ask the VALIDATOR.
                // ChainIndex step 5
                match self
                    .source()
                    .get_block(HashOrHeight::Hash(zebra_chain::block::Hash::from(*hash)))
                    .await
                {
                    Ok(Some(block)) => {
                        // At this point, we know that
                        // the block is in the VALIDATOR.
                        match block.coinbase_height() {
                            None => {
                                // the block is in the VALIDATOR. but doesnt have a height. That would imply a bug.
                                Err(ChainIndexError::validator_data_error_block_coinbase_height_missing())
                            }
                            Some(height) => {
                                // The VALIDATOR returned a block with a height.
                                // However, there is as of yet no guaranteed the Block is FINALIZED
                                if height <= *validator_finalized_height {
                                    Ok(Some((
                                        types::BlockHash::from(block.hash()),
                                        types::Height::from(height),
                                    )))
                                } else {
                                    // non-finalized block
                                    // no passthrough
                                    Ok(None)
                                }
                            }
                        }
                    }

                    Ok(None) => {
                        // At this point, we know that
                        // the block is NOT FINALIZED in the VALIDATOR.
                        // Return Ok(None) = no block found.
                        Ok(None)
                    }
                    Err(e) => Err(ChainIndexError::backing_validator(e)),
                }
            }
        }
    }

    /// Returns the block commitment tree data by hash.
    async fn get_treestate(
        &self,
        hash: &types::BlockHash,
    ) -> Result<
        (
            Option<source::PoolTreestate>,
            Option<source::PoolTreestate>,
            Option<source::PoolTreestate>,
        ),
        Self::Error,
    > {
        let snapshot = self.snapshot_nonfinalized_state().await?;
        if !self.block_hash_known_for_treestate(&snapshot, hash).await? {
            return Err(ChainIndexError::internal(format!(
                "block hash {hash} not found in local chain index"
            )));
        }

        match self.source().get_treestate(*hash).await {
            Ok(resp) => Ok(resp),
            Err(e) => Err(ChainIndexError {
                kind: ChainIndexErrorKind::InternalServerError,
                message: "failed to fetch treestate from validator".to_string(),
                source: Some(Box::new(e)),
            }),
        }
    }

    /// Gets the subtree roots of a given pool and the end heights of each root,
    /// starting at the provided index, up to an optional maximum number of roots.
    async fn get_subtree_roots(
        &self,
        pool: ShieldedPool,
        start_index: u16,
        max_entries: Option<u16>,
    ) -> Result<Vec<([u8; 32], u32)>, Self::Error> {
        self.source()
            .get_subtree_roots(pool, start_index, max_entries)
            .await
            .map_err(ChainIndexError::backing_validator)
    }

    // ********** Transparent address history methods **********

    /// Returns the total transparent balance for the given addresses.
    async fn get_address_balance(
        &self,
        address_strings: GetAddressBalanceRequest,
    ) -> Result<zaino_primitives::types::AddressBalance, Self::Error> {
        self.source()
            .get_address_balance(address_strings)
            .await
            .map_err(ChainIndexError::backing_validator)
    }

    /// Returns the transaction ids made by the given transparent addresses.
    async fn get_address_txids(
        &self,
        request: GetAddressTxIdsRequest,
    ) -> Result<Vec<types::TransactionHash>, Self::Error> {
        self.source()
            .get_address_txids(request)
            .await
            .map_err(ChainIndexError::backing_validator)
    }

    /// Returns all unspent transparent outputs for the given addresses.
    async fn get_address_utxos(
        &self,
        address_strings: GetAddressBalanceRequest,
    ) -> Result<Vec<zaino_primitives::types::Utxo>, Self::Error> {
        self.source()
            .get_address_utxos(address_strings)
            .await
            .map_err(ChainIndexError::backing_validator)
    }

    async fn get_outpoint_spenders(
        &self,
        snapshot: &Self::Snapshot,
        outpoints: Vec<types::Outpoint>,
        scope: types::ChainScope,
    ) -> Result<Vec<Option<types::TransactionHash>>, Self::Error> {
        use std::collections::HashMap;

        let mut result: Vec<Option<TransactionHash>> = vec![None; outpoints.len()];

        // 1) Non-finalised best chain (FullChain scope only). Scan only the blocks reachable
        //    via `heights_to_hashes` (the `blocks` map also holds reorged-away blocks, which
        //    must not count). One pass builds an outpoint -> spending-txid map regardless of
        //    how many outpoints we look up. Under `Finalised` scope this is skipped so results
        //    are reorg-stable.
        if let (
            types::ChainScope::FullChain,
            ChainIndexSnapshot::NonFinalizedStateExists {
                non_finalized_snapshot,
            },
        ) = (scope, snapshot)
        {
            let mut nfs_spenders: HashMap<Outpoint, TransactionHash> = HashMap::new();
            for hash in non_finalized_snapshot.heights_to_hashes.values() {
                let Some(block) = non_finalized_snapshot.blocks.get(hash) else {
                    continue;
                };
                for tx in block.transactions() {
                    let txid = *tx.txid();
                    // `spent_outpoints` already skips coinbase null prevouts and builds each
                    // `Outpoint`, keeping the construction in one place (see #1332).
                    for outpoint in tx.transparent().spent_outpoints() {
                        nfs_spenders.insert(outpoint, txid);
                    }
                }
            }
            for (i, outpoint) in outpoints.iter().enumerate() {
                if let Some(txid) = nfs_spenders.get(outpoint) {
                    result[i] = Some(*txid);
                }
            }
        }

        // 2) Finalised lookup for the still-unresolved outpoints, batched into one DB call.
        let unresolved_indices: Vec<usize> = result
            .iter()
            .enumerate()
            .filter_map(|(i, r)| r.is_none().then_some(i))
            .collect();
        if unresolved_indices.is_empty() {
            return Ok(result);
        }
        let unresolved_outpoints: Vec<Outpoint> =
            unresolved_indices.iter().map(|&i| outpoints[i]).collect();
        let locations = self
            .finalized_state
            .get_outpoint_spenders(unresolved_outpoints)
            .await?;

        // 3) Resolve each finalised `TxLocation` to a txid. Dedup identical locations so a
        //    block spending several queried outpoints is only fetched once. `get_txid` is a
        //    single keyed lookup, far cheaper than reconstructing the whole block.
        let mut slots_by_location: HashMap<types::TxLocation, Vec<usize>> = HashMap::new();
        for (slot, location) in unresolved_indices.into_iter().zip(locations) {
            if let Some(location) = location {
                slots_by_location.entry(location).or_default().push(slot);
            }
        }
        for (location, slots) in slots_by_location {
            let txid = self.finalized_state.get_txid(location).await?;
            for slot in slots {
                result[slot] = Some(txid);
            }
        }

        Ok(result)
    }

    // ********** Metadata methods **********

    async fn best_chaintip(&self, snapshot: &Self::Snapshot) -> Result<BlockIndex, Self::Error> {
        Ok(match snapshot {
            ChainIndexSnapshot::NonFinalizedStateExists {
                non_finalized_snapshot,
            } => non_finalized_snapshot.best_tip,

            ChainIndexSnapshot::StillSyncingFinalizedState {
                validator_finalized_height,
            } => {
                BlockIndex {
                    height: *validator_finalized_height,
                    hash: self
                        .source()
                        // TODO: do something more efficient than getting the whole block
                        .get_block(HashOrHeight::Height((*validator_finalized_height).into()))
                        .await
                        .map_err(|e| {
                            ChainIndexError::database_hole(
                                validator_finalized_height,
                                Some(Box::new(e)),
                            )
                        })?
                        .ok_or(ChainIndexError::database_hole(
                            validator_finalized_height,
                            None,
                        ))?
                        .hash()
                        .into(),
                }
            }
        })
    }
}

impl<Source: BlockchainSource> ChainIndexRpcExt for NodeBackedChainIndexSubscriber<Source> {
    /// Returns the *compact* block for the given height.
    ///
    /// Returns `None` if the specified `height` is greater than the snapshot's tip.
    ///
    /// ## Pool filtering
    ///
    /// - `pool_types` controls which per-transaction components are populated.
    /// - Transactions that contain no elements in any requested pool are omitted from `vtx`.
    ///   The original transaction index is preserved in `CompactTx.index`.
    /// - `PoolTypeFilter::default()` preserves the legacy behaviour (only Sapling and Orchard
    ///   components are populated).
    ///
    /// Returns None if the specified height
    /// is greater than the snapshot's tip
    ///
    /// **NOTE: This Method is currently not "passthrough aware", this should be added by
    /// fetching block data from the backing validator when not locally available.**
    async fn get_compact_block(
        &self,
        snapshot: &Self::Snapshot,
        height: types::Height,
        pool_types: PoolTypeFilter,
    ) -> Result<Option<zaino_proto::proto::compact_formats::CompactBlock>, Self::Error> {
        match snapshot {
            ChainIndexSnapshot::NonFinalizedStateExists {
                non_finalized_snapshot,
            } => {
                if height <= non_finalized_snapshot.best_tip.height {
                    Ok(Some(match snapshot.get_chainblock_by_height(&height) {
                        Some(block) => prune_compact_block(block.to_compact_block(), &pool_types),
                        None => {
                            match self
                                .finalized_state
                                .get_compact_block(height, pool_types.clone())
                                .await
                            {
                                Ok(block) => block,
                                Err(_) => self
                                    .get_compact_block_from_node(height, &pool_types)
                                    .await?
                                    .ok_or(ChainIndexError::database_hole(height, None))?,
                            }
                        }
                    }))
                } else {
                    Ok(None)
                }
            }

            ChainIndexSnapshot::StillSyncingFinalizedState {
                validator_finalized_height: _,
                //TODO: Once we make chainwork an option field we should be able to
                // support passthrougth for this
            } => Ok(None),
        }
    }

    /// Streams *compact* blocks for an inclusive height range.
    ///
    /// Returns `Ok(None)` if the request is descending and `start_height` exceeds the chain tip.
    /// For ascending requests that exceed the tip, returns a stream that ends with an
    /// `out_of_range` error after all available blocks have been sent.
    ///
    /// - The stream covers `[start_height, end_height]` (inclusive).
    /// - If `start_height <= end_height` the stream is ascending; otherwise it is descending.
    ///
    /// ## Pool filtering
    ///
    /// - `pool_types` controls which per-transaction components are populated.
    /// - Transactions that contain no elements in any requested pool are omitted from `vtx`.
    ///   The original transaction index is preserved in `CompactTx.index`.
    /// - `PoolTypeFilter::default()` preserves the legacy behaviour (only Sapling and Orchard
    ///   components are populated).
    ///
    /// **NOTE: This Method is currently not "passthrough aware", this should be added by
    /// fetching block data from the backing validator when not locally available.**
    #[allow(clippy::type_complexity)]
    async fn get_compact_block_stream(
        &self,
        nonfinalized_snapshot: &Self::Snapshot,
        start_height: types::Height,
        end_height: types::Height,
        pool_types: PoolTypeFilter,
    ) -> Result<Option<CompactBlockStream>, Self::Error> {
        let chain_tip_height = self.best_chaintip(nonfinalized_snapshot).await?.height;

        // The finalised state serves heights up to `finalized_height_floor(tip)`
        // (= `tip - OPERATIONAL_NFS_DEPTH`, saturating at genesis); the non-finalised cache serves
        // everything above it, so the lowest non-finalised height is one past the finalised floor.
        let lowest_nonfinalized_height =
            types::Height(finalized_height_floor(chain_tip_height.0).0 + 1);

        let is_ascending = start_height <= end_height;

        // Descending: the first block we'd try to return is already above the tip → error immediately.
        if !is_ascending && start_height > chain_tip_height {
            return Ok(None);
        }

        // For ascending requests that extend past the tip: cap the streaming range at the tip,
        // then append a trailing out_of_range error after all valid blocks have been sent.
        let needs_out_of_range = is_ascending && end_height > chain_tip_height;
        let capped_end_height = if needs_out_of_range {
            chain_tip_height
        } else {
            end_height
        };

        // Pre-create any finalized-state stream(s) we will need so that errors are returned
        // from this method (not deferred into the spawned task).
        let finalized_stream: Option<CompactBlockStream> = if is_ascending {
            if start_height < lowest_nonfinalized_height {
                let finalized_end_height = types::Height(std::cmp::min(
                    capped_end_height.0,
                    lowest_nonfinalized_height.0.saturating_sub(1),
                ));

                if start_height <= finalized_end_height {
                    Some(
                        self.finalized_state
                            .get_compact_block_stream(
                                start_height,
                                finalized_end_height,
                                pool_types.clone(),
                            )
                            .await
                            .map_err(ChainIndexError::from)?,
                    )
                } else {
                    None
                }
            } else {
                None
            }
        // Serve in reverse order.
        } else if end_height < lowest_nonfinalized_height {
            let finalized_start_height = if start_height < lowest_nonfinalized_height {
                start_height
            } else {
                types::Height(lowest_nonfinalized_height.0.saturating_sub(1))
            };

            Some(
                self.finalized_state
                    .get_compact_block_stream(
                        finalized_start_height,
                        end_height,
                        pool_types.clone(),
                    )
                    .await
                    .map_err(ChainIndexError::from)?,
            )
        } else {
            None
        };

        let nonfinalized_snapshot = nonfinalized_snapshot.clone();
        let source = self.source.clone();
        let network = self.network.clone();
        let pool_types_for_node = pool_types.clone();
        // TODO: Investigate whether channel size should be changed, added to config, or set dynamically based on resources.
        let (channel_sender, channel_receiver) = tokio::sync::mpsc::channel(128);

        tokio::spawn(async move {
            if is_ascending {
                // 1) Finalized segment (if any), ascending.
                if let Some(mut finalized_stream) = finalized_stream {
                    while let Some(stream_item) = finalized_stream.next().await {
                        if channel_sender.send(stream_item).await.is_err() {
                            return;
                        }
                    }
                }

                // 2) Nonfinalized segment, ascending.
                let nonfinalized_start_height =
                    types::Height(std::cmp::max(start_height.0, lowest_nonfinalized_height.0));

                for height_value in nonfinalized_start_height.0..=capped_end_height.0 {
                    let Some(indexed_block) = nonfinalized_snapshot
                        .get_chainblock_by_height(&types::Height(height_value))
                    else {
                        match compact_block_from_source(
                            &source,
                            network.clone(),
                            types::Height(height_value),
                            &pool_types_for_node,
                        )
                        .await
                        {
                            Ok(Some(compact_block)) => {
                                if channel_sender.send(Ok(compact_block)).await.is_err() {
                                    return;
                                }
                                continue;
                            }
                            Ok(None) => {
                                let _ = channel_sender
                                    .send(Err(tonic::Status::internal(format!(
                                        "Internal error, missing nonfinalized block at height [{height_value}].",
                                    ))))
                                    .await;
                                return;
                            }
                            Err(error) => {
                                let _ = channel_sender
                                    .send(Err(tonic::Status::internal(error.to_string())))
                                    .await;
                                return;
                            }
                        }
                    };
                    let compact_block =
                        prune_compact_block(indexed_block.to_compact_block(), &pool_types);
                    if channel_sender.send(Ok(compact_block)).await.is_err() {
                        return;
                    }
                }
                // If the original end_height was above the tip, signal out_of_range after all valid blocks.
                if needs_out_of_range {
                    let _ = channel_sender
                          .send(Err(tonic::Status::out_of_range(format!(
                              "Error: Height out of range [{}]. Height requested is greater than the best chain tip [{}].",
                              end_height.0, chain_tip_height.0,
                          ))))
                          .await;
                }
            } else {
                // 1) Nonfinalized segment, descending.
                if start_height >= lowest_nonfinalized_height {
                    let nonfinalized_end_height =
                        types::Height(std::cmp::max(end_height.0, lowest_nonfinalized_height.0));

                    for height_value in (nonfinalized_end_height.0..=start_height.0).rev() {
                        let Some(indexed_block) = nonfinalized_snapshot
                            .get_chainblock_by_height(&types::Height(height_value))
                        else {
                            match compact_block_from_source(
                                &source,
                                network.clone(),
                                types::Height(height_value),
                                &pool_types_for_node,
                            )
                            .await
                            {
                                Ok(Some(compact_block)) => {
                                    if channel_sender.send(Ok(compact_block)).await.is_err() {
                                        return;
                                    }
                                    continue;
                                }
                                Ok(None) => {
                                    let _ = channel_sender
                                        .send(Err(tonic::Status::internal(format!(
                                            "Internal error, missing nonfinalized block at height [{height_value}].",
                                        ))))
                                        .await;
                                    return;
                                }
                                Err(error) => {
                                    let _ = channel_sender
                                        .send(Err(tonic::Status::internal(error.to_string())))
                                        .await;
                                    return;
                                }
                            }
                        };
                        let compact_block =
                            prune_compact_block(indexed_block.to_compact_block(), &pool_types);
                        if channel_sender.send(Ok(compact_block)).await.is_err() {
                            return;
                        }
                    }
                }

                // 2) Finalized segment (if any), descending.
                if let Some(mut finalized_stream) = finalized_stream {
                    while let Some(stream_item) = finalized_stream.next().await {
                        if channel_sender.send(stream_item).await.is_err() {
                            return;
                        }
                    }
                }
            }
        });

        Ok(Some(CompactBlockStream::new(channel_receiver)))
    }

    async fn z_get_block(
        &self,
        hash_or_height: String,
        verbosity: Option<u8>,
    ) -> Result<GetBlock, Self::Error> {
        // Resolve tip-relative negative heights against the best chaintip,
        // matching zebra's own `getblock` semantics (`-1` is the tip). A
        // rejected identifier carries zcashd's legacy InvalidParameter code
        // as a typed `RpcError` source, which the serve layer recovers by
        // downcast-walking the error chain.
        let snapshot = self.snapshot_nonfinalized_state().await?;
        let tip = self.best_chaintip(&snapshot).await?;
        let id = HashOrHeight::new(&hash_or_height, Some(tip.height.into())).map_err(|error| {
            ChainIndexError::internal_from(crate::error::LegacyRpcError::new(
                zebra_rpc::server::error::LegacyCode::InvalidParameter,
                error,
            ))
        })?;
        self.source()
            .get_block_verbose(id, verbosity)
            .await
            .map_err(ChainIndexError::backing_validator)
    }

    async fn get_block_header(&self, hash: String) -> Result<BlockHeaderVerbose, Self::Error> {
        self.source()
            .get_block_header(hash)
            .await
            .map_err(ChainIndexError::backing_validator)
    }

    async fn get_raw_block_header(&self, hash: String) -> Result<Vec<u8>, Self::Error> {
        self.source()
            .get_raw_block_header(hash)
            .await
            .map_err(ChainIndexError::backing_validator)
    }

    // TODO(internal-first): `getblockdeltas` is buildable from the indexed chainblocks
    // + finalised/non-finalised prevout resolution. Build it internally by default once
    // an internal prevout resolver (spanning non-finalised + finalised, reconstructing
    // addresses from `TxOutCompact`) exists, keeping this source call as the fallback.
    async fn get_block_deltas(&self, hash: String) -> Result<BlockDeltas, Self::Error> {
        self.source()
            .get_block_deltas(hash)
            .await
            .map_err(ChainIndexError::backing_validator)
    }

    // `getdifficulty` is the difficulty-adjusted expected difficulty (over a block
    // window), not the tip block's stored bits, so it cannot be built from indexed data:
    // always delegate to the backing validator.
    async fn get_difficulty(&self) -> Result<f64, Self::Error> {
        self.source()
            .get_difficulty()
            .await
            .map_err(ChainIndexError::backing_validator)
    }

    async fn get_info(&self) -> Result<NodeInfo, Self::Error> {
        self.source()
            .get_info()
            .await
            .map_err(ChainIndexError::backing_validator)
    }

    // `getblockchaininfo` needs cumulative pool value balances (TipPoolValues) and on-disk
    // size, which are not in the ChainIndex's indexed data, so it cannot be built
    // internally: always delegate to the backing validator.
    async fn get_blockchain_info(
        &self,
    ) -> Result<zaino_primitives::types::BlockchainInfo, Self::Error> {
        self.source()
            .get_blockchain_info()
            .await
            .map_err(ChainIndexError::backing_validator)
    }

    async fn get_peer_info(&self) -> Result<Vec<PeerInfo>, Self::Error> {
        self.source()
            .get_peer_info()
            .await
            .map_err(ChainIndexError::backing_validator)
    }

    async fn get_block_subsidy(&self, height: u32) -> Result<BlockSubsidy, Self::Error> {
        self.source()
            .get_block_subsidy(height)
            .await
            .map_err(ChainIndexError::backing_validator)
    }

    async fn get_mining_info(&self) -> Result<MiningInfo, Self::Error> {
        self.source()
            .get_mining_info()
            .await
            .map_err(ChainIndexError::backing_validator)
    }

    async fn get_tx_out(
        &self,
        txid: String,
        n: u32,
        include_mempool: Option<bool>,
    ) -> Result<Option<zaino_primitives::types::rpc::TxOut>, Self::Error> {
        self.source()
            .get_tx_out(txid, n, include_mempool)
            .await
            .map_err(ChainIndexError::backing_validator)
    }

    async fn get_spent_info(
        &self,
        outpoint: zaino_primitives::types::rpc::SpentOutpoint,
    ) -> Result<zaino_primitives::types::rpc::SpentInfo, Self::Error> {
        self.source()
            .get_spent_info(outpoint)
            .await
            .map_err(ChainIndexError::backing_validator)
    }

    async fn get_network_sol_ps(
        &self,
        blocks: Option<i32>,
        height: Option<i32>,
    ) -> Result<u64, Self::Error> {
        self.source()
            .get_network_sol_ps(blocks, height)
            .await
            .map_err(ChainIndexError::backing_validator)
    }

    async fn send_raw_transaction(
        &self,
        raw_transaction_hex: String,
    ) -> Result<zaino_primitives::types::TransactionId, Self::Error> {
        // A local rejection, before the validator round trip. It carries
        // zcashd's `InvalidParameter` so the client sees the same code it
        // would have got from the validator, rather than a generic internal
        // error — the serving layer recovers it by downcasting the source
        // chain.
        validate_raw_transaction_hex(&raw_transaction_hex).map_err(|error| {
            ChainIndexError::internal_from(crate::error::LegacyRpcError::new(
                zebra_rpc::server::error::LegacyCode::InvalidParameter,
                error.to_string(),
            ))
        })?;
        self.source()
            .send_raw_transaction(raw_transaction_hex)
            .await
            .map_err(ChainIndexError::backing_validator)
    }

    async fn get_treestate_by_id(
        &self,
        hash_or_height: String,
    ) -> Result<zaino_primitives::types::Treestate, Self::Error> {
        self.source()
            .get_treestate_by_id(hash_or_height)
            .await
            .map_err(ChainIndexError::backing_validator)
    }

    /// Returns all changes for the given transparent addresses.
    async fn get_address_deltas(
        &self,
        params: AddressDeltasRequest,
    ) -> Result<AddressDeltas, Self::Error> {
        self.source()
            .get_address_deltas(params)
            .await
            .map_err(ChainIndexError::backing_validator)
    }

    /// Returns Information about the mempool state:
    /// - size: Current tx count
    /// - bytes: Sum of all tx sizes
    /// - usage: Total memory usage for the mempool
    async fn get_mempool_info(&self) -> MempoolInfo {
        // Read off the tip-agnostic set: `getmempoolinfo` reports what is in the
        // mempool, not where the chain is, so it must not freeze.
        let info = self.mempool.get_mempool_info();
        MempoolInfo {
            size: info.size,
            bytes: info.bytes,
            usage: info.usage,
        }
    }

    async fn get_tx_out_set_info(&self) -> Result<Option<TxOutSetInfo>, Self::Error> {
        use crate::chain_index::types::db::metadata::{
            is_unspendable_tx_out, ZAINO_TXOUTSET_ENTRY_LEN,
        };
        use hex::ToHex as _;
        use std::collections::HashMap;

        let snapshot = self.snapshot_nonfinalized_state().await?;
        let best_tip = self.best_chaintip(&snapshot).await?;

        let non_finalized_snapshot = match &snapshot {
            ChainIndexSnapshot::NonFinalizedStateExists {
                non_finalized_snapshot,
            } => non_finalized_snapshot,
            ChainIndexSnapshot::StillSyncingFinalizedState { .. } => {
                // Accumulator invariants are not established until the finalised state catches
                // up. Match zcashd's "stats collection failed" empty-object shape.
                return Ok(None);
            }
        };

        let mut accumulator = self
            .finalized_state
            .get_tx_out_set_info_accumulator()
            .await
            .map_err(|e| {
                ChainIndexError::internal(format!(
                    "get_tx_out_set_info: finalised accumulator unavailable: {e}"
                ))
            })?;

        // Outputs created inside the non-finalised state, keyed by outpoint. Lets same-NFS
        // spends resolve their prev output without touching the finalised database.
        let mut nfs_created: HashMap<Outpoint, TxOutCompact> = HashMap::new();

        // Per-transaction "currently-unspent transparent outputs" counter across the combined
        // finalised + non-finalised UTXO set. Seeded lazily:
        // - For NFS-created txs: starts at 0 and increments on each output added.
        // - For purely-finalised prev txs first encountered as a spend: seeded by counting how
        //   many of that tx's transparent outputs are unspent in the finalised state right now.
        //
        // We only modify `accumulator.transactions` on 0↔>0 transitions of this counter; the
        // finalised accumulator already reflects the steady-state count for every tx not
        // touched by the NFS walk.
        let mut tx_unspent_count: HashMap<TransactionHash, u64> = HashMap::new();

        let mut heights: Vec<types::Height> = non_finalized_snapshot
            .heights_to_hashes
            .keys()
            .copied()
            .collect();
        heights.sort();

        for height in heights {
            let Some(block) = non_finalized_snapshot.get_chainblock_by_height(&height) else {
                return Err(ChainIndexError::internal(format!(
                    "get_tx_out_set_info: non-finalised snapshot height {height:?} has no block"
                )));
            };

            for tx in block.transactions() {
                let txid = *tx.txid();
                let transparent = tx.transparent();

                // Created outputs enter the UTXO set.
                //
                // NonStandard (unspendable) outputs are skipped at every level — the accumulator
                // never saw them on the finalised side either, so they must not contribute to
                // `transactions` or to the resolution map for later same-NFS spends.
                for (output_index, output) in transparent.outputs().iter().enumerate() {
                    if is_unspendable_tx_out(output) {
                        continue;
                    }
                    let outpoint = Outpoint::new(txid.0, output_index as u32);
                    accumulator
                        .apply_added_output(&outpoint, output)
                        .map_err(|e| ChainIndexError::internal(e.to_string()))?;
                    nfs_created.insert(outpoint, *output);

                    let entry = tx_unspent_count.entry(txid).or_insert(0);
                    let prev = *entry;
                    *entry += 1;
                    if prev == 0 {
                        // 0 -> >0 transition: this tx enters the in-set transaction count.
                        accumulator.transactions =
                            accumulator.transactions.checked_add(1).ok_or_else(|| {
                                ChainIndexError::internal(
                                    "get_tx_out_set_info: transactions counter overflow"
                                        .to_string(),
                                )
                            })?;
                    }
                }

                // Spent prev outputs leave the UTXO set.
                for outpoint in transparent.spent_outpoints() {
                    let prev_txid = TransactionHash::from(*outpoint.prev_txid());

                    let prev_out_from_nfs = nfs_created.remove(&outpoint);
                    let prev_out = match prev_out_from_nfs {
                        Some(out) => out,
                        None => self
                            .finalized_state
                            .get_previous_output(outpoint)
                            .await
                            .map_err(|e| {
                                ChainIndexError::internal(format!(
                                    "get_tx_out_set_info: finalised prev output for {outpoint:?} not found: {e}"
                                ))
                            })?,
                    };

                    accumulator
                        .apply_removed_output(&outpoint, &prev_out)
                        .map_err(|e| ChainIndexError::internal(e.to_string()))?;

                    // Seed the prev_txid unspent counter if this is the first time we touch it.
                    if let std::collections::hash_map::Entry::Vacant(e) =
                        tx_unspent_count.entry(prev_txid)
                    {
                        let seed = self
                            .count_finalised_unspent_outputs(prev_txid)
                            .await
                            .map_err(|e| {
                                ChainIndexError::internal(format!(
                                    "get_tx_out_set_info: cannot seed unspent counter for {prev_txid:?}: {e}"
                                ))
                            })?;
                        e.insert(seed);
                    }

                    let entry = tx_unspent_count.get_mut(&prev_txid).expect("seeded above");
                    if *entry == 0 {
                        return Err(ChainIndexError::internal(format!(
                            "get_tx_out_set_info: tx {prev_txid:?} unspent counter underflow"
                        )));
                    }
                    *entry -= 1;
                    if *entry == 0 {
                        accumulator.transactions =
                            accumulator.transactions.checked_sub(1).ok_or_else(|| {
                                ChainIndexError::internal(
                                    "get_tx_out_set_info: transactions counter underflow"
                                        .to_string(),
                                )
                            })?;
                    }
                }
            }
        }

        // Invariant: bytes_serialized == transaction_outputs * ZAINO_TXOUTSET_ENTRY_LEN.
        let expected_bytes = accumulator
            .transaction_outputs
            .checked_mul(ZAINO_TXOUTSET_ENTRY_LEN)
            .ok_or_else(|| {
                ChainIndexError::internal(
                    "get_tx_out_set_info: bytes_serialized invariant overflow".to_string(),
                )
            })?;
        if accumulator.bytes_serialized != expected_bytes {
            return Err(ChainIndexError::internal(format!(
                "get_tx_out_set_info: bytes_serialized invariant violated (got {}, expected {})",
                accumulator.bytes_serialized, expected_bytes
            )));
        }

        // ZEC denomination and display-order hex are the wire's business; this
        // hands over integer zatoshis and the hash bytes as they are.
        Ok(Some(TxOutSetInfo {
            height: zaino_primitives::types::Height::try_from(best_tip.height.0)
                .map_err(|e| ChainIndexError::internal(e.to_string()))?,
            best_block: zaino_primitives::types::BlockHash::from(best_tip.hash.0),
            transactions: accumulator.transactions,
            tx_outs: accumulator.transaction_outputs,
            bytes_serialized: accumulator.bytes_serialized,
            hash_serialized: accumulator.hash_serialized.encode_hex(),
            total_amount: zaino_primitives::types::Zatoshis::new(accumulator.total_zatoshis)
                .map_err(|e| ChainIndexError::internal(e.to_string()))?,
        }))
    }
}

/// The available shielded pools
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ShieldedPool {
    /// Sapling
    Sapling,
    /// Orchard
    Orchard,
    /// Ironwood
    Ironwood,
}

impl ShieldedPool {
    /// The network upgrade that activates this pool.
    pub(crate) fn activation_upgrade(&self) -> zebra_chain::parameters::NetworkUpgrade {
        match self {
            ShieldedPool::Sapling => zebra_chain::parameters::NetworkUpgrade::Sapling,
            ShieldedPool::Orchard => zebra_chain::parameters::NetworkUpgrade::Nu5,
            ShieldedPool::Ironwood => zebra_chain::parameters::NetworkUpgrade::Nu6_3,
        }
    }

    /// [`ShieldedPool::activation_upgrade`] in `zcash_protocol` terms, for call sites
    /// gated through [`zcash_protocol::consensus::Parameters`].
    pub(crate) fn zcash_protocol_activation_upgrade(
        &self,
    ) -> zcash_protocol::consensus::NetworkUpgrade {
        match self {
            ShieldedPool::Sapling => zcash_protocol::consensus::NetworkUpgrade::Sapling,
            ShieldedPool::Orchard => zcash_protocol::consensus::NetworkUpgrade::Nu5,
            ShieldedPool::Ironwood => zcash_protocol::consensus::NetworkUpgrade::Nu6_3,
        }
    }

    /// Returns the string representative of the given pool.
    ///
    /// Used for display purposes and in converting the strongly types `PoolType`
    /// struct into the string that the Zcash RPCs require as input.
    pub fn pool_string(&self) -> String {
        match self {
            ShieldedPool::Sapling => "sapling".to_string(),
            ShieldedPool::Orchard => "orchard".to_string(),
            ShieldedPool::Ironwood => "ironwood".to_string(),
        }
    }
}

impl<T> NonFinalizedSnapshot for Arc<T>
where
    T: NonFinalizedSnapshot,
{
    fn get_chainblock_by_hash(&self, target_hash: &types::BlockHash) -> Option<&IndexedBlock> {
        self.as_ref().get_chainblock_by_hash(target_hash)
    }

    fn get_chainblock_by_height(&self, target_height: &types::Height) -> Option<&IndexedBlock> {
        self.as_ref().get_chainblock_by_height(target_height)
    }

    fn max_serviceable_height(&self) -> &types::Height {
        self.as_ref().max_serviceable_height()
    }
}

/// A snapshot of the non-finalized state, for consistent queries
pub trait NonFinalizedSnapshot {
    /// Hash -> block
    fn get_chainblock_by_hash(&self, target_hash: &types::BlockHash) -> Option<&IndexedBlock>;
    /// Height -> block
    fn get_chainblock_by_height(&self, target_height: &types::Height) -> Option<&IndexedBlock>;
    /// The maximum height that this snapshot can serve data for.
    fn max_serviceable_height(&self) -> &types::Height;
}

impl NonFinalizedSnapshot for NonfinalizedBlockCacheSnapshot {
    fn get_chainblock_by_hash(&self, target_hash: &types::BlockHash) -> Option<&IndexedBlock> {
        self.blocks.iter().find_map(|(hash, chainblock)| {
            if hash == target_hash {
                Some(chainblock)
            } else {
                None
            }
        })
    }
    fn get_chainblock_by_height(&self, target_height: &types::Height) -> Option<&IndexedBlock> {
        self.heights_to_hashes.iter().find_map(|(height, hash)| {
            if height == target_height {
                self.get_chainblock_by_hash(hash)
            } else {
                None
            }
        })
    }

    fn max_serviceable_height(&self) -> &types::Height {
        &self.best_tip.height
    }
}

impl NonFinalizedSnapshot for ChainIndexSnapshot {
    fn get_chainblock_by_hash(&self, target_hash: &types::BlockHash) -> Option<&IndexedBlock> {
        match self {
            ChainIndexSnapshot::NonFinalizedStateExists {
                non_finalized_snapshot,
            } => non_finalized_snapshot.get_chainblock_by_hash(target_hash),

            ChainIndexSnapshot::StillSyncingFinalizedState { .. } => None,
        }
    }

    fn get_chainblock_by_height(&self, target_height: &types::Height) -> Option<&IndexedBlock> {
        match self {
            ChainIndexSnapshot::NonFinalizedStateExists {
                non_finalized_snapshot,
            } => non_finalized_snapshot.get_chainblock_by_height(target_height),

            ChainIndexSnapshot::StillSyncingFinalizedState { .. } => None,
        }
    }

    fn max_serviceable_height(&self) -> &types::Height {
        match self {
            ChainIndexSnapshot::NonFinalizedStateExists {
                non_finalized_snapshot,
            } => non_finalized_snapshot.max_serviceable_height(),

            ChainIndexSnapshot::StillSyncingFinalizedState {
                validator_finalized_height,
            } => validator_finalized_height,
        }
    }
}
