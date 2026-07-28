//! Trait implementations: zaino-source query traits on [`ZebraRpcAdapter`].

use zaino_primitives::types::{
    Block, BlockHash, ChainMetadata, Height, TransactionHash, Treestate,
};
use zaino_rpc::RpcClient;
use zaino_source::{
    FailureMode, FetchError, GetBlockError, GetChainTipError, GetTreestateError, QueryError,
};
use zebra_chain::serialization::ZcashDeserializeInto;

use crate::parse;

/// Zebra JSON-RPC adapter.
///
/// Implements zaino-source query traits by delegating to an [`RpcClient`],
/// deserializing via `zebra-chain`, and converting to domain types.
/// Single-attempt — wrap with [`zaino_source::Resilient`] for retries.
pub struct ZebraRpcAdapter {
    rpc: RpcClient,
}

impl ZebraRpcAdapter {
    /// Wrap an existing [`RpcClient`].
    pub fn new(rpc: RpcClient) -> Self {
        Self { rpc }
    }
}

/// Parse errors are always non-retryable.
fn from_parse(e: parse::ParseError) -> FetchError {
    FetchError::new(FailureMode::Parse, e.to_string())
}

impl zaino_source::GetBlock for ZebraRpcAdapter {
    #[cfg_attr(feature = "tracing", tracing::instrument(skip(self), fields(h = u32::from(height))))]
    async fn get_block(&self, height: Height) -> Result<Block, QueryError<GetBlockError>> {
        // Fetch raw hex block via getblock(height, 0).
        let params = vec![
            serde_json::Value::String(u32::from(height).to_string()),
            serde_json::Value::Number(0.into()),
        ];
        let value = self
            .rpc
            .call("getblock", params)
            .await
            .map_err(|e| QueryError::Fetch(e.into()))?;

        // Hex decode.
        let raw_bytes = parse::parse_raw_block(&value).map_err(from_parse)?;

        // Deserialize via zebra-chain.
        let zebra_block: zebra_chain::block::Block = raw_bytes
            .zcash_deserialize_into()
            .map_err(|e| from_parse(parse::ParseError::Deserialize(e.to_string())))?;

        // Cumulative tree sizes are indexed state, not present in the block
        // bytes, so they are zero here and populated by the caller that tracks
        // them (via `GetTreestate` or its own index). Zero is a placeholder,
        // not a measurement: a consumer that needs real sizes must not read
        // them off this block.
        let chain_metadata = ChainMetadata {
            sapling_tree_size: 0,
            orchard_tree_size: 0,
            ironwood_tree_size: 0,
        };

        zaino_convert_zebra::block_from_zebra(&zebra_block, chain_metadata)
            .map_err(|e| FetchError::new(FailureMode::Parse, e.to_string()).into())
    }
}

impl zaino_source::GetChainTip for ZebraRpcAdapter {
    #[cfg_attr(feature = "tracing", tracing::instrument(skip(self)))]
    async fn get_chain_tip(&self) -> Result<(BlockHash, Height), QueryError<GetChainTipError>> {
        let hash_value = self
            .rpc
            .call("getbestblockhash", vec![])
            .await
            .map_err(|e| QueryError::Fetch(e.into()))?;
        let hash = parse::parse_block_hash(&hash_value).map_err(from_parse)?;

        let height_value = self
            .rpc
            .call("getblockcount", vec![])
            .await
            .map_err(|e| QueryError::Fetch(e.into()))?;
        let height = parse::parse_height(&height_value).map_err(from_parse)?;

        Ok((hash, height))
    }
}

impl zaino_source::GetPreIndexCompactBlock for ZebraRpcAdapter {
    #[cfg_attr(feature = "tracing", tracing::instrument(skip(self), fields(h = u32::from(height))))]
    async fn get_pre_index_compact_block(
        &self,
        height: Height,
    ) -> Result<zaino_primitives::types::PreIndexCompactBlock, QueryError<GetBlockError>> {
        // RPC returns full block bytes — no way to request compact from the validator.
        // We full-deserialize via zebra-chain then convert to our compact type.
        // The savings vs get_block is skipping the domain Block intermediate —
        // we go zebra Block → compact directly.
        //
        // TODO: once compact_deserialize supports streaming (Reader instead of
        // &[u8]), we can skip the full zebra deserialize on this path too.
        use zaino_source::GetBlock;
        let block = self.get_block(height).await?;
        Ok(zaino_primitives::types::PreIndexCompactBlock::from(&block))
    }
}

