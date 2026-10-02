//! The JSON-RPC service surface (jsonrpsee), backed by the [`NodeRpc`] handler.
//!
//! JSON method names follow zcashd; the Rust method names differ from the
//! handler's inherent methods on purpose, so the impl body calls the inherent
//! method (`self.get_block_count()`) rather than recursing into the trait.

use jsonrpsee::proc_macros::rpc;
use jsonrpsee::types::{ErrorCode, ErrorObjectOwned};

use zaino_service::error::AddressReadError;
use zaino_service::error::TransactionViewError;
use zaino_service::error::TxReadError;
use zaino_service::NodeRpcService;

use crate::error::RpcError;
use crate::wire::params::{AddressDeltasParam, AddressesParam};
use crate::wire::response::{
    AddressBalanceResponse, AddressDeltasResponse, BlockHeaderResponse, BlockchainInfoResponse,
    GetBlockResponse, GetRawTransactionResponse, UnifiedReceiversResponse, ValidateAddressResponse,
    ZValidateAddressResponse,
};
use crate::NodeRpc;

/// The node JSON-RPC surface this adapter serves.
#[rpc(server)]
pub trait NodeRpcApi {
    #[method(name = "getblockcount")]
    async fn block_count(&self) -> Result<u32, ErrorObjectOwned>;

    #[method(name = "getbestblockhash")]
    async fn best_block_hash(&self) -> Result<String, ErrorObjectOwned>;

    #[method(name = "getrawtransaction")]
    async fn raw_transaction(
        &self,
        txid: String,
        verbosity: Option<u32>,
    ) -> Result<GetRawTransactionResponse, ErrorObjectOwned>;

    #[method(name = "sendrawtransaction")]
    async fn send_raw(&self, hex: String) -> Result<String, ErrorObjectOwned>;

    #[method(name = "getblock")]
    async fn block(
        &self,
        blockid: String,
        verbosity: Option<u32>,
    ) -> Result<GetBlockResponse, ErrorObjectOwned>;

    #[method(name = "getblockheader")]
    async fn block_header(&self, hash: String) -> Result<BlockHeaderResponse, ErrorObjectOwned>;

