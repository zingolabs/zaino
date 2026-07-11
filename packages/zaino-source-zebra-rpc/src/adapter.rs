//! Trait implementations: zaino-source query traits on [`ZebraRpcAdapter`].

use zaino_primitives::types::{Block, BlockHash, Height, Treestate};
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
    async fn get_block(
        &self,
        height: Height,
    ) -> Result<Block, QueryError<GetBlockError>> {
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

        // TODO: fetch tree sizes from z_gettreestate or chain metadata.
        // For now, leave as 0 — tree size indexes will populate these
        // via GetTreestate separately.
        let sapling_tree_size = 0;
        let orchard_tree_size = 0;

        zaino_convert_zebra::block_from_zebra(zebra_block, sapling_tree_size, orchard_tree_size)
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