impl zaino_source::GetTreestate for ZebraRpcAdapter {
    #[cfg_attr(feature = "tracing", tracing::instrument(skip(self), fields(h = u32::from(height))))]
    async fn get_treestate(
        &self,
        height: Height,
    ) -> Result<Treestate, QueryError<GetTreestateError>> {
        let params = vec![serde_json::Value::String(u32::from(height).to_string())];
        let value = self
            .rpc
            .call("z_gettreestate", params)
            .await
            .map_err(|e| QueryError::Fetch(e.into()))?;
        parse::parse_treestate(&value).map_err(|e| from_parse(e).into())
    }
}

/// Build the positional params for a hash-addressed call.
fn hash_param(hash: BlockHash) -> Vec<serde_json::Value> {
    vec![serde_json::Value::String(hash_to_display_hex(hash))]
}

/// Render a block hash in RPC display order (big-endian hex).
fn hash_to_display_hex(hash: BlockHash) -> String {
    let mut bytes = <[u8; 32]>::from(hash);
    bytes.reverse();
    hex::encode(bytes)
}

/// Render a transaction id in RPC display order (big-endian hex).
fn txid_to_display_hex(txid: TransactionHash) -> String {
    let mut bytes = <[u8; 32]>::from(txid);
    bytes.reverse();
    hex::encode(bytes)
}

fn addresses_param(addresses: Vec<String>) -> serde_json::Value {
    serde_json::json!({ "addresses": addresses })
}

impl ZebraRpcAdapter {
    /// Issue a call and parse its result, mapping transport and parse failures
    /// into the caller's error type.
    async fn call_parsed<T, E>(
        &self,
        method: &str,
        params: Vec<serde_json::Value>,
        parse: impl FnOnce(&serde_json::Value) -> Result<T, parse::ParseError>,
    ) -> Result<T, QueryError<E>>
    where
        E: std::fmt::Debug + std::fmt::Display,
    {
        let value = self
            .rpc
            .call(method, params)
            .await
            .map_err(|e| QueryError::Fetch(e.into()))?;
        parse(&value).map_err(|e| QueryError::Fetch(from_parse(e)))
    }
}

impl zaino_source::GetBlockByHash for ZebraRpcAdapter {
    async fn get_block_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Block, QueryError<zaino_source::GetBlockByHashError>> {
        let params = vec![
            serde_json::Value::String(hash_to_display_hex(hash)),
            serde_json::Value::Number(0.into()),
        ];
        let raw_bytes: Vec<u8> = self
            .call_parsed("getblock", params, parse::parse_raw_block)
            .await?;

        let zebra_block: zebra_chain::block::Block = raw_bytes
            .zcash_deserialize_into()
            .map_err(|e| from_parse(parse::ParseError::Deserialize(e.to_string())))?;

        // Tree sizes are indexed state, not block data — see `GetBlock`.
        let chain_metadata = ChainMetadata {
            sapling_tree_size: 0,
            orchard_tree_size: 0,
            ironwood_tree_size: 0,
        };
        zaino_convert_zebra::block_from_zebra(&zebra_block, chain_metadata)
            .map_err(|e| FetchError::new(FailureMode::Parse, e.to_string()).into())
    }
}

impl zaino_source::GetBestBlockHeight for ZebraRpcAdapter {
    async fn get_best_block_height(
        &self,
    ) -> Result<Height, QueryError<zaino_source::GetBestBlockHeightError>> {
        self.call_parsed("getblockcount", vec![], parse::parse_height)
            .await
    }
}

