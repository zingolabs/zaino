//! The Zebra validator source: every query answered over JSON-RPC.

use std::time::Duration;

use tokio::sync::watch;
use zaino_primitives::types::{
    rpc, AddressBalance, AddressDelta, Block, BlockHash, BlockVerbose, BlockchainInfo, Difficulty,
    Height, OutputIndex, PreIndexCompactBlock, PreIndexCompactTx, ShieldedPool, SubtreeRoot,
    TransactionId, TreeRoots, Treestate, Utxo,
};
use zaino_source::*;
use zaino_source_zebra_rpc::ZebraRpcAdapter;

/// Normalise a sub-adapter's own non-domain error to the seam type, so the
/// source presents one `QueryError<E>` regardless of which handle answered.
fn to_seam<
    E: core::fmt::Debug + core::fmt::Display,
    N: std::error::Error + Into<NonDomainError>,
>(
    e: QueryError<E, N>,
) -> QueryError<E> {
    match e {
        QueryError::Domain(d) => QueryError::Domain(d),
        QueryError::NonDomain(n) => QueryError::NonDomain(n.into()),
    }
}

/// A Zebra validator reached over its JSON-RPC interface.
///
/// JSON-RPC answers every query — blocks, the mempool, the passthrough RPCs, and
/// the derived queries the validator computes — at the cost of a request/response
/// round-trip per call. It is the only transport this source speaks.
pub struct ZebraValidator {
    /// The JSON-RPC handle every query routes through.
    rpc: ZebraRpcAdapter,
    /// Synthesised tip subscription, present once `with_tip_polling` is called.
    tip: Option<PolledChainTip>,
}

impl ZebraValidator {
    /// A validator reached over JSON-RPC.
    pub fn rpc_only(rpc: ZebraRpcAdapter) -> Self {
        Self { rpc, tip: None }
    }

    /// Add a tip subscription, polling `source` every `interval`.
    ///
    /// Fallible and opt-in: seeding takes one live read, so a caller building a
    /// handle while the validator is down defers this until it is up. The poll
    /// task owns `source` for its lifetime, so the caller passes a second handle
    /// to the same validator; anything answering [`GetChainTip`] will do, which
    /// also lets a test drive the subscription without one.
    pub async fn with_tip_polling<S>(
        mut self,
        source: S,
        interval: Duration,
    ) -> Result<Self, QueryError<GetChainTipError>>
    where
        S: OneShotGetChainTip + Send + 'static,
    {
        self.tip = Some(
            PolledChainTip::spawn(source, interval)
                .await
                .map_err(to_seam)?,
        );
        Ok(self)
    }
}

// ---------------------------------------------------------------------------
// Blocks and chain
// ---------------------------------------------------------------------------

impl zaino_source::ValidatorSource for ZebraValidator {
    type NonDomain = zaino_source::NonDomainError;
}

impl OneShotGetBlock for ZebraValidator {
    async fn get_block(&self, height: Height) -> Result<Block, QueryError<GetBlockError>> {
        self.rpc.get_block(height).await
    }
}

impl OneShotGetBlockByHash for ZebraValidator {
    async fn get_block_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Block, QueryError<GetBlockByHashError>> {
        self.rpc.get_block_by_hash(hash).await
    }
}

impl OneShotGetRawBlock for ZebraValidator {
    async fn get_raw_block(&self, height: Height) -> Result<Vec<u8>, QueryError<GetBlockError>> {
        self.rpc.get_raw_block(height).await
    }
}

impl OneShotGetRawBlockByHash for ZebraValidator {
    async fn get_raw_block_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Vec<u8>, QueryError<GetBlockByHashError>> {
        self.rpc.get_raw_block_by_hash(hash).await
    }
}

impl OneShotGetChainTip for ZebraValidator {
    async fn get_chain_tip(&self) -> Result<(BlockHash, Height), QueryError<GetChainTipError>> {
        self.rpc.get_chain_tip().await
    }
}

impl OneShotGetBestBlockHeight for ZebraValidator {
    async fn get_best_block_height(&self) -> Result<Height, QueryError<GetBestBlockHeightError>> {
        self.rpc.get_best_block_height().await
    }
}

impl OneShotGetPreIndexCompactBlock for ZebraValidator {
    async fn get_pre_index_compact_block(
        &self,
        height: Height,
    ) -> Result<PreIndexCompactBlock, QueryError<GetBlockError>> {
        self.rpc.get_pre_index_compact_block(height).await
    }
}

// ---------------------------------------------------------------------------
// Transactions
// ---------------------------------------------------------------------------

impl OneShotGetTransaction for ZebraValidator {
    async fn get_transaction(
        &self,
        txid: TransactionId,
    ) -> Result<TransactionResponse, QueryError<GetTransactionError>> {
        self.rpc.get_transaction(txid).await
    }
}

