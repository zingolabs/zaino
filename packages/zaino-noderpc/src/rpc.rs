//! The JSON-RPC service surface (jsonrpsee), backed by the [`NodeRpc`] handler.
//!
//! JSON method names follow zcashd; the Rust method names differ from the
//! handler's inherent methods on purpose, so the impl body calls the inherent
//! method (`self.get_block_count()`) rather than recursing into the trait.

use jsonrpsee::proc_macros::rpc;
use jsonrpsee::types::{ErrorCode, ErrorObjectOwned};

use zaino_service::error::AddressReadError;
use zaino_service::error::TxReadError;
use zaino_service::NodeRpcService;

use crate::error::RpcError;
use crate::wire::params::{AddressDeltasParam, AddressesParam};
use crate::wire::response::{
    AddressBalanceResponse, AddressDeltasResponse, UnifiedReceiversResponse,
    ValidateAddressResponse, ZValidateAddressResponse,
};
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

    #[method(name = "getrawtransaction")]
    async fn raw_transaction(
        &self,
        txid: String,
        verbosity: Option<u32>,
    ) -> Result<String, ErrorObjectOwned>;

    #[method(name = "sendrawtransaction")]
    async fn send_raw(&self, hex: String) -> Result<String, ErrorObjectOwned>;

    #[method(name = "getblockchaininfo")]
    async fn blockchain_info(&self) -> Result<String, ErrorObjectOwned>;

    #[method(name = "getmininginfo")]
    async fn mining_info(&self) -> Result<String, ErrorObjectOwned>;

    #[method(name = "getaddressbalance")]
    async fn address_balance(
        &self,
        params: AddressesParam,
    ) -> Result<AddressBalanceResponse, ErrorObjectOwned>;

    #[method(name = "getaddressdeltas")]
    async fn address_deltas(
        &self,
        params: AddressDeltasParam,
    ) -> Result<AddressDeltasResponse, ErrorObjectOwned>;

    #[method(name = "validateaddress")]
    async fn validate_addr(
        &self,
        address: String,
    ) -> Result<ValidateAddressResponse, ErrorObjectOwned>;

    #[method(name = "z_validateaddress")]
    async fn z_validate_addr(
        &self,
        address: String,
    ) -> Result<ZValidateAddressResponse, ErrorObjectOwned>;

    #[method(name = "z_listunifiedreceivers")]
    async fn z_list_receivers(
        &self,
        address: String,
    ) -> Result<UnifiedReceiversResponse, ErrorObjectOwned>;
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
    async fn raw_transaction(
        &self,
        txid: String,
        verbosity: Option<u32>,
    ) -> Result<String, ErrorObjectOwned> {
        self.get_raw_transaction(&txid, verbosity)
            .await
            .map_err(to_error_object)
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
    async fn address_balance(
        &self,
        params: AddressesParam,
    ) -> Result<AddressBalanceResponse, ErrorObjectOwned> {
        self.get_address_balance(params)
            .await
            .map_err(to_error_object)
    }
    async fn address_deltas(
        &self,
        params: AddressDeltasParam,
    ) -> Result<AddressDeltasResponse, ErrorObjectOwned> {
        self.get_address_deltas(params)
            .await
            .map_err(to_error_object)
    }
    async fn validate_addr(
        &self,
        address: String,
    ) -> Result<ValidateAddressResponse, ErrorObjectOwned> {
        self.validate_address(&address)
            .await
            .map_err(to_error_object)
    }
    async fn z_validate_addr(
        &self,
        address: String,
    ) -> Result<ZValidateAddressResponse, ErrorObjectOwned> {
        self.z_validate_address(&address)
            .await
            .map_err(to_error_object)
    }
    async fn z_list_receivers(
        &self,
        address: String,
    ) -> Result<UnifiedReceiversResponse, ErrorObjectOwned> {
        self.z_list_unified_receivers(&address)
            .await
            .map_err(to_error_object)
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
        RpcError::AddressRead(AddressReadError::Transient(cause)) => {
            (ErrorCode::InternalError, cause)
        }
        RpcError::AddressRead(AddressReadError::Fatal(cause)) => (ErrorCode::InternalError, cause),
        RpcError::AddressRead(e @ AddressReadError::NotServiceable(_)) => {
            (ErrorCode::InternalError, e.to_string())
        }
        RpcError::NotFound(message) => (ErrorCode::InvalidParams, message),
        RpcError::TxRead(TxReadError::Transient(cause)) => (ErrorCode::InternalError, cause),
        RpcError::TxRead(TxReadError::Fatal(cause)) => (ErrorCode::InternalError, cause),
        RpcError::TxRead(e @ TxReadError::NotServiceable(_)) => {
            (ErrorCode::InternalError, e.to_string())
        }
    };
    ErrorObjectOwned::owned(code.code(), message, None::<()>)
}

#[cfg(test)]
mod tests {
    use super::to_error_object;
    use crate::error::RpcError;
    use jsonrpsee::types::ErrorCode;
    use zaino_service::error::AddressReadError;
    use zaino_service::error::TxReadError;
    use zaino_service::Capability;

    /// A fatal address read is an unrecoverable backend failure — a server
    /// fault, not bad client input — so it maps to the internal-error code.
    /// Fails if the arm is remapped to invalid-params.
    #[test]
    fn fatal_address_read_is_an_internal_error() {
        let obj = to_error_object(RpcError::AddressRead(AddressReadError::Fatal(
            "backend failure".to_string(),
        )));
        assert_eq!(obj.code(), ErrorCode::InternalError.code());
    }

    /// A transient address read is a retryable server-side race, also internal.
    #[test]
    fn transient_address_read_is_an_internal_error() {
        let obj = to_error_object(RpcError::AddressRead(AddressReadError::Transient(
            "mid-swap race".to_string(),
        )));
        assert_eq!(obj.code(), ErrorCode::InternalError.code());
    }

    /// An unserviceable address read is a server-readiness fault, also internal.
    #[test]
    fn not_serviceable_address_read_is_an_internal_error() {
        let obj = to_error_object(RpcError::AddressRead(AddressReadError::NotServiceable(
            Capability::AddressHistory,
        )));
        assert_eq!(obj.code(), ErrorCode::InternalError.code());
    }

    /// A read failure is the server's fault, never the caller's. Only a
    /// malformed txid is a params error.
    #[test]
    fn tx_read_failures_render_as_internal_errors() {
        for err in [
            RpcError::TxRead(TxReadError::Transient("gone".into())),
            RpcError::TxRead(TxReadError::Fatal("broken".into())),
        ] {
            assert_eq!(to_error_object(err).code(), ErrorCode::InternalError.code());
        }
    }
}