impl zaino_source::GetBlockVerbose for ZebraRpcAdapter {
    async fn get_block_verbose(
        &self,
        height: Height,
    ) -> Result<zaino_primitives::types::BlockVerbose, QueryError<zaino_source::GetBlockVerboseError>>
    {
        let params = vec![
            serde_json::Value::String(u32::from(height).to_string()),
            serde_json::Value::Number(1.into()),
        ];
        self.call_parsed("getblock", params, parse::parse_block_verbose)
            .await
    }
}

impl zaino_source::GetBlockHeader for ZebraRpcAdapter {
    async fn get_block_header(
        &self,
        hash: BlockHash,
    ) -> Result<
        zaino_primitives::types::rpc::BlockHeaderVerbose,
        QueryError<zaino_source::GetBlockHeaderError>,
    > {
        let params = vec![
            serde_json::Value::String(hash_to_display_hex(hash)),
            serde_json::Value::Bool(true),
        ];
        self.call_parsed("getblockheader", params, parse::parse_block_header_verbose)
            .await
    }
}

impl zaino_source::GetRawBlockHeader for ZebraRpcAdapter {
    async fn get_raw_block_header(
        &self,
        hash: BlockHash,
    ) -> Result<Vec<u8>, QueryError<zaino_source::GetBlockHeaderError>> {
        let params = vec![
            serde_json::Value::String(hash_to_display_hex(hash)),
            serde_json::Value::Bool(false),
        ];
        self.call_parsed("getblockheader", params, parse::parse_raw_block)
            .await
    }
}

impl zaino_source::GetBlockDeltas for ZebraRpcAdapter {
    async fn get_block_deltas(
        &self,
        hash: BlockHash,
    ) -> Result<
        zaino_primitives::types::rpc::BlockDeltas,
        QueryError<zaino_source::GetBlockDeltasError>,
    > {
        self.call_parsed(
            "getblockdeltas",
            hash_param(hash),
            parse::parse_block_deltas,
        )
        .await
    }
}

impl zaino_source::GetChainTips for ZebraRpcAdapter {
    async fn get_chain_tips(
        &self,
    ) -> Result<
        Vec<zaino_primitives::types::rpc::ChainTip>,
        QueryError<zaino_source::GetChainTipsError>,
    > {
        self.call_parsed("getchaintips", vec![], parse::parse_chain_tips)
            .await
    }
}

impl zaino_source::GetDifficulty for ZebraRpcAdapter {
    async fn get_difficulty(
        &self,
    ) -> Result<zaino_primitives::types::Difficulty, QueryError<zaino_source::GetDifficultyError>>
    {
        self.call_parsed("getdifficulty", vec![], parse::as_f64)
            .await
    }
}

impl zaino_source::GetBlockchainInfo for ZebraRpcAdapter {
    async fn get_blockchain_info(
        &self,
    ) -> Result<
        zaino_primitives::types::BlockchainInfo,
        QueryError<zaino_source::GetBlockchainInfoError>,
    > {
        self.call_parsed("getblockchaininfo", vec![], parse::parse_blockchain_info)
            .await
    }
}

impl zaino_source::GetMempoolTxids for ZebraRpcAdapter {
    async fn get_mempool_txids(
        &self,
    ) -> Result<Vec<TransactionHash>, QueryError<zaino_source::GetMempoolTxidsError>> {
        self.call_parsed("getrawmempool", vec![], parse::parse_txids)
            .await
    }
}

impl zaino_source::GetAddressBalance for ZebraRpcAdapter {
    async fn get_address_balance(
        &self,
        addresses: Vec<String>,
    ) -> Result<
        zaino_primitives::types::AddressBalance,
        QueryError<zaino_source::GetAddressBalanceError>,
    > {
        self.call_parsed(
            "getaddressbalance",
            vec![addresses_param(addresses)],
            parse::parse_address_balance,
        )
        .await
    }
}