// ---------------------------------------------------------------------------
// Shielded state
// ---------------------------------------------------------------------------

impl OneShotGetTreestate for ZebraValidator {
    async fn get_treestate(
        &self,
        height: Height,
    ) -> Result<Treestate, QueryError<GetTreestateError>> {
        self.rpc.get_treestate(height).await
    }
}

impl OneShotGetTreestateByHash for ZebraValidator {
    async fn get_treestate_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Treestate, QueryError<GetTreestateByHashError>> {
        self.rpc.get_treestate_by_hash(hash).await
    }
}

impl OneShotGetCommitmentTreeRoots for ZebraValidator {
    async fn get_commitment_tree_roots(
        &self,
        block: BlockHash,
    ) -> Result<TreeRoots, QueryError<GetCommitmentTreeRootsError>> {
        self.rpc.get_commitment_tree_roots(block).await
    }
}

impl OneShotGetSubtreeRoots for ZebraValidator {
    async fn get_subtree_roots(
        &self,
        pool: ShieldedPool,
        start_index: u16,
        limit: Option<u16>,
    ) -> Result<Vec<SubtreeRoot>, QueryError<GetSubtreeRootsError>> {
        self.rpc.get_subtree_roots(pool, start_index, limit).await
    }
}

// ---------------------------------------------------------------------------
// Transparent addresses
// ---------------------------------------------------------------------------

impl OneShotGetAddressBalance for ZebraValidator {
    async fn get_address_balance(
        &self,
        addresses: Vec<String>,
    ) -> Result<AddressBalance, QueryError<GetAddressBalanceError>> {
        self.rpc.get_address_balance(addresses).await
    }
}

impl OneShotGetAddressTxids for ZebraValidator {
    async fn get_address_txids(
        &self,
        addresses: Vec<String>,
        start: Height,
        end: Height,
    ) -> Result<Vec<TransactionId>, QueryError<GetAddressTxidsError>> {
        self.rpc.get_address_txids(addresses, start, end).await
    }
}

impl OneShotGetAddressUtxos for ZebraValidator {
    async fn get_address_utxos(
        &self,
        addresses: Vec<String>,
    ) -> Result<Vec<Utxo>, QueryError<GetAddressUtxosError>> {
        self.rpc.get_address_utxos(addresses).await
    }
}

impl OneShotGetAddressDeltas for ZebraValidator {
    async fn get_address_deltas(
        &self,
        addresses: Vec<String>,
        start: Height,
        end: Height,
    ) -> Result<Vec<AddressDelta>, QueryError<GetAddressDeltasError>> {
        self.rpc.get_address_deltas(addresses, start, end).await
    }
}

// ---------------------------------------------------------------------------
// Mempool and node-local facts
// ---------------------------------------------------------------------------

impl OneShotGetMempoolTxids for ZebraValidator {
    async fn get_mempool_txids(
        &self,
    ) -> Result<Vec<TransactionId>, QueryError<GetMempoolTxidsError>> {
        self.rpc.get_mempool_txids().await
    }
}

impl OneShotGetMempoolMetadata for ZebraValidator {
    async fn get_mempool_metadata(
        &self,
    ) -> Result<Vec<MempoolTxMeta>, QueryError<GetMempoolMetadataError>> {
        self.rpc.get_mempool_metadata().await
    }
}

impl OneShotGetRawMempoolTransaction for ZebraValidator {
    async fn get_raw_mempool_transaction(
        &self,
        txid: TransactionId,
    ) -> Result<Vec<u8>, QueryError<GetRawMempoolTransactionError>> {
        self.rpc.get_raw_mempool_transaction(txid).await
    }
}

impl OneShotGetMempoolCompactTransaction for ZebraValidator {
    async fn get_mempool_compact_transaction(
        &self,
        txid: TransactionId,
    ) -> Result<PreIndexCompactTx, QueryError<GetRawMempoolTransactionError>> {
        self.rpc.get_mempool_compact_transaction(txid).await
    }
}

impl OneShotGetMempoolSourceTip for ZebraValidator {
    async fn get_mempool_source_tip(
        &self,
    ) -> Result<(BlockHash, Height), QueryError<std::convert::Infallible>> {
        self.rpc.get_mempool_source_tip().await
    }
}

impl OneShotGetChainTips for ZebraValidator {
    async fn get_chain_tips(&self) -> Result<Vec<rpc::ChainTip>, QueryError<GetChainTipsError>> {
        self.rpc.get_chain_tips().await
    }
}

impl OneShotGetBlockVerbose for ZebraValidator {
    async fn get_block_verbose(
        &self,
        height: Height,
    ) -> Result<BlockVerbose, QueryError<GetBlockVerboseError>> {
        self.rpc.get_block_verbose(height).await
    }
}

