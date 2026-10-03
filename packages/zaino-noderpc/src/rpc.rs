//! The JSON-RPC service surface (jsonrpsee), backed by the [`NodeRpc`] handler.
//!
//! JSON method names follow zcashd; the Rust method names differ from the
//! handler's inherent methods on purpose, so the impl body calls the inherent
//! method (`self.get_block_count()`) rather than recursing into the trait.

use jsonrpsee::proc_macros::rpc;
use jsonrpsee::types::{ErrorCode, ErrorObjectOwned};

use zaino_service::error::AddressReadError;
use zaino_service::error::MempoolReadError;
use zaino_service::error::TransactionViewError;
use zaino_service::error::TxReadError;
use zaino_service::NodeRpcService;
use zaino_service::NodeStatusError;

use crate::error::RpcError;
use crate::wire::params::{AddressDeltasParam, AddressesParam};
use crate::wire::response::{
    AddressBalanceResponse, AddressDeltasResponse, BlockHeaderResponse, BlockchainInfoResponse,
    GetBlockResponse, GetRawTransactionResponse, MempoolInfoResponse, MiningInfoResponse,
    NodeInfoResponse, PeerInfoEntry, RawMempoolResponse, UnifiedReceiversResponse,
    ValidateAddressResponse, ZValidateAddressResponse,
};
use crate::NodeRpc;

/// The node JSON-RPC surface this adapter serves.
#[rpc(server)]
pub(crate) trait NodeRpcApi {
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

    #[method(name = "getinfo")]
    async fn info(&self) -> Result<NodeInfoResponse, ErrorObjectOwned>;

    #[method(name = "getmininginfo")]
    async fn mining_info(&self) -> Result<MiningInfoResponse, ErrorObjectOwned>;

    #[method(name = "getpeerinfo")]
    async fn peer_info(&self) -> Result<Vec<PeerInfoEntry>, ErrorObjectOwned>;

    #[method(name = "getnetworksolps")]
    async fn network_sol_ps(
        &self,
        blocks: Option<u32>,
        height: Option<i64>,
    ) -> Result<u64, ErrorObjectOwned>;

    #[method(name = "getrawmempool")]
    async fn raw_mempool(
        &self,
        verbose: Option<bool>,
    ) -> Result<RawMempoolResponse, ErrorObjectOwned>;

