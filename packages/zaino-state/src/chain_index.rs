//! Zaino's legacy chain index: the `ChainIndex` interface, answered by
//! ChainView and the mempool.
//!
//! A deprecated adapter. It launches the chain store, the chain head, the
//! chain view over them and the mempool, converts between their types and the
//! legacy ones, and passes each call through. It holds no chain logic of its
//! own.

use crate::chain_index::types::BlockIndex;
use crate::chain_index::types::{BestChainLocation, NonBestChainLocation};
use crate::CompactBlockStream;
use crate::IndexedBlock;
use std::collections::HashSet;
use zaino_primitives::types::rpc::{
    AddressDeltas, AddressDeltasRequest, BlockDeltas, BlockHeaderVerbose, BlockSubsidy, MiningInfo,
    NodeInfo, PeerInfo,
};
use zaino_primitives::types::MempoolInfo;
use zaino_primitives::types::TxOutSetInfo;
use zaino_proto::proto::utils::PoolTypeFilter;
pub use zebra_chain::parameters::Network as ZebraNetwork;
use zebra_rpc::{
    client::{GetAddressBalanceRequest, GetAddressTxIdsRequest},
    methods::GetBlock,
};

/// ChainIndex's side of the ChainHead boundary: handing ChainHead a validator.
pub mod chain_head;
/// ChainIndex's side of the ChainStore boundary: handing the finalised store a
/// validator.
pub mod chain_store;
/// ChainIndex's side of the ChainView boundary: handing ChainView a validator,
/// and composing it.
pub mod chain_view;
mod mempool;
mod network_adoption;
mod node_backed;
mod reads;
mod rpc_reads;
/// ChainIndex's driven port onto the validator. Temporary scaffolding — see
/// the module docs.
pub mod source;
pub mod source_ports;
/// The legacy type vocabulary and its conversions.
pub(crate) mod types;
pub mod validator_source;
pub mod wire_types;

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) use node_backed::combine_component_statuses;
pub use node_backed::{NodeBackedChainIndex, NodeBackedChainIndexSubscriber};

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
#[cfg(test)]
pub(crate) fn finalized_height_floor(chain_tip: u32) -> crate::Height {
    crate::Height(chain_tip.saturating_sub(OPERATIONAL_NFS_DEPTH))
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
/// - A JSON-RPC connection to a validator node (zebrad or another zainod)
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
/// #     subscriber: impl zaino_state::ChainIndex<Error: std::error::Error + 'static>,
/// # ) -> Result<(), Box<dyn std::error::Error>> {
/// use zaino_state::ChainIndex as _;
///
/// // Capturing a snapshot cannot fail and cannot block: the chain head has
/// // published one before this subscriber could exist.
/// let snapshot = subscriber.snapshot_nonfinalized_state();
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
    /// A snapshot of the chain, needed for atomic access.
    ///
    /// Every query below takes one rather than reading the live state, so a
    /// caller that captures a snapshot and asks several questions gets answers
    /// describing a single coherent chain state.
    type Snapshot: zaino_chain::ChainViewSnapshot;

    /// How it can fail
    type Error;

    // ********** Utility methods **********

    /// Captures the view every query below answers from.
    ///
    /// Neither fallible nor awaited: the chain head publishes a complete view
    /// and republishes whole, so there is always exactly one coherent view to
    /// hand back and nothing to wait for. A caller asking several questions
    /// captures once and passes the result to each, so the answers describe a
    /// single chain state even if the chain moves in between.
    fn snapshot_nonfinalized_state(&self) -> Self::Snapshot;

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
    fn get_indexed_block_by_hash(
        &self,
        snapshot: &Self::Snapshot,
        target_hash: &types::BlockHash,
    ) -> impl std::future::Future<Output = Result<Option<IndexedBlock>, Self::Error>>;

    /// Returns Some(IndexedBlock) for the given block height.in the best chain.
    ///
    /// Returns None if the specified block height is above the best chain tip.
    fn get_indexed_block_by_height(
        &self,
        snapshot: &Self::Snapshot,
        target_height: &types::Height,
    ) -> impl std::future::Future<Output = Result<Option<IndexedBlock>, Self::Error>>;

    /// Given inclusive start and end heights, stream all blocks
    /// between the given heights, descending when `start` is above `end`.
    /// A missing `end` is the snapshot's tip; a height above the tip yields
    /// an error.
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
    /// `verbosity` follows the legacy full-node `getblock` convention (0 = raw, 1 = object with
    /// txids, 2 = object with full transaction data).
    ///
    /// Zcash RPC reference: [`getblock`](https://zcash.github.io/rpc/getblock.html)
    fn z_get_block(
        &self,
        hash_or_height: String,
        verbosity: Option<u8>,
    ) -> impl std::future::Future<Output = Result<GetBlock, Self::Error>>;

    /// Returns the `getblockheader`-shaped header for the given block hash.
    ///
    /// Zcash RPC reference: [`getblockheader`](https://zcash.github.io/rpc/getblockheader.html)
    fn get_block_header(
        &self,
        hash: String,
    ) -> impl std::future::Future<Output = Result<BlockHeaderVerbose, Self::Error>>;

    /// Returns the raw serialised header of the block with the given hash.
    ///
    /// The non-verbose half of `getblockheader`; see
    /// [`BlockchainSource::get_raw_block_header`](source::BlockchainSource::get_raw_block_header).
    fn get_raw_block_header(
        &self,
        hash: String,
    ) -> impl std::future::Future<Output = Result<Vec<u8>, Self::Error>>;

    /// Returns the `getblockdeltas`-shaped transparent input/output deltas for the block
    /// with the given hash.
    ///
    /// Zcash RPC reference: [`getblockdeltas`](https://zcash.github.io/rpc/getblockdeltas.html)
    fn get_block_deltas(
        &self,
        hash: String,
    ) -> impl std::future::Future<Output = Result<BlockDeltas, Self::Error>>;

    /// Returns the proof-of-work difficulty of the best chain as a multiple of the
    /// minimum difficulty.
    ///
    /// Zcash RPC reference: [`getdifficulty`](https://zcash.github.io/rpc/getdifficulty.html)
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

    /// Returns the full `gettxoutsetinfo` response for the whole chain at the tip.
    ///
    /// Returns `None` while the finalised state and the chain head do not yet
    /// meet. The wire layer renders that as the legacy full node's empty object.
    fn get_tx_out_set_info(
        &self,
    ) -> impl std::future::Future<Output = Result<Option<TxOutSetInfo>, Self::Error>>;
}

/// The shielded pools this crate names.
///
/// Not defined here: the finalised store owns the mapping from a pool to the
/// network upgrade that activates it, because that is what it needs to decide
/// whether a block should have a commitment tree root. A second copy here drifts
/// the moment a pool is added.
pub use zaino_chain_store_zainodb::pool::ShieldedPool;
