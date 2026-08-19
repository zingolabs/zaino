//! The routing table: which transport answers each question.

use std::time::Duration;

use tokio::sync::watch;
use zaino_primitives::types::{
    rpc, AddressBalance, AddressDelta, Block, BlockHash, BlockVerbose, BlockchainInfo, Difficulty,
    Height, OutputIndex, PreIndexCompactBlock, ShieldedPool, SubtreeRoot, TransactionId, TreeRoots,
    Treestate, Utxo,
};
use zaino_source::*;
use zaino_source_zebra_readstate::ZebraReadStateAdapter;
use zaino_source_zebra_rpc::ZebraRpcAdapter;

use crate::fallback::retry_on_slow_path;

/// A Zebra validator reached over one or both of its transports.
pub struct ZebraValidator {
    /// Always present: the mempool and the passthrough RPCs are reachable no
    /// other way.
    rpc: ZebraRpcAdapter,
    /// The accelerator, when this deployment has direct database access.
    readstate: Option<ZebraReadStateAdapter>,
    /// Synthesised tip subscription, present once `with_tip_polling` is called.
    tip: Option<PolledChainTip>,
}

impl ZebraValidator {
    /// A validator reached over JSON-RPC alone.
    pub fn rpc_only(rpc: ZebraRpcAdapter) -> Self {
        Self {
            rpc,
            readstate: None,
            tip: None,
        }
    }

    /// A validator whose state database is also readable directly.
    ///
    /// The JSON-RPC adapter is still required: it serves the mempool and the
    /// passthrough RPCs, which the state database cannot answer at all.
    pub fn with_read_state(rpc: ZebraRpcAdapter, readstate: ZebraReadStateAdapter) -> Self {
        Self {
            rpc,
            readstate: Some(readstate),
            tip: None,
        }
    }

    /// Add a tip subscription, polling `source` every `interval`.
    ///
    /// Opt-in and fallible, rather than part of construction, for two reasons.
    /// Seeding a subscription takes one live read, so folding it into
    /// construction would make a validator handle impossible to build while the
    /// validator is down — exactly when an indexer most wants to start and
    /// retry. And polling a validator nobody is watching is pure cost, so the
    /// caller says when it wants the capability.
    ///
    /// The poll task owns its source for its lifetime and so cannot borrow this
    /// composite; the caller passes a second handle to the same validator.
    /// Anything that can answer [`GetChainTip`] will do, which also lets a test
    /// drive the subscription without a validator.
    ///
    /// Zebra's read-only state handle exposes no native tip stream, so this is
    /// currently the only way to obtain one over either transport. If that
    /// changes, [`SubscribeChainTip`] prefers the native stream and this
    /// becomes the fallback.
    pub async fn with_tip_polling<S>(
        mut self,
        source: S,
        interval: Duration,
    ) -> Result<Self, QueryError<GetChainTipError>>
    where
        S: GetChainTip + Send + 'static,
    {
        self.tip = Some(PolledChainTip::spawn(source, interval).await?);
        Ok(self)
    }

    /// The state adapter, when this deployment has one.
    fn fast(&self) -> Option<&ZebraReadStateAdapter> {
        self.readstate.as_ref()
    }

    /// The state adapter, exposed for tests that read the database directly.
    #[cfg(feature = "test_dependencies")]
    pub fn read_state(&self) -> Option<&ZebraReadStateAdapter> {
        self.readstate.as_ref()
    }
}

/// Route a query to the state service, falling back to JSON-RPC on a domain
/// miss.
///
/// Used only where the fast path is *semantically* narrower than the slow one —
/// side-chain blocks and unmined transactions — not as a general availability
/// fallback. See [`retry_on_slow_path`].
///
/// A macro rather than a function because a function cannot express it: the
/// call has to dispatch the same method name across two unrelated types and
/// return a future that borrows the receiver, which no closure signature in
/// stable Rust can name.
macro_rules! fast_then_slow {
    ($self:ident, $method:ident $(, $arg:expr)*) => {{
        if let Some(fast) = $self.fast() {
            let result = fast.$method($($arg),*).await;
            if !retry_on_slow_path(&result) {
                return result;
            }
        }
        $self.rpc.$method($($arg),*).await
    }};
}

/// Route a query to the state service where available, JSON-RPC otherwise.
macro_rules! fast_or_slow {
    ($self:ident, $method:ident $(, $arg:expr)*) => {{
        match $self.fast() {
            Some(fast) => fast.$method($($arg),*).await,
            None => $self.rpc.$method($($arg),*).await,
        }
    }};
}

// ---------------------------------------------------------------------------
// Blocks and chain
// ---------------------------------------------------------------------------