    #[method(name = "getmempoolinfo")]
    async fn mempool_info(&self) -> Result<MempoolInfoResponse, ErrorObjectOwned>;

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
    async fn info(&self) -> Result<NodeInfoResponse, ErrorObjectOwned> {
        self.get_info().await.map_err(to_error_object)
    }
    async fn mining_info(&self) -> Result<MiningInfoResponse, ErrorObjectOwned> {
        self.get_mining_info().await.map_err(to_error_object)
    }
    async fn peer_info(&self) -> Result<Vec<PeerInfoEntry>, ErrorObjectOwned> {
        self.get_peer_info().await.map_err(to_error_object)
    }
    async fn network_sol_ps(
        &self,
        blocks: Option<u32>,
        height: Option<i64>,
    ) -> Result<u64, ErrorObjectOwned> {
        self.get_network_sol_ps(blocks, crate::wire::network_solps_height(height))
            .await
            .map_err(to_error_object)
    }
    async fn raw_mempool(
        &self,
        verbose: Option<bool>,
    ) -> Result<RawMempoolResponse, ErrorObjectOwned> {
        self.get_raw_mempool(verbose.unwrap_or(false))
            .await
            .map_err(to_error_object)
    }
    async fn mempool_info(&self) -> Result<MempoolInfoResponse, ErrorObjectOwned> {
        self.get_mempool_info().await.map_err(to_error_object)
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

/// The code zcashd and zebra report for an unknown block or transaction
/// (`RPC_INVALID_ADDRESS_OR_KEY`). zebra's message is "block height not in best
/// chain"; Zaino keeps its own accurate message and matches only the code.
const NOT_FOUND_CODE: i32 = -5;

/// Map a domain-side RPC error onto a JSON-RPC error object: invalid input is a
/// params error, an unknown object is `-5` (zcashd/zebra's not-found code), and
/// everything else an internal error carrying the reason.
fn to_error_object(err: RpcError) -> ErrorObjectOwned {
    let (code, message): (i32, String) = match err {
        RpcError::InvalidParams(m) => (ErrorCode::InvalidParams.code(), m),
        RpcError::Rejected(r) => (ErrorCode::InvalidParams.code(), r.to_string()),
        RpcError::NoBlocks => (
            ErrorCode::InternalError.code(),
            "no blocks available yet".to_string(),
        ),
        RpcError::Unavailable(t) => (ErrorCode::InternalError.code(), t.to_string()),
        RpcError::Read(e) => (ErrorCode::InternalError.code(), e.to_string()),
        RpcError::AddressRead(AddressReadError::Transient(cause)) => {
            (ErrorCode::InternalError.code(), cause)
        }
        RpcError::AddressRead(AddressReadError::Fatal(cause)) => {
            (ErrorCode::InternalError.code(), cause)
        }
        // The validator does not implement this address method (e.g.
        // getaddressdeltas against a zebra backend). zcashd and zebra report an
        // absent method as method-not-found (-32601), so Zaino does too — the
        // truthful code for the validator's gap, not an internal error that blames
        // the server. The message is the typed variant's own, not a stringified
        // cause.
        RpcError::AddressRead(e @ AddressReadError::Unsupported(_)) => {
            (ErrorCode::MethodNotFound.code(), e.to_string())
        }
        RpcError::AddressRead(e @ AddressReadError::NotServiceable(_)) => {
            (ErrorCode::InternalError.code(), e.to_string())
        }
        RpcError::NotFound(message) => (NOT_FOUND_CODE, message),
        RpcError::TxRead(TxReadError::Transient(cause)) => (ErrorCode::InternalError.code(), cause),
        RpcError::TxRead(TxReadError::Fatal(cause)) => (ErrorCode::InternalError.code(), cause),
        RpcError::TxRead(e @ TxReadError::NotServiceable(_)) => {
            (ErrorCode::InternalError.code(), e.to_string())
        }
        // Resolving a transaction's inputs is a server-side concern throughout:
        // `Unavailable` is a transport failure, `MissingPrevout` /
        // `PrevoutIndexOutOfRange` are inconsistencies in what the validator
        // served — the spending transaction without the output it spends — and
        // `PrevoutFanoutTooLarge` is a server policy refusal to fan a single
        // passthrough request out past its ceiling. None of these is bad client
        // input (the client asked for a well-formed, valid block or transaction),
        // so all map to the internal-error code. Each variant's own `Display` is
        // used (which does not stringify the `#[source]` cause), per variant,
        // rather than a blanket `to_string()` of the cause.
        RpcError::TransactionView(e @ TransactionViewError::Unavailable { .. }) => {
            (ErrorCode::InternalError.code(), e.to_string())
        }
        RpcError::TransactionView(e @ TransactionViewError::PrevoutFanoutTooLarge { .. }) => {
            (ErrorCode::InternalError.code(), e.to_string())
        }
        RpcError::TransactionView(e @ TransactionViewError::MissingPrevout { .. }) => {
            (ErrorCode::InternalError.code(), e.to_string())
        }
        RpcError::TransactionView(e @ TransactionViewError::PrevoutIndexOutOfRange { .. }) => {
            (ErrorCode::InternalError.code(), e.to_string())
        }
        // Both are internal, and that is Review Focus 1: `NotReady` means the
        // validator will answer shortly, and `Unreachable` means it cannot be
        // reached — neither is the caller's fault, and neither may render as a
        // success a warmer would cache. Each variant's own `Display` is used,
        // which does not stringify the `#[source]` cause.
        RpcError::NodeStatus(e @ NodeStatusError::NotReady) => {
            (ErrorCode::InternalError.code(), e.to_string())
        }
        RpcError::NodeStatus(e @ NodeStatusError::Unreachable { .. }) => {
            (ErrorCode::InternalError.code(), e.to_string())
        }
        // A mempool read is passthrough to the validator: none of its three
        // cases is bad client input, so each is an internal error. `Transient`
        // is the live one — a transport failure the explorer's warmer must not
        // cache as a success. Each variant's own `Display` is used, which does
        // not stringify a `#[source]` cause.
        RpcError::MempoolRead(e @ MempoolReadError::Transient(_)) => {
            (ErrorCode::InternalError.code(), e.to_string())
        }
        RpcError::MempoolRead(e @ MempoolReadError::Fatal(_)) => {
            (ErrorCode::InternalError.code(), e.to_string())
        }
        RpcError::MempoolRead(e @ MempoolReadError::NotServiceable(_)) => {
            (ErrorCode::InternalError.code(), e.to_string())
        }
    };
    ErrorObjectOwned::owned(code, message, None::<()>)
}

#[cfg(test)]
mod tests {
    use super::to_error_object;
    use crate::error::RpcError;
    use jsonrpsee::types::ErrorCode;
    use zaino_service::error::AddressReadError;
    use zaino_service::error::MempoolReadError;
    use zaino_service::error::TransactionViewError;
    use zaino_service::error::TxReadError;
    use zaino_service::Capability;
    use zaino_service::NodeStatusError;

    /// Review Focus 1 on the mempool path: a transient mempool read is a
    /// transport failure, a server-side concern, so it is an internal error —
    /// never a success the explorer's warmer would cache.
    #[test]
    fn transient_mempool_read_is_an_internal_error() {
        let obj = to_error_object(RpcError::MempoolRead(MempoolReadError::Transient(
            "validator unavailable".to_string(),
        )));
        assert_eq!(obj.code(), ErrorCode::InternalError.code());
    }

    /// An unknown block or transaction is reported with code -5, matching
    /// zcashd and zebra (`RPC_INVALID_ADDRESS_OR_KEY`), not the params code.
    #[test]
    fn not_found_maps_to_minus_five() {
        let obj = to_error_object(RpcError::NotFound("no block for 999999".to_string()));
        assert_eq!(obj.code(), -5);
        assert_eq!(obj.message(), "no block for 999999");
    }

    /// A validator that does not implement an address method (getaddressdeltas on
    /// a zebra backend) is reported as method-not-found (-32601) — the truthful
    /// code for the validator's gap — not an internal error.
    #[test]
    fn unsupported_address_read_maps_to_method_not_found() {
        let obj = to_error_object(RpcError::AddressRead(AddressReadError::Unsupported(
            "the validator does not implement this address method".to_string(),
        )));
        assert_eq!(obj.code(), ErrorCode::MethodNotFound.code());
    }

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

    /// R52: a `MissingPrevout` raised while serving `getblock 2` reaches the client
    /// as a JSON-RPC internal error, driven through the generated surface rather
    /// than asserted on `to_error_object` alone. The service mock scripts the
    /// resolved-block read to fail with `MissingPrevout` for a named outpoint, and
    /// the call must surface the internal-error code with the outpoint in the
    /// message — never a params error blaming the caller, and never a partial block.
    #[tokio::test]
    async fn getblock_missing_prevout_drives_a_json_rpc_internal_error() {
        use super::NodeRpcApiServer;
        use crate::NodeRpc;
        use jsonrpsee::core::params::ArrayParams;
        use jsonrpsee::core::server::MethodsError;
        use zaino_primitives::types::{TransactionId, TransparentInput};
        use zaino_service::testing::{MockChain, MockIndexerService};
        use zcash_protocol::consensus::Network;

        let module = NodeRpc::new(
            MockIndexerService::new(MockChain {
                block_transaction_views_missing_prevout: Some(TransparentInput {
                    prev_txid: TransactionId::from([0x01; 32]),
                    prev_index: 0,
                }),
                ..MockChain::default()
            }),
            Network::MainNetwork,
        )
        .into_rpc();

        let mut params = ArrayParams::new();
        params.insert("2468").expect("block id param");
        params.insert(2u32).expect("verbosity param");
        let err = module
            .call::<_, serde_json::Value>("getblock", params)
            .await
            .expect_err("a missing prevout must fail the call, not render a partial block");
        match err {
            MethodsError::JsonRpc(obj) => {
                assert_eq!(obj.code(), ErrorCode::InternalError.code());
                assert!(
                    obj.message().contains("unknown to the validator"),
                    "the message names the inconsistency: {}",
                    obj.message()
                );
            }
            other => panic!("expected a JSON-RPC internal error, got {other:?}"),
        }
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

    /// An over-large prevout fan-out is a server policy refusal, not bad client
    /// input: the caller asked for a well-formed, valid block, and cannot
    /// reformulate the request to need fewer prevout fetches — the block genuinely
    /// spends that many distinct outputs. So it maps to the internal-error code
    /// (the same class as the other server-side resolution failures), never
    /// invalid-params, which would wrongly blame the caller for a limit Zaino
    /// imposes during the passthrough phase. The message names both the needed
    /// count and the ceiling, via the variant's own `Display`.
    #[test]
    fn prevout_fanout_too_large_is_an_internal_error() {
        let obj = to_error_object(RpcError::TransactionView(
            TransactionViewError::PrevoutFanoutTooLarge {
                needed: 9000,
                ceiling: 8192,
            },
        ));
        assert_eq!(obj.code(), ErrorCode::InternalError.code());
        assert!(
            obj.message().contains("9000") && obj.message().contains("8192"),
            "the message names the needed count and the ceiling: {}",
            obj.message()
        );
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

    /// A node-status failure must render as an internal error, never as a
    /// success with default values: the explorer's warmers cache successes and
    /// ignore errors, so a defaulted `Ok` would poison their cache for 15
    /// seconds while an error leaves the previous value intact.
    #[test]
    fn node_status_failures_render_as_internal_errors() {
        for err in [
            RpcError::NodeStatus(NodeStatusError::NotReady),
            RpcError::NodeStatus(NodeStatusError::unreachable(std::io::Error::other(
                "unreachable",
            ))),
        ] {
            assert_eq!(to_error_object(err).code(), ErrorCode::InternalError.code());
        }
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
