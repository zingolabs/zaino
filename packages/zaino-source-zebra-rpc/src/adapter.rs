//! Trait implementations: zaino-source query traits on [`ZebraRpcAdapter`].

use zaino_primitives::types::{BlockHash, Height, Treestate};
use zaino_rpc::RpcClient;
use zaino_source::{
    FailureMode, GetBlockBytesError, GetChainTipError, GetTreestateError, QueryError,
    FetchError,
};

use crate::parse;

/// Zebra JSON-RPC adapter.
///
/// Implements zaino-source query traits by delegating to an [`RpcClient`]
/// and parsing Zebra's response format. Single-attempt — wrap with
/// [`zaino_source::Resilient`] for retries.
pub struct ZebraRpcAdapter {
    rpc: RpcClient,
}

impl ZebraRpcAdapter {
    /// Wrap an existing [`RpcClient`].
    pub fn new(rpc: RpcClient) -> Self {
        Self { rpc }
    }
}

/// Parse errors are always non-retryable — the response arrived but was malformed.
fn from_parse(e: crate::parse::ParseError) -> FetchError {
    FetchError::new(FailureMode::Parse, e.to_string())
}

impl zaino_source::GetBlockBytes for ZebraRpcAdapter {
    async fn get_block_bytes(
        &self,
        height: Height,
    ) -> Result<Vec<u8>, QueryError<GetBlockBytesError>> {
        let params = vec![
            serde_json::Value::String(u32::from(height).to_string()),
            serde_json::Value::Number(0.into()),
        ];
        // RpcError → FetchError via From impl in zaino-rpc
        let value = self
            .rpc
            .call("getblock", params)
            .await
            .map_err(|e| QueryError::Fetch(e.into()))?;
        parse::parse_raw_block(&value).map_err(|e| from_parse(e).into())
    }
}

impl zaino_source::GetChainTip for ZebraRpcAdapter {
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