    #[method(name = "getblockchaininfo")]
    async fn blockchain_info(&self) -> Result<BlockchainInfoResponse, ErrorObjectOwned>;

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
    async fn raw_transaction(
        &self,
        txid: String,
        verbosity: Option<u32>,
    ) -> Result<GetRawTransactionResponse, ErrorObjectOwned> {
        self.get_raw_transaction(&txid, verbosity)
            .await
            .map_err(to_error_object)
    }
    async fn send_raw(&self, hex: String) -> Result<String, ErrorObjectOwned> {
        self.send_raw_transaction(&hex)
            .await
            .map_err(to_error_object)
    }
    async fn block(
        &self,
        blockid: String,
        verbosity: Option<u32>,
    ) -> Result<GetBlockResponse, ErrorObjectOwned> {
        self.get_block(&blockid, verbosity)
            .await
            .map_err(to_error_object)
    }
    async fn block_header(&self, hash: String) -> Result<BlockHeaderResponse, ErrorObjectOwned> {
        self.get_block_header(&hash).await.map_err(to_error_object)
    }
    async fn blockchain_info(&self) -> Result<BlockchainInfoResponse, ErrorObjectOwned> {
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
        // Resolving a transaction's inputs is a server-side concern throughout:
        // `Unavailable` is a transport failure, and `MissingPrevout` /
        // `PrevoutIndexOutOfRange` are inconsistencies in what the validator
        // served — the spending transaction without the output it spends. None of
        // these is bad client input, so all map to the internal-error code. Each
        // variant's own `Display` is used (which does not stringify the `#[source]`
        // cause), per variant, rather than a blanket `to_string()` of the cause.
        RpcError::TransactionView(e @ TransactionViewError::Unavailable { .. }) => {
            (ErrorCode::InternalError, e.to_string())
        }
        RpcError::TransactionView(e @ TransactionViewError::MissingPrevout { .. }) => {
            (ErrorCode::InternalError, e.to_string())
        }
        RpcError::TransactionView(e @ TransactionViewError::PrevoutIndexOutOfRange { .. }) => {
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
    use zaino_service::error::TransactionViewError;
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

    /// Review Focus 2: an unreachable validator behind `getblockchaininfo`
    /// surfaces as an internal error, never a response with defaulted fields.
    /// The chain-info read fails with a `ReadError`, which lifts into
    /// `RpcError::Read` and must map to the internal-error code. Fails if the arm
    /// is remapped or dropped.
    #[test]
    fn chain_info_read_failure_is_an_internal_error() {
        use zaino_service::error::ReadError;
        let obj = to_error_object(RpcError::Read(ReadError::Fatal(
            "validator unreachable".to_string(),
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

    /// A missing prevout is a source inconsistency — the validator served a
    /// spending transaction but not the output it spends — not bad client input.
    /// It is the server's fault, so it maps to the internal-error code, never
    /// invalid-params (which would blame the caller for the validator's gap). The
    /// outpoint is named in the message via the variant's own `Display`, without
    /// stringifying a `#[source]` cause.
    #[test]
    fn missing_prevout_is_an_internal_error() {
        use zaino_primitives::types::{TransactionId, TransparentInput};
        let outpoint = TransparentInput {
            prev_txid: TransactionId::from([0x01; 32]),
            prev_index: 7,
        };
        let obj = to_error_object(RpcError::TransactionView(
            TransactionViewError::MissingPrevout { outpoint },
        ));
        assert_eq!(obj.code(), ErrorCode::InternalError.code());
        assert!(
            obj.message().contains("unknown to the validator"),
            "the message names the inconsistency: {}",
            obj.message()
        );
    }

    /// An out-of-range prevout index is likewise a source inconsistency, internal.
    #[test]
    fn prevout_index_out_of_range_is_an_internal_error() {
        use zaino_primitives::types::{TransactionId, TransparentInput};
        let outpoint = TransparentInput {
            prev_txid: TransactionId::from([0x02; 32]),
            prev_index: 9,
        };
        let obj = to_error_object(RpcError::TransactionView(
            TransactionViewError::PrevoutIndexOutOfRange {
                outpoint,
                index: 9,
                outputs: 2,
            },
        ));
        assert_eq!(obj.code(), ErrorCode::InternalError.code());
    }

    /// An unavailable validator while resolving inputs is a transport failure,
    /// also internal — the same class as the other read transport failures.
    #[test]
    fn transaction_view_unavailable_is_an_internal_error() {
        let obj = to_error_object(RpcError::TransactionView(
            TransactionViewError::Unavailable {
                cause: "connection reset".into(),
            },
        ));
        assert_eq!(obj.code(), ErrorCode::InternalError.code());
    }

    /// `gettxout` is no longer served: the generated surface answers it with
    /// JSON-RPC method-not-found rather than an invented spend-status string.
    /// Fails if the method is ever re-registered on this adapter — a correct
    /// object-shaped rendering is a later task, and a plausible-looking wrong
    /// answer is worse than method-not-found.
    #[tokio::test]
    async fn gettxout_is_method_not_found() {
        use super::NodeRpcApiServer;
        use crate::NodeRpc;
        use jsonrpsee::core::params::ArrayParams;
        use jsonrpsee::core::server::MethodsError;
        use zaino_service::testing::{MockChain, MockIndexerService};
        use zcash_protocol::consensus::Network;

        let module = NodeRpc::new(
            MockIndexerService::new(MockChain::default()),
            Network::MainNetwork,
        )
        .into_rpc();

        // Params are irrelevant: method lookup fails before they are read.
        let err = module
            .call::<_, serde_json::Value>("gettxout", ArrayParams::new())
            .await
            .expect_err("gettxout is no longer a served method");
        match err {
            MethodsError::JsonRpc(obj) => {
                assert_eq!(obj.code(), ErrorCode::MethodNotFound.code());
            }
            other => panic!("expected a method-not-found JSON-RPC error, got {other:?}"),
        }
    }
}