impl zaino_source::GetAddressDeltas for ZebraRpcAdapter {
    async fn get_address_deltas(
        &self,
        addresses: Vec<String>,
        start: Height,
        end: Height,
    ) -> Result<
        Vec<zaino_primitives::types::AddressDelta>,
        QueryError<zaino_source::GetAddressDeltasError>,
    > {
        let params = vec![serde_json::json!({
            "addresses": addresses,
            "start": u32::from(start),
            "end": u32::from(end),
        })];
        self.call_parsed("getaddressdeltas", params, parse::parse_address_deltas)
            .await
    }
}

impl zaino_source::GetAddressTxids for ZebraRpcAdapter {
    async fn get_address_txids(
        &self,
        addresses: Vec<String>,
        start: Height,
        end: Height,
    ) -> Result<Vec<TransactionHash>, QueryError<zaino_source::GetAddressTxidsError>> {
        let params = vec![serde_json::json!({
            "addresses": addresses,
            "start": u32::from(start),
            "end": u32::from(end),
        })];
        self.call_parsed("getaddresstxids", params, parse::parse_txids)
            .await
    }
}

impl zaino_source::GetAddressUtxos for ZebraRpcAdapter {
    async fn get_address_utxos(
        &self,
        addresses: Vec<String>,
    ) -> Result<Vec<zaino_primitives::types::Utxo>, QueryError<zaino_source::GetAddressUtxosError>>
    {
        self.call_parsed(
            "getaddressutxos",
            vec![addresses_param(addresses)],
            parse::parse_address_utxos,
        )
        .await
    }
}

impl zaino_source::GetTreestateByHash for ZebraRpcAdapter {
    async fn get_treestate_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Treestate, QueryError<zaino_source::GetTreestateByHashError>> {
        self.call_parsed("z_gettreestate", hash_param(hash), parse::parse_treestate)
            .await
    }
}

impl zaino_source::GetCommitmentTreeRoots for ZebraRpcAdapter {
    async fn get_commitment_tree_roots(
        &self,
        block: BlockHash,
    ) -> Result<
        zaino_primitives::types::TreeRoots,
        QueryError<zaino_source::GetCommitmentTreeRootsError>,
    > {
        self.call_parsed("z_gettreestate", hash_param(block), parse::parse_tree_roots)
            .await
    }
}

impl zaino_source::GetSubtreeRoots for ZebraRpcAdapter {
    async fn get_subtree_roots(
        &self,
        pool: zaino_primitives::types::ShieldedPool,
        start_index: u16,
        limit: Option<u16>,
    ) -> Result<
        Vec<zaino_primitives::types::SubtreeRoot>,
        QueryError<zaino_source::GetSubtreeRootsError>,
    > {
        let mut params = vec![
            serde_json::Value::String(pool.to_string()),
            serde_json::Value::Number(start_index.into()),
        ];
        // Omit the limit rather than sending a sentinel: the validator's own
        // default applies when the argument is absent.
        if let Some(limit) = limit {
            params.push(serde_json::Value::Number(limit.into()));
        }
        self.call_parsed("z_getsubtreesbyindex", params, parse::parse_subtree_roots)
            .await
    }
}

impl zaino_source::GetSpentInfo for ZebraRpcAdapter {
    async fn get_spent_info(
        &self,
        outpoint: zaino_primitives::types::rpc::SpentOutpoint,
    ) -> Result<
        Option<zaino_primitives::types::rpc::SpentInfo>,
        QueryError<zaino_source::GetSpentInfoError>,
    > {
        let params = vec![serde_json::json!({
            "txid": txid_to_display_hex(outpoint.txid),
            "index": outpoint.index,
        })];
        self.call_parsed("getspentinfo", params, parse::parse_spent_info)
            .await
    }
}

impl zaino_source::GetTxOut for ZebraRpcAdapter {
    async fn get_tx_out(
        &self,
        txid: TransactionHash,
        index: zaino_primitives::types::OutputIndex,
        include_mempool: bool,
    ) -> Result<Option<zaino_primitives::types::rpc::TxOut>, QueryError<zaino_source::GetTxOutError>>
    {
        let params = vec![
            serde_json::Value::String(txid_to_display_hex(txid)),
            serde_json::Value::Number(index.into()),
            serde_json::Value::Bool(include_mempool),
        ];
        self.call_parsed("gettxout", params, parse::parse_tx_out)
            .await
    }
}