impl OneShotGetBlockVerboseByHash for ZebraValidator {
    async fn get_block_verbose_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<BlockVerbose, QueryError<GetBlockVerboseError>> {
        self.rpc.get_block_verbose_by_hash(hash).await
    }
}

impl OneShotGetBlockHeader for ZebraValidator {
    async fn get_block_header(
        &self,
        hash: BlockHash,
    ) -> Result<rpc::BlockHeaderVerbose, QueryError<GetBlockHeaderError>> {
        self.rpc.get_block_header(hash).await
    }
}

impl OneShotGetRawBlockHeader for ZebraValidator {
    async fn get_raw_block_header(
        &self,
        hash: BlockHash,
    ) -> Result<Vec<u8>, QueryError<GetBlockHeaderError>> {
        self.rpc.get_raw_block_header(hash).await
    }
}

impl OneShotGetBlockDeltas for ZebraValidator {
    async fn get_block_deltas(
        &self,
        hash: BlockHash,
    ) -> Result<rpc::BlockDeltas, QueryError<GetBlockDeltasError>> {
        // `getblockdeltas` is a legacy full-node method that **zebrad does not
        // implement** — it answers `-32601 Method not found`. It is served here
        // only by the legacy full node.
        self.rpc.get_block_deltas(hash).await
    }
}

impl OneShotGetBlockSubsidy for ZebraValidator {
    async fn get_block_subsidy(
        &self,
        height: Height,
    ) -> Result<rpc::BlockSubsidy, QueryError<GetBlockSubsidyError>> {
        self.rpc.get_block_subsidy(height).await
    }
}

impl OneShotGetNodeInfo for ZebraValidator {
    async fn get_node_info(&self) -> Result<rpc::NodeInfo, QueryError<GetNodeInfoError>> {
        self.rpc.get_node_info().await
    }
}

impl OneShotGetPeerInfo for ZebraValidator {
    async fn get_peer_info(&self) -> Result<Vec<rpc::PeerInfo>, QueryError<GetPeerInfoError>> {
        self.rpc.get_peer_info().await
    }
}

impl OneShotGetMiningInfo for ZebraValidator {
    async fn get_mining_info(&self) -> Result<rpc::MiningInfo, QueryError<GetMiningInfoError>> {
        self.rpc.get_mining_info().await
    }
}

impl OneShotGetNetworkSolPs for ZebraValidator {
    async fn get_network_sol_ps(
        &self,
        blocks: Option<u32>,
        height: Option<Height>,
    ) -> Result<u64, QueryError<GetNetworkSolPsError>> {
        self.rpc.get_network_sol_ps(blocks, height).await
    }
}

impl OneShotGetTxOut for ZebraValidator {
    async fn get_tx_out(
        &self,
        txid: TransactionId,
        index: OutputIndex,
        include_mempool: bool,
    ) -> Result<Option<rpc::TxOut>, QueryError<GetTxOutError>> {
        self.rpc.get_tx_out(txid, index, include_mempool).await
    }
}

impl OneShotGetSpentInfo for ZebraValidator {
    async fn get_spent_info(
        &self,
        outpoint: rpc::SpentOutpoint,
    ) -> Result<rpc::SpentInfo, QueryError<GetSpentInfoError>> {
        // `getspentinfo` reads a spent index zebrad does not expose; against
        // zebrad this answers `Unsupported`.
        self.rpc.get_spent_info(outpoint).await
    }
}

impl OneShotSendRawTransaction for ZebraValidator {
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

impl OneShotGetDifficulty for ZebraValidator {
    async fn get_difficulty(&self) -> Result<Difficulty, QueryError<GetDifficultyError>> {
        self.rpc.get_difficulty().await
    }
}

impl OneShotGetBlockchainInfo for ZebraValidator {
    async fn get_blockchain_info(
        &self,
    ) -> Result<BlockchainInfo, QueryError<GetBlockchainInfoError>> {
        self.rpc.get_blockchain_info().await
    }
}

// ---------------------------------------------------------------------------
// Subscriptions and lifecycle
// ---------------------------------------------------------------------------

impl SubscribeChainTip for ZebraValidator {
    fn subscribe_to_chain_tip(&self) -> Option<watch::Receiver<TipObservation>> {
        // Return the synthesised poller's receiver, or None when polling was
        // never started.
        self.tip
            .as_ref()
            .and_then(|tip| tip.subscribe_to_chain_tip())
    }
}

impl SubscribeBlocks for ZebraValidator {
    fn subscribe_to_blocks_received(&self) -> Option<watch::Receiver<()>> {
        // The transport does not push block arrivals; that signal belongs to the
        // syncer, which this source does not own.
        None
    }
}

impl SourceLifecycle for ZebraValidator {
    fn shutdown(&self) {
        self.rpc.shutdown();
    }
}