impl GetBlock for ZebraValidator {
    async fn get_block(&self, height: Height) -> Result<Block, QueryError<GetBlockError>> {
        // A height names a best-chain block, which the finalized state has, so
        // there is nothing the slow path could add on a miss.
        fast_or_slow!(self, get_block, height)
    }
}

impl GetBlockByHash for ZebraValidator {
    async fn get_block_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Block, QueryError<GetBlockByHashError>> {
        // A hash can name a side-chain block, which the finalized state does
        // not hold. Its `NotFound` therefore means "not in the finalized
        // state", not "no such block" — so the miss is retried over JSON-RPC,
        // which sees the whole block tree. This is the accumulated knowledge
        // the previous enum encoded inline.
        fast_then_slow!(self, get_block_by_hash, hash)
    }
}

impl GetRawBlock for ZebraValidator {
    async fn get_raw_block(&self, height: Height) -> Result<Vec<u8>, QueryError<GetBlockError>> {
        fast_or_slow!(self, get_raw_block, height)
    }
}

impl GetRawBlockByHash for ZebraValidator {
    async fn get_raw_block_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Vec<u8>, QueryError<GetBlockByHashError>> {
        // Same side-chain gap as `GetBlockByHash`: the finalized state does not
        // hold blocks off the best chain.
        fast_then_slow!(self, get_raw_block_by_hash, hash)
    }
}

impl GetChainTip for ZebraValidator {
    async fn get_chain_tip(&self) -> Result<(BlockHash, Height), QueryError<GetChainTipError>> {
        fast_or_slow!(self, get_chain_tip)
    }
}

impl GetBestBlockHeight for ZebraValidator {
    async fn get_best_block_height(&self) -> Result<Height, QueryError<GetBestBlockHeightError>> {
        fast_or_slow!(self, get_best_block_height)
    }
}

impl GetPreIndexCompactBlock for ZebraValidator {
    async fn get_pre_index_compact_block(
        &self,
        height: Height,
    ) -> Result<PreIndexCompactBlock, QueryError<GetBlockError>> {
        fast_or_slow!(self, get_pre_index_compact_block, height)
    }
}

// ---------------------------------------------------------------------------
// Transactions
// ---------------------------------------------------------------------------

impl GetTransaction for ZebraValidator {
    async fn get_transaction(
        &self,
        txid: TransactionId,
    ) -> Result<TransactionResponse, QueryError<GetTransactionError>> {
        // The state service has no mempool, so its `NotFound` means "not
        // mined". An unmined transaction is found only over JSON-RPC, which is
        // why this is a fallback rather than a preference.
        fast_then_slow!(self, get_transaction, txid)
    }
}

// ---------------------------------------------------------------------------
// Shielded state
// ---------------------------------------------------------------------------

impl GetTreestate for ZebraValidator {
    async fn get_treestate(
        &self,
        height: Height,
    ) -> Result<Treestate, QueryError<GetTreestateError>> {
        fast_or_slow!(self, get_treestate, height)
    }
}

impl GetTreestateByHash for ZebraValidator {
    async fn get_treestate_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Treestate, QueryError<GetTreestateByHashError>> {
        fast_or_slow!(self, get_treestate_by_hash, hash)
    }
}

impl GetCommitmentTreeRoots for ZebraValidator {
    async fn get_commitment_tree_roots(
        &self,
        block: BlockHash,
    ) -> Result<TreeRoots, QueryError<GetCommitmentTreeRootsError>> {
        // Strongly worth taking the fast path: over JSON-RPC the roots are not
        // reported at all and have to be recovered by deserialising each pool's
        // commitment tree, whereas the state service hands back a live tree.
        fast_or_slow!(self, get_commitment_tree_roots, block)
    }
}

impl GetSubtreeRoots for ZebraValidator {
    async fn get_subtree_roots(
        &self,
        pool: ShieldedPool,
        start_index: u16,
        limit: Option<u16>,
    ) -> Result<Vec<SubtreeRoot>, QueryError<GetSubtreeRootsError>> {
        fast_or_slow!(self, get_subtree_roots, pool, start_index, limit)
    }
}

// ---------------------------------------------------------------------------
// Transparent addresses
// ---------------------------------------------------------------------------

impl GetAddressBalance for ZebraValidator {
    async fn get_address_balance(
        &self,
        addresses: Vec<String>,
    ) -> Result<AddressBalance, QueryError<GetAddressBalanceError>> {
        match self.fast() {
            Some(fast) => fast.get_address_balance(addresses).await,
            None => self.rpc.get_address_balance(addresses).await,
        }
    }
}

impl GetAddressTxids for ZebraValidator {
    async fn get_address_txids(
        &self,
        addresses: Vec<String>,
        start: Height,
        end: Height,
    ) -> Result<Vec<TransactionId>, QueryError<GetAddressTxidsError>> {
        match self.fast() {
            Some(fast) => fast.get_address_txids(addresses, start, end).await,
            None => self.rpc.get_address_txids(addresses, start, end).await,
        }
    }
}