impl zaino_source::SendRawTransaction for ZebraRpcAdapter {
    async fn send_raw_transaction(
        &self,
        transaction: Vec<u8>,
    ) -> Result<TransactionHash, QueryError<zaino_source::SendRawTransactionError>> {
        let params = vec![serde_json::Value::String(hex::encode(transaction))];
        self.call_parsed("sendrawtransaction", params, parse::as_txid)
            .await
    }
}

impl zaino_source::GetNodeInfo for ZebraRpcAdapter {
    async fn get_node_info(
        &self,
    ) -> Result<zaino_primitives::types::rpc::NodeInfo, QueryError<zaino_source::GetNodeInfoError>>
    {
        self.call_parsed("getinfo", vec![], parse::parse_node_info)
            .await
    }
}

impl zaino_source::GetPeerInfo for ZebraRpcAdapter {
    async fn get_peer_info(
        &self,
    ) -> Result<
        Vec<zaino_primitives::types::rpc::PeerInfo>,
        QueryError<zaino_source::GetPeerInfoError>,
    > {
        self.call_parsed("getpeerinfo", vec![], parse::parse_peer_info)
            .await
    }
}

impl zaino_source::GetMiningInfo for ZebraRpcAdapter {
    async fn get_mining_info(
        &self,
    ) -> Result<
        zaino_primitives::types::rpc::MiningInfo,
        QueryError<zaino_source::GetMiningInfoError>,
    > {
        self.call_parsed("getmininginfo", vec![], parse::parse_mining_info)
            .await
    }
}

impl zaino_source::GetBlockSubsidy for ZebraRpcAdapter {
    async fn get_block_subsidy(
        &self,
        height: Height,
    ) -> Result<
        zaino_primitives::types::rpc::BlockSubsidy,
        QueryError<zaino_source::GetBlockSubsidyError>,
    > {
        let params = vec![serde_json::Value::Number(u32::from(height).into())];
        self.call_parsed("getblocksubsidy", params, parse::parse_block_subsidy)
            .await
    }
}

impl zaino_source::GetNetworkSolPs for ZebraRpcAdapter {
    async fn get_network_sol_ps(
        &self,
        blocks: Option<u32>,
        height: Option<Height>,
    ) -> Result<u64, QueryError<zaino_source::GetNetworkSolPsError>> {
        // Both arguments are positional, so a height cannot be sent without a
        // window before it; the validator's own default window fills the gap.
        let mut params = Vec::new();
        if blocks.is_some() || height.is_some() {
            params.push(serde_json::Value::Number(blocks.unwrap_or(120).into()));
        }
        if let Some(height) = height {
            params.push(serde_json::Value::Number(u32::from(height).into()));
        }
        self.call_parsed("getnetworksolps", params, parse::as_u64)
            .await
    }
}

/// The RPC adapter owns no background work: it holds a connection pool that
/// drops with it, so there is nothing to release.
impl zaino_source::SourceLifecycle for ZebraRpcAdapter {}

/// Reaching the validator over request/response gives no push path, so this
/// adapter has no block-arrival signal to offer and inherits `None`. Consumers
/// pace themselves on their own timer.
impl zaino_source::SubscribeBlocks for ZebraRpcAdapter {}

impl zaino_source::GetTransaction for ZebraRpcAdapter {
    async fn get_transaction(
        &self,
        txid: TransactionHash,
    ) -> Result<zaino_source::TransactionResponse, QueryError<zaino_source::GetTransactionError>>
    {
        // Verbosity 1: the raw hex plus the height needed to place the
        // transaction. Verbosity 0 would return the bytes alone, leaving the
        // caller unable to tell a mined transaction from a mempool one.
        let params = vec![
            serde_json::Value::String(txid_to_display_hex(txid)),
            serde_json::Value::Number(1.into()),
        ];
        self.call_parsed("getrawtransaction", params, parse::parse_transaction)
            .await
    }
}
