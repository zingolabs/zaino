//! The JSON-RPC service surface (jsonrpsee), backed by the [`NodeRpc`] handler.
//!
//! JSON method names follow zcashd; the Rust method names differ from the
//! handler's inherent methods on purpose, so the impl body calls the inherent
//! method (`self.get_block_count()`) rather than recursing into the trait.

use jsonrpsee::proc_macros::rpc;
use jsonrpsee::types::{ErrorCode, ErrorObjectOwned};

use zaino_service::NodeRpcService;

use crate::error::RpcError;
use crate::NodeRpc;

/// The node JSON-RPC surface this adapter serves.
#[rpc(server)]
pub trait NodeRpcApi {
    #[method(name = "getblockcount")]
    async fn block_count(&self) -> Result<u32, ErrorObjectOwned>;

    #[method(name = "getbestblockhash")]
    async fn best_block_hash(&self) -> Result<String, ErrorObjectOwned>;

    #[method(name = "gettxout")]
    async fn tx_out(&self, txid: String, index: u32) -> Result<String, ErrorObjectOwned>;

    #[method(name = "sendrawtransaction")]
    async fn send_raw(&self, hex: String) -> Result<String, ErrorObjectOwned>;

    #[method(name = "getblockchaininfo")]
    async fn blockchain_info(&self) -> Result<String, ErrorObjectOwned>;

    #[method(name = "getmininginfo")]
    async fn mining_info(&self) -> Result<String, ErrorObjectOwned>;
}

#[jsonrpsee::core::async_trait]
impl<S: NodeRpcService + 'static> NodeRpcApiServer for NodeRpc<S> {
    async fn block_count(&self) -> Result<u32, ErrorObjectOwned> {
        self.get_block_count().await.map_err(to_error_object)
    }
    async fn best_block_hash(&self) -> Result<String, ErrorObjectOwned> {
        self.get_best_block_hash().await.map_err(to_error_object)
    }
    async fn tx_out(&self, txid: String, index: u32) -> Result<String, ErrorObjectOwned> {
        self.get_tx_out(&txid, index).await.map_err(to_error_object)
    }
    async fn send_raw(&self, hex: String) -> Result<String, ErrorObjectOwned> {
        self.send_raw_transaction(&hex)
            .await
            .map_err(to_error_object)
    }
    async fn blockchain_info(&self) -> Result<String, ErrorObjectOwned> {
        self.get_blockchain_info().await.map_err(to_error_object)
    }
    async fn mining_info(&self) -> Result<String, ErrorObjectOwned> {
        self.get_mining_info().await.map_err(to_error_object)
    }
}

/// Map a domain-side RPC error onto a JSON-RPC error object: invalid input is a
/// params error, everything else an internal error carrying the reason.
fn to_error_object(err: RpcError) -> ErrorObjectOwned {
    let (code, message) = match err {
        RpcError::InvalidParams(m) => (ErrorCode::InvalidParams, m),
        RpcError::Rejected(r) => (ErrorCode::InvalidParams, r.to_string()),
        RpcError::NoBlocks => (
            ErrorCode::InternalError,
            "no blocks available yet".to_string(),
        ),
        RpcError::Unavailable(t) => (ErrorCode::InternalError, t.to_string()),
        RpcError::SpendRead(e) => (ErrorCode::InternalError, e.to_string()),
        RpcError::Read(e) => (ErrorCode::InternalError, e.to_string()),
    };
    ErrorObjectOwned::owned(code.code(), message, None::<()>)
}