impl GetAddressUtxos for ZebraValidator {
    async fn get_address_utxos(
        &self,
        addresses: Vec<String>,
    ) -> Result<Vec<Utxo>, QueryError<GetAddressUtxosError>> {
        match self.fast() {
            Some(fast) => fast.get_address_utxos(addresses).await,
            None => self.rpc.get_address_utxos(addresses).await,
        }
    }
}

impl GetAddressDeltas for ZebraValidator {
    async fn get_address_deltas(
        &self,
        addresses: Vec<String>,
        start: Height,
        end: Height,
    ) -> Result<Vec<AddressDelta>, QueryError<GetAddressDeltasError>> {
        // The state service first where there is one. This inverts the usual
        // reasoning — deltas cover every transaction in a height range, so
        // asking the validator to compute them would be the natural choice —
        // but `getaddressdeltas` is a zcashd method that Zebra does not
        // implement. Against Zebra the state service is not an accelerator
        // here; it is the only thing that can answer at all.
        //
        // Both paths report mined transactions only, so the routing does not
        // change which transactions are covered. Against zcashd the RPC path
        // additionally reports spends, which the state service cannot resolve
        // (see the readstate implementation).
        match self.fast() {
            Some(fast) => fast.get_address_deltas(addresses, start, end).await,
            None => self.rpc.get_address_deltas(addresses, start, end).await,
        }
    }
}

// ---------------------------------------------------------------------------
// JSON-RPC only
//
// Everything below has no state-service implementation, because the state
// database cannot answer it: mempool contents, node-local facts, the block
// tree beyond the finalized chain, and derived queries the validator computes.
// These need no routing decision — there is one transport that can answer.
// ---------------------------------------------------------------------------

impl GetMempoolTxids for ZebraValidator {
    async fn get_mempool_txids(
        &self,
    ) -> Result<Vec<TransactionId>, QueryError<GetMempoolTxidsError>> {
        self.rpc.get_mempool_txids().await
    }
}

impl GetMempoolMetadata for ZebraValidator {
    async fn get_mempool_metadata(
        &self,
    ) -> Result<Vec<MempoolTxMeta>, QueryError<GetMempoolMetadataError>> {
        self.rpc.get_mempool_metadata().await
    }
}

impl GetRawMempoolTransaction for ZebraValidator {
    async fn get_raw_mempool_transaction(
        &self,
        txid: TransactionId,
    ) -> Result<Vec<u8>, QueryError<GetRawMempoolTransactionError>> {
        self.rpc.get_raw_mempool_transaction(txid).await
    }
}

impl GetMempoolSourceTip for ZebraValidator {
    async fn get_mempool_source_tip(
        &self,
    ) -> Result<(BlockHash, Height), QueryError<std::convert::Infallible>> {
        // Deliberately *not* `fast_or_slow!`, unlike `GetChainTip` above. This
        // tip tags a mempool set read over JSON-RPC, and the comparison it
        // exists for is only sound if both come from one source — see the port's
        // documentation.
        self.rpc.get_mempool_source_tip().await
    }
}

impl GetChainTips for ZebraValidator {
    async fn get_chain_tips(&self) -> Result<Vec<rpc::ChainTip>, QueryError<GetChainTipsError>> {
        // Enumerating the block tree includes side-chain tips, which the
        // finalized state does not retain.
        self.rpc.get_chain_tips().await
    }
}

impl GetBlockVerbose for ZebraValidator {
    async fn get_block_verbose(
        &self,
        height: Height,
    ) -> Result<BlockVerbose, QueryError<GetBlockVerboseError>> {
        self.rpc.get_block_verbose(height).await
    }
}

impl GetBlockVerboseByHash for ZebraValidator {
    async fn get_block_verbose_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<BlockVerbose, QueryError<GetBlockVerboseError>> {
        self.rpc.get_block_verbose_by_hash(hash).await
    }
}

impl GetBlockHeader for ZebraValidator {
    async fn get_block_header(
        &self,
        hash: BlockHash,
    ) -> Result<rpc::BlockHeaderVerbose, QueryError<GetBlockHeaderError>> {
        self.rpc.get_block_header(hash).await
    }
}

impl GetRawBlockHeader for ZebraValidator {
    async fn get_raw_block_header(
        &self,
        hash: BlockHash,
    ) -> Result<Vec<u8>, QueryError<GetBlockHeaderError>> {
        self.rpc.get_raw_block_header(hash).await
    }
}

