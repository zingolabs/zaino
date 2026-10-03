//! The JSON-RPC service surface (jsonrpsee), backed by the [`NodeRpc`] handler.
//!
//! JSON method names follow zcashd; the Rust method names differ from the
//! handler's inherent methods on purpose, so the impl body calls the inherent
//! method (`self.get_block_count()`) rather than recursing into the trait.

use jsonrpsee::proc_macros::rpc;
use jsonrpsee::types::{ErrorCode, ErrorObjectOwned};

use zaino_service::error::AddressReadError;
use zaino_service::error::BlockDeltasError;
use zaino_service::error::BlockHashReadError;
use zaino_service::error::MempoolReadError;
use zaino_service::error::ReadError;
use zaino_service::error::TransactionViewError;
use zaino_service::error::TreestateReadError;
use zaino_service::error::TxReadError;
use zaino_service::NodeRpcService;
use zaino_service::NodeStatusError;

use crate::error::RpcError;
use crate::wire::params::{
    AddressDeltasParam, AddressTxidsParam, AddressesParam, GetBlockHashesOptions,
};
use crate::wire::response::{
    AddressBalanceResponse, AddressDeltasResponse, AddressUtxoEntry, BlockHeaderResponse,
    BlockchainInfoResponse, GetBlockDeltasResponse, GetBlockHashesResponse, GetBlockResponse,
    GetRawTransactionResponse, MempoolInfoResponse, MiningInfoResponse, NetworkInfoResponse,
    NodeInfoResponse, PeerInfoEntry, RawMempoolResponse, SubtreeRootsResponse, TreestateResponse,
    TxOutResponse, UnifiedReceiversResponse, ValidateAddressResponse, ZValidateAddressResponse,
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

    #[method(name = "getblockhashes")]
    async fn block_hashes(
        &self,
        high: u32,
        low: u32,
        options: Option<GetBlockHashesOptions>,
    ) -> Result<GetBlockHashesResponse, ErrorObjectOwned>;

    #[method(name = "getblockhash")]
    async fn block_hash(&self, height: u32) -> Result<String, ErrorObjectOwned>;

    #[method(name = "getblockdeltas")]
    async fn block_deltas(
        &self,
        blockhash: String,
    ) -> Result<GetBlockDeltasResponse, ErrorObjectOwned>;

    #[method(name = "gettxout")]
    async fn tx_out(
        &self,
        txid: String,
        n: u32,
        include_mempool: Option<bool>,
    ) -> Result<Option<TxOutResponse>, ErrorObjectOwned>;

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

    #[method(name = "getdifficulty")]
    async fn difficulty(&self) -> Result<f64, ErrorObjectOwned>;

    #[method(name = "getnetworkinfo")]
    async fn network_info(&self) -> Result<NetworkInfoResponse, ErrorObjectOwned>;

    #[method(name = "ping")]
    async fn ping(&self) -> Result<(), ErrorObjectOwned>;

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

    #[method(name = "getaddresstxids")]
    async fn address_txids(
        &self,
        params: AddressTxidsParam,
    ) -> Result<Vec<String>, ErrorObjectOwned>;

    #[method(name = "getaddressutxos")]
    async fn address_utxos(
        &self,
        params: AddressesParam,
    ) -> Result<Vec<AddressUtxoEntry>, ErrorObjectOwned>;

    #[method(name = "z_gettreestate")]
    async fn z_treestate(
        &self,
        hash_or_height: String,
    ) -> Result<TreestateResponse, ErrorObjectOwned>;

    #[method(name = "z_getsubtreesbyindex")]
    async fn z_subtrees_by_index(
        &self,
        pool: String,
        start_index: u16,
        limit: Option<u16>,
    ) -> Result<SubtreeRootsResponse, ErrorObjectOwned>;

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
    async fn block_hashes(
        &self,
        high: u32,
        low: u32,
        options: Option<GetBlockHashesOptions>,
    ) -> Result<GetBlockHashesResponse, ErrorObjectOwned> {
        self.get_block_hashes(high, low, options)
            .await
            .map_err(to_error_object)
    }
    async fn blockchain_info(&self) -> Result<BlockchainInfoResponse, ErrorObjectOwned> {
        self.get_blockchain_info().await.map_err(to_error_object)
    }
    async fn block_hash(&self, height: u32) -> Result<String, ErrorObjectOwned> {
        self.get_block_hash(height).await.map_err(to_error_object)
    }
    async fn block_deltas(
        &self,
        blockhash: String,
    ) -> Result<GetBlockDeltasResponse, ErrorObjectOwned> {
        self.get_block_deltas(&blockhash)
            .await
            .map_err(to_error_object)
    }
    async fn tx_out(
        &self,
        txid: String,
        n: u32,
        include_mempool: Option<bool>,
    ) -> Result<Option<TxOutResponse>, ErrorObjectOwned> {
        self.get_tx_out(&txid, n, include_mempool)
            .await
            .map_err(to_error_object)
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
    async fn difficulty(&self) -> Result<f64, ErrorObjectOwned> {
        self.get_difficulty().await.map_err(to_error_object)
    }
    async fn network_info(&self) -> Result<NetworkInfoResponse, ErrorObjectOwned> {
        self.get_network_info().await.map_err(to_error_object)
    }
    async fn ping(&self) -> Result<(), ErrorObjectOwned> {
        self.get_ping().await.map_err(to_error_object)
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
    async fn address_txids(
        &self,
        params: AddressTxidsParam,
    ) -> Result<Vec<String>, ErrorObjectOwned> {
        self.get_address_txids(params)
            .await
            .map_err(to_error_object)
    }
    async fn address_utxos(
        &self,
        params: AddressesParam,
    ) -> Result<Vec<AddressUtxoEntry>, ErrorObjectOwned> {
        self.get_address_utxos(params)
            .await
            .map_err(to_error_object)
    }
    async fn z_treestate(
        &self,
        hash_or_height: String,
    ) -> Result<TreestateResponse, ErrorObjectOwned> {
        self.get_treestate(&hash_or_height)
            .await
            .map_err(to_error_object)
    }
    async fn z_subtrees_by_index(
        &self,
        pool: String,
        start_index: u16,
        limit: Option<u16>,
    ) -> Result<SubtreeRootsResponse, ErrorObjectOwned> {
        self.get_subtrees_by_index(&pool, start_index, limit)
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

/// zcashd's `getblockhash` code for a height beyond the chain
/// (`RPC_INVALID_PARAMETER`), with its message "Block height out of range". A
/// distinct code from the not-found `-5`, matching zcashd/zebra.
const OUT_OF_RANGE_CODE: i32 = -8;

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
        RpcError::OutOfRange(message) => (OUT_OF_RANGE_CODE, message),
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
        // The `getblockhashes` timestamp-range search fails only server-side, never
        // on client input: `MissingHeader` is a chain-view inconsistency (a header
        // the search needed at or below the pinned tip was absent), and `TierRead`
        // wraps a tier-read failure. Both are internal errors. `MissingHeader`
        // renders its own `Display` (which names the height, not a `#[source]`
        // cause); `TierRead` follows the mapping every other `BlockReadError`-backed
        // read uses — lifted into a `ReadError` and rendered by that type's own
        // `Display`, not by stringifying the kept `#[source]` cause.
        RpcError::BlockHashRead(e @ BlockHashReadError::MissingHeader { .. }) => {
            (ErrorCode::InternalError.code(), e.to_string())
        }
        RpcError::BlockHashRead(BlockHashReadError::TierRead { source }) => (
            ErrorCode::InternalError.code(),
            ReadError::from(source).to_string(),
        ),
        // A treestate read is passthrough to the validator: none of its three
        // cases is bad client input (a malformed height/hash is rejected at the
        // wire boundary, and an unknown block is a `NotFound` above), so each is
        // an internal error. Each variant's own `Display` names the reason without
        // stringifying a `#[source]` cause (these are the known String-typed read
        // errors, which carry their reason inline).
        RpcError::Treestate(e @ TreestateReadError::NotServiceable(_)) => {
            (ErrorCode::InternalError.code(), e.to_string())
        }
        RpcError::Treestate(e @ TreestateReadError::Transient(_)) => {
            (ErrorCode::InternalError.code(), e.to_string())
        }
        RpcError::Treestate(e @ TreestateReadError::Fatal(_)) => {
            (ErrorCode::InternalError.code(), e.to_string())
        }
        // Composing `getblockdeltas` fails only server-side, never on client input
        // (an unknown block is the `NotFound` above): `TransactionView` is a
        // resolution failure, `Block` a read failure, `MissingHeader` a chain-view
        // hole in the median-time window, and `InputValueOutOfRange` a corrupt
        // amount. All map to the internal-error code. Each variant renders its own
        // `Display`, which does not stringify a `#[source]` cause.
        RpcError::BlockDeltas(e @ BlockDeltasError::TransactionView(_)) => {
            (ErrorCode::InternalError.code(), e.to_string())
        }
        RpcError::BlockDeltas(e @ BlockDeltasError::Block(_)) => {
            (ErrorCode::InternalError.code(), e.to_string())
        }
        RpcError::BlockDeltas(e @ BlockDeltasError::MissingHeader { .. }) => {
            (ErrorCode::InternalError.code(), e.to_string())
        }
        RpcError::BlockDeltas(e @ BlockDeltasError::InputValueOutOfRange { .. }) => {
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

    /// An out-of-range block height carries zcashd's `-8` code (distinct from the
    /// `-5` not-found), with the "Block height out of range" message intact.
    #[test]
    fn out_of_range_maps_to_minus_eight() {
        let obj = to_error_object(RpcError::OutOfRange(
            "Block height out of range".to_string(),
        ));
        assert_eq!(obj.code(), -8);
        assert_eq!(obj.message(), "Block height out of range");
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

    /// `getaddressdeltas` returns both receives and spends through the wire.
    ///
    /// This is the method the explorer's address page needs and that no
    /// validator answers in plain RPC mode; served locally, the engine's
    /// `AddressRead::deltas` carries positive receives and negative spends, and
    /// the wire handler renders each with its signed magnitude. The mock scripts
    /// two receives and a spend for one address; the call must surface all three
    /// with the signs preserved — a receive rendered as a spend, or a dropped
    /// spend, would fail here.
    #[tokio::test]
    async fn getaddressdeltas_returns_receives_and_spends_through_the_wire() {
        use super::NodeRpcApiServer;
        use crate::NodeRpc;
        use jsonrpsee::core::params::ArrayParams;
        use zaino_primitives::types::{
            AddressDelta, BlockHash, BlockRef, Height, SignedZatoshis, TransactionId,
            TransparentAddress,
        };
        use zaino_service::testing::{MockChain, MockIndexerService};
        use zcash_protocol::consensus::Network;

        let addr = "t1LocalAddressHistoryProbe0000000000";
        let delta = |height: u32, satoshis: i64, index: u32| AddressDelta {
            satoshis: SignedZatoshis::try_new(satoshis).expect("a valid delta"),
            txid: TransactionId::from([u8::try_from(height).expect("a small height"); 32]),
            index,
            height: Height::try_from(height).expect("a valid height"),
            address: TransparentAddress::new(addr.to_string()),
            block_index: Some(0),
        };

        let module = NodeRpc::new(
            MockIndexerService::new(MockChain {
                tip: Some(BlockRef {
                    height: Height::try_from(10).expect("a valid height"),
                    hash: BlockHash::from([1u8; 32]),
                }),
                deltas: vec![delta(1, 100, 0), delta(1, 200, 1), delta(3, -100, 0)],
                ..MockChain::default()
            }),
            Network::MainNetwork,
        )
        .into_rpc();

        let mut params = ArrayParams::new();
        params
            .insert(serde_json::json!({ "addresses": [addr], "start": 0, "end": 4 }))
            .expect("the getaddressdeltas object param");
        let response = module
            .call::<_, serde_json::Value>("getaddressdeltas", params)
            .await
            .expect("getaddressdeltas is served locally");

        let deltas = response
            .get("deltas")
            .and_then(serde_json::Value::as_array)
            .expect("the response carries a deltas array");
        let satoshis: Vec<i64> = deltas
            .iter()
            .filter_map(|entry| entry.get("satoshis").and_then(serde_json::Value::as_i64))
            .collect();
        assert_eq!(
            satoshis,
            vec![100, 200, -100],
            "two receives at height 1 then the spend at height 3, signs preserved"
        );
        assert!(
            deltas.iter().all(
                |entry| entry.get("address").and_then(serde_json::Value::as_str) == Some(addr)
            ),
            "every delta names the queried address"
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

    /// `gettxout` is served: a spent or unknown outpoint answers JSON `null`
    /// through the generated surface, matching zcashd/zebra — not a
    /// method-not-found, and not an invented spend-status string. The default
    /// mock scripts no output, so the outpoint reads as absent.
    #[tokio::test]
    async fn gettxout_unknown_outpoint_is_null() {
        use super::NodeRpcApiServer;
        use crate::NodeRpc;
        use jsonrpsee::core::params::ArrayParams;
        use zaino_service::testing::{MockChain, MockIndexerService};
        use zcash_protocol::consensus::Network;

        let module = NodeRpc::new(
            MockIndexerService::new(MockChain::default()),
            Network::MainNetwork,
        )
        .into_rpc();

        let mut params = ArrayParams::new();
        params.insert("ab".repeat(32)).expect("txid param");
        params.insert(0u32).expect("n param");
        let result = module
            .call::<_, serde_json::Value>("gettxout", params)
            .await
            .expect("gettxout is served");
        assert!(
            result.is_null(),
            "a spent or unknown outpoint is null: {result:?}"
        );
    }

    /// A `MissingHeader` — a hole in the chain view the timestamp search needed —
    /// is a server-side inconsistency, so it maps to the internal-error code, never
    /// a params error blaming the caller. The message names the height via the
    /// variant's own `Display`.
    #[test]
    fn block_hash_missing_header_is_an_internal_error() {
        use zaino_primitives::types::Height;
        use zaino_service::error::BlockHashReadError;
        let obj = to_error_object(RpcError::BlockHashRead(BlockHashReadError::MissingHeader {
            height: Height::try_from(42).expect("valid height"),
        }));
        assert_eq!(obj.code(), ErrorCode::InternalError.code());
        assert!(
            obj.message().contains("42"),
            "the message names the missing height: {}",
            obj.message()
        );
    }

    /// A tier-read failure during the search follows the `BlockReadError` mapping:
    /// internal error, never bad client input.
    #[test]
    fn block_hash_tier_read_is_an_internal_error() {
        use zaino_service::error::{BlockHashReadError, BlockReadError};
        let obj = to_error_object(RpcError::BlockHashRead(BlockHashReadError::TierRead {
            source: BlockReadError::Fatal("backend failure".to_string()),
        }));
        assert_eq!(obj.code(), ErrorCode::InternalError.code());
    }

    /// An asymmetric block hash whose internal first and last bytes differ, so its
    /// display render (a byte-reversal) is distinguishable from the internal bytes.
    fn asym_hash(lead: u8, tail: u8) -> zaino_primitives::types::BlockHash {
        let mut bytes = [0u8; 32];
        bytes[0] = lead;
        bytes[31] = tail;
        zaino_primitives::types::BlockHash::from(bytes)
    }

    /// The display-order hex of [`asym_hash`]: `tail`, then the zero middle, then
    /// `lead`.
    fn asym_display(lead: u8, tail: u8) -> String {
        format!("{tail:02x}{}{lead:02x}", "00".repeat(30))
    }

    /// Two scripted blocks, ascending by time, for the timestamp-range tests.
    fn block_hashes_fixture() -> Vec<zaino_service::BlockHashAt> {
        use zaino_primitives::types::Height;
        use zaino_service::BlockHashAt;
        vec![
            BlockHashAt {
                height: Height::try_from(100).expect("valid height"),
                hash: asym_hash(0x11, 0xaa),
                time: 1_600_000_000,
            },
            BlockHashAt {
                height: Height::try_from(101).expect("valid height"),
                hash: asym_hash(0x22, 0xbb),
                time: 1_600_000_600,
            },
        ]
    }

    fn block_hashes_module(
    ) -> jsonrpsee::RpcModule<crate::NodeRpc<zaino_service::testing::MockIndexerService>> {
        use super::NodeRpcApiServer;
        use crate::NodeRpc;
        use zaino_service::testing::{MockChain, MockIndexerService};
        use zcash_protocol::consensus::Network;
        NodeRpc::new(
            MockIndexerService::new(MockChain {
                block_hashes: block_hashes_fixture(),
                ..MockChain::default()
            }),
            Network::MainNetwork,
        )
        .into_rpc()
    }

    /// Each non-logical param shape the explorer sends — `[high, low]`,
    /// `[high, low, {}]`, and `[high, low, {"noOrphans":true,"logicalTimes":false}]`
    /// — returns the same bare array of display-order hash strings, ascending by
    /// block time. Driven through the generated jsonrpsee surface, so this exercises
    /// the real positional-parameter parsing, including the optional options object.
    #[tokio::test]
    async fn getblockhashes_non_logical_param_shapes_render_display_order_hashes() {
        use jsonrpsee::core::params::ArrayParams;
        use serde_json::Value;
        let expected = Value::from(vec![asym_display(0x11, 0xaa), asym_display(0x22, 0xbb)]);
        let shapes: Vec<ArrayParams> = {
            // [high, low]
            let mut bare = ArrayParams::new();
            bare.insert(1_600_001_000u32).expect("high");
            bare.insert(0u32).expect("low");
            // [high, low, {}]
            let mut empty = ArrayParams::new();
            empty.insert(1_600_001_000u32).expect("high");
            empty.insert(0u32).expect("low");
            empty.insert(serde_json::json!({})).expect("empty options");
            // [high, low, {"noOrphans":true,"logicalTimes":false}]
            let mut full = ArrayParams::new();
            full.insert(1_600_001_000u32).expect("high");
            full.insert(0u32).expect("low");
            full.insert(serde_json::json!({"noOrphans": true, "logicalTimes": false}))
                .expect("options");
            vec![bare, empty, full]
        };
        for params in shapes {
            let module = block_hashes_module();
            let result: Value = module
                .call("getblockhashes", params)
                .await
                .expect("getblockhashes succeeds");
            assert_eq!(result, expected);
        }
    }

    /// `logicalTimes: true` renders `{blockhash, logicalts}` objects — exactly those
    /// two keys — in the same ascending-by-time order, with the block times as
    /// `logicalts` and the hashes in display order.
    #[tokio::test]
    async fn getblockhashes_logical_times_renders_objects() {
        use jsonrpsee::core::params::ArrayParams;
        use serde_json::Value;
        let module = block_hashes_module();
        let mut params = ArrayParams::new();
        params.insert(1_600_001_000u32).expect("high");
        params.insert(0u32).expect("low");
        params
            .insert(serde_json::json!({"noOrphans": true, "logicalTimes": true}))
            .expect("options");
        let result: Value = module
            .call("getblockhashes", params)
            .await
            .expect("getblockhashes succeeds");
        let arr = result.as_array().expect("an array of objects");
        assert_eq!(arr.len(), 2);
        let mut keys: Vec<&str> = arr[0]
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, ["blockhash", "logicalts"]);
        assert_eq!(
            arr[0].get("blockhash").and_then(Value::as_str),
            Some(asym_display(0x11, 0xaa).as_str())
        );
        assert_eq!(
            arr[0].get("logicalts").and_then(Value::as_u64),
            Some(1_600_000_000)
        );
        assert_eq!(
            arr[1].get("logicalts").and_then(Value::as_u64),
            Some(1_600_000_600)
        );
    }

    /// `high` below `low` is an empty range `[low, high)`, so the result is an empty
    /// array — matching zcashd, which seeks its timestamp index to `low` and stops
    /// at the first entry not below `high`, returning an empty list rather than an
    /// error.
    #[tokio::test]
    async fn getblockhashes_high_below_low_is_an_empty_list() {
        use jsonrpsee::core::params::ArrayParams;
        use serde_json::Value;
        let module = block_hashes_module();
        let mut params = ArrayParams::new();
        params.insert(100u32).expect("high");
        params.insert(1_600_000_000u32).expect("low");
        let result: Value = module
            .call("getblockhashes", params)
            .await
            .expect("an inverted range is a valid query with an empty answer");
        assert_eq!(result, Value::from(Vec::<Value>::new()));
    }

    /// A non-integer timestamp is rejected with invalid-params by the generated
    /// surface, before the handler runs — mirroring zcashd's `get_int()`, which
    /// throws on a non-numeric parameter.
    #[tokio::test]
    async fn getblockhashes_non_integer_timestamp_is_invalid_params() {
        use jsonrpsee::core::params::ArrayParams;
        use jsonrpsee::core::server::MethodsError;
        use serde_json::Value;
        let module = block_hashes_module();
        let mut params = ArrayParams::new();
        params.insert("not-a-number").expect("high as a string");
        params.insert(0u32).expect("low");
        let err = module
            .call::<_, Value>("getblockhashes", params)
            .await
            .expect_err("a non-integer timestamp must be rejected");
        match err {
            MethodsError::JsonRpc(obj) => {
                assert_eq!(obj.code(), ErrorCode::InvalidParams.code());
            }
            other => panic!("expected an invalid-params JSON-RPC error, got {other:?}"),
        }
    }
}