impl GetBlockDeltas for ZebraValidator {
    async fn get_block_deltas(
        &self,
        hash: BlockHash,
    ) -> Result<rpc::BlockDeltas, QueryError<GetBlockDeltasError>> {
        // State service first, and it is not merely a preference: `getblockdeltas`
        // is a zcashd method that **zebrad does not implement** — it answers
        // `-32601 Method not found` — so on a zebrad-backed deployment the
        // derivation in the state adapter is the only implementation there is.
        // The RPC path remains for zcashd, and for a side-chain block the
        // finalized state does not hold.
        fast_then_slow!(self, get_block_deltas, hash)
    }
}

impl GetBlockSubsidy for ZebraValidator {
    async fn get_block_subsidy(
        &self,
        height: Height,
    ) -> Result<rpc::BlockSubsidy, QueryError<GetBlockSubsidyError>> {
        self.rpc.get_block_subsidy(height).await
    }
}

impl GetNodeInfo for ZebraValidator {
    async fn get_node_info(&self) -> Result<rpc::NodeInfo, QueryError<GetNodeInfoError>> {
        self.rpc.get_node_info().await
    }
}

impl GetPeerInfo for ZebraValidator {
    async fn get_peer_info(&self) -> Result<Vec<rpc::PeerInfo>, QueryError<GetPeerInfoError>> {
        self.rpc.get_peer_info().await
    }
}

impl GetMiningInfo for ZebraValidator {
    async fn get_mining_info(&self) -> Result<rpc::MiningInfo, QueryError<GetMiningInfoError>> {
        self.rpc.get_mining_info().await
    }
}

impl GetNetworkSolPs for ZebraValidator {
    async fn get_network_sol_ps(
        &self,
        blocks: Option<u32>,
        height: Option<Height>,
    ) -> Result<u64, QueryError<GetNetworkSolPsError>> {
        self.rpc.get_network_sol_ps(blocks, height).await
    }
}

impl GetTxOut for ZebraValidator {
    async fn get_tx_out(
        &self,
        txid: TransactionId,
        index: OutputIndex,
        include_mempool: bool,
    ) -> Result<Option<rpc::TxOut>, QueryError<GetTxOutError>> {
        self.rpc.get_tx_out(txid, index, include_mempool).await
    }
}

impl GetSpentInfo for ZebraValidator {
    async fn get_spent_info(
        &self,
        outpoint: rpc::SpentOutpoint,
    ) -> Result<rpc::SpentInfo, QueryError<GetSpentInfoError>> {
        // RPC only, and not merely by preference: `getspentinfo` reads a spent
        // index that the read-state service does not expose, so there is no
        // faster path to prefer. Against zebrad this answers `Unsupported`.
        self.rpc.get_spent_info(outpoint).await
    }
}

impl SendRawTransaction for ZebraValidator {
    async fn send_raw_transaction(
        &self,
        transaction: Vec<u8>,
    ) -> Result<TransactionId, QueryError<SendRawTransactionError>> {
        self.rpc.send_raw_transaction(transaction).await
    }
}

// ---------------------------------------------------------------------------
// Chain-wide facts
// ---------------------------------------------------------------------------

impl GetDifficulty for ZebraValidator {
    async fn get_difficulty(&self) -> Result<Difficulty, QueryError<GetDifficultyError>> {
        fast_or_slow!(self, get_difficulty)
    }
}

impl GetBlockchainInfo for ZebraValidator {
    async fn get_blockchain_info(
        &self,
    ) -> Result<BlockchainInfo, QueryError<GetBlockchainInfoError>> {
        fast_or_slow!(self, get_blockchain_info)
    }
}

// ---------------------------------------------------------------------------
// Subscriptions and lifecycle
// ---------------------------------------------------------------------------

impl SubscribeChainTip for ZebraValidator {
    fn subscribe_to_chain_tip(&self) -> Option<watch::Receiver<TipObservation>> {
        // Prefer a native stream if either adapter ever gains one; fall back to
        // the synthesised poller. Today neither transport has a native stream,
        // so this is the poller or nothing.
        self.readstate
            .as_ref()
            .and_then(|readstate| readstate.subscribe_to_chain_tip())
            .or_else(|| {
                self.tip
                    .as_ref()
                    .and_then(|tip| tip.subscribe_to_chain_tip())
            })
    }
}

impl SubscribeBlocks for ZebraValidator {
    fn subscribe_to_blocks_received(&self) -> Option<watch::Receiver<()>> {
        // Neither transport pushes block arrivals; that signal belongs to the
        // syncer, which neither adapter owns.
        None
    }
}

impl SourceLifecycle for ZebraValidator {
    fn shutdown(&self) {
        // Both, unconditionally: shutdown is idempotent and an adapter that
        // owns nothing has a no-op implementation, so there is nothing gained
        // by asking which of them holds resources.
        self.rpc.shutdown();
        if let Some(readstate) = &self.readstate {
            readstate.shutdown();
        }
    }
}
