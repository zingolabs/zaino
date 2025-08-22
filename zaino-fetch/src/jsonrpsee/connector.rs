//! JsonRPSee client implementation.
//!
//! TODO: - Add option for http connector.
//!       - Refactor JsonRPSeecConnectorError into concrete error types and implement fmt::display [<https://github.com/zingolabs/zaino/issues/67>].

use http::Uri;
use reqwest::{Client, ClientBuilder, Url};
use serde::{Deserialize, Serialize};
use std::{
    any::type_name,
    convert::Infallible,
    fmt,
    path::Path,
    sync::{
        atomic::{AtomicI32, Ordering},
        Arc,
    },
    time::Duration,
};
use tracing::error;
use zaino_commons::config::{
    AuthHeader, BackendConfig, ValidatorConfig, ValidatorFetchConfig, ZcashdAuth, ZebradAuth, ZebradStateConfig,
};

use zebra_rpc::client::ValidateAddressResponse;

use crate::jsonrpsee::{
    error::{JsonRpcError, TransportError},
    response::{
        GetBalanceError, GetBalanceResponse, GetBlockCountResponse, GetBlockError, GetBlockHash,
        GetBlockResponse, GetBlockchainInfoResponse, GetInfoResponse, GetMempoolInfoResponse,
        GetSubtreesError, GetSubtreesResponse, GetTransactionResponse, GetTreestateError,
        GetTreestateResponse, GetUtxosError, GetUtxosResponse, SendTransactionError,
        SendTransactionResponse, TxidsError, TxidsResponse,
    },
};

use super::response::GetDifficultyResponse;

#[derive(Serialize, Deserialize, Debug)]
struct RpcRequest<T> {
    jsonrpc: String,
    method: String,
    params: T,
    id: i32,
}

#[derive(Serialize, Deserialize, Debug)]
struct RpcResponse<T> {
    id: i64,
    jsonrpc: Option<String>,
    result: Option<T>,
    error: Option<RpcError>,
}

/// Json RPSee Error type.
#[derive(Serialize, Deserialize, Debug)]
pub struct RpcError {
    /// Error Code.
    pub code: i64,
    /// Error Message.
    pub message: String,
    /// Error Data.
    pub data: Option<JsonRpcError>,
}

impl RpcError {
    /// Creates a new `RpcError` from zebra's `LegacyCode` enum
    pub fn new_from_legacycode(
        code: zebra_rpc::server::error::LegacyCode,
        message: impl Into<String>,
    ) -> Self {
        RpcError {
            code: code as i64,
            message: message.into(),
            data: None,
        }
    }
    /// Creates a new `RpcError` from jsonrpsee-types `ErrorObject`.
    pub fn new_from_errorobject(
        error_obj: jsonrpsee_types::ErrorObject<'_>,
        fallback_message: impl Into<String>,
    ) -> Self {
        RpcError {
            // We can use the actual JSON-RPC code:
            code: error_obj.code() as i64,

            // Or combine the fallback with the original message:
            message: format!("{}: {}", fallback_message.into(), error_obj.message()),

            // If you want to store the data too:
            data: error_obj
                .data()
                .map(|raw| serde_json::from_str(raw.get()).unwrap()),
        }
    }
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RPC Error (code: {}): {}", self.code, self.message)
    }
}

impl std::error::Error for RpcError {}

// Cookie token reading functionality moved to zaino-commons AuthMethod
// for self-contained authentication handling

/// Trait to convert a JSON-RPC response to an error.
pub trait ResponseToError: Sized {
    /// The error type.
    type RpcError: std::fmt::Debug
        + TryFrom<RpcError, Error: std::error::Error + Send + Sync + 'static>;

    /// Converts a JSON-RPC response to an error.
    fn to_error(self) -> Result<Self, Self::RpcError> {
        Ok(self)
    }
}

/// Error type for JSON-RPC requests.
#[derive(Debug, thiserror::Error)]
pub enum RpcRequestError<MethodError> {
    /// Error variant for errors related to the transport layer.
    #[error("Transport error: {0}")]
    Transport(#[from] TransportError),

    /// Error variant for errors related to the JSON-RPC method being called.
    #[error("Method error: {0:?}")]
    Method(MethodError),

    /// The provided input failed to serialize.
    #[error("request input failed to serialize: {0:?}")]
    JsonRpc(serde_json::Error),

    /// Internal unrecoverable error.
    #[error("Internal unrecoverable error")]
    InternalUnrecoverable,

    /// Server at capacity
    #[error("rpc server at capacity, please try again")]
    ServerWorkQueueFull,

    /// An error related to the specific JSON-RPC method being called, that
    /// wasn't accounted for as a MethodError. This means that either
    /// Zaino has not yet accounted for the possibilty of this error,
    /// or the Node returned an undocumented/malformed error response.
    #[error("unexpected error response from server: {0}")]
    UnexpectedErrorResponse(Box<dyn std::error::Error + Send + Sync + 'static>),
}

/// JsonRpSee Client config data.
#[derive(Debug, Clone)]
pub struct JsonRpSeeConnector {
    url: Url,
    id_counter: Arc<AtomicI32>,
    client: Client,
    auth_header: Option<AuthHeader>,
}

impl JsonRpSeeConnector {
    /// Creates a new JsonRpSeeConnector with optional authentication header.
    ///
    /// The service should handle authentication and provide the pre-computed
    /// auth header. This keeps the connector focused on JSON-RPC transport.
    pub fn new(url: Url, auth_header: Option<AuthHeader>) -> Result<Self, TransportError> {
        let client = ClientBuilder::new()
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(TransportError::ReqwestError)?;

        Ok(Self {
            url,
            id_counter: Arc::new(AtomicI32::new(0)),
            client,
            auth_header,
        })
    }

    /// Creates a connector from a ValidatorFetchConfig.
    pub fn from_validator_fetch_config(
        config: &ValidatorFetchConfig,
    ) -> Result<Self, TransportError> {
        let (rpc_address, auth) = match config {
            ValidatorFetchConfig::Zebrad { rpc_address, auth } => {
                (rpc_address, auth.get_auth_header())
            }
            ValidatorFetchConfig::Zcashd { rpc_address, auth } => {
                (rpc_address, auth.get_auth_header())
            }
        };

        let url = reqwest::Url::parse(&format!("http://{}", rpc_address))
            .map_err(|e| TransportError::BadNodeData(Box::new(e), "URL parsing"))?;

        let auth_header =
            auth.map_err(|e| TransportError::BadNodeData(Box::new(e), "Auth header"))?;

        Self::new(url, auth_header)
    }

    /// Creates a connector from a ZebradStateConfig.
    pub fn from_zebrad_state_config(config: &ZebradStateConfig) -> Result<Self, TransportError> {
        let url = reqwest::Url::parse(&format!("http://{}", config.rpc_address))
            .map_err(|e| TransportError::BadNodeData(Box::new(e), "URL parsing"))?;

        let auth_header = config
            .auth
            .get_auth_header()
            .map_err(|e| TransportError::BadNodeData(Box::new(e), "Auth header"))?;

        Self::new(url, auth_header)
    }

    /// Creates a connector from a BackendConfig.
    /// This provides a convenient way to create a connector directly from the
    /// backend configuration, handling connection testing and auth automatically.
    pub async fn from_backend_config(config: &BackendConfig) -> Result<Self, TransportError> {
        // Use test_and_get_url to get a validated, tested URL
        let url = config.test_and_get_url()
            .await
            .map_err(|e| TransportError::BadNodeData(Box::new(e), "Backend connection test failed"))?;

        let auth_header = match config {
            BackendConfig::LocalZebra { auth, .. } 
            | BackendConfig::RemoteZebra { auth, .. } => {
                auth.get_auth_header()
                    .map_err(|e| TransportError::BadNodeData(Box::new(e), "Zebra auth header"))?
            },
            BackendConfig::RemoteZcashd { auth, .. } 
            | BackendConfig::RemoteZainod { auth, .. } => {
                auth.get_auth_header()
                    .map_err(|e| TransportError::BadNodeData(Box::new(e), "Zcashd auth header"))?
            },
        };

        Self::new(url, auth_header)
    }

    /// Returns the http::uri the JsonRpSeeConnector is configured to send requests to.
    pub fn uri(&self) -> Result<Uri, TransportError> {
        Ok(self.url.as_str().parse()?)
    }

    /// Returns the reqwest::url the JsonRpSeeConnector is configured to send requests to.
    pub fn url(&self) -> Url {
        self.url.clone()
    }

    /// Sends a jsonRPC request and returns the response.
    ///
    /// NOTE: This function currently resends the call up to 5 times on a server response of "Work queue depth exceeded".
    ///       This is because the node's queue can become overloaded and stop servicing RPCs.
    async fn send_request<
        T: std::fmt::Debug + Serialize,
        R: std::fmt::Debug + for<'de> Deserialize<'de> + ResponseToError,
    >(
        &self,
        method: &str,
        params: T,
    ) -> Result<R, RpcRequestError<R::RpcError>>
    where
        R::RpcError: Send + Sync + 'static,
    {
        let id = self.id_counter.fetch_add(1, Ordering::SeqCst);

        let max_attempts = 5;
        let mut attempts = 0;
        loop {
            attempts += 1;

            let request_builder = self
                .build_request(method, &params, id)
                .map_err(RpcRequestError::JsonRpc)?;

            let response = request_builder
                .send()
                .await
                .map_err(|e| RpcRequestError::Transport(TransportError::ReqwestError(e)))?;

            let status = response.status();

            let body_bytes = response
                .bytes()
                .await
                .map_err(|e| RpcRequestError::Transport(TransportError::ReqwestError(e)))?;

            let body_str = String::from_utf8_lossy(&body_bytes);

            if body_str.contains("Work queue depth exceeded") {
                if attempts >= max_attempts {
                    return Err(RpcRequestError::ServerWorkQueueFull);
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                continue;
            }

            let code = status.as_u16();
            return match code {
                // Invalid
                ..100 | 600.. => Err(RpcRequestError::Transport(
                    TransportError::InvalidStatusCode(code),
                )),
                // Informational | Redirection
                100..200 | 300..400 => Err(RpcRequestError::Transport(
                    TransportError::UnexpectedStatusCode(code),
                )),
                // Success
                200..300 => {
                    let response: RpcResponse<R> = serde_json::from_slice(&body_bytes)
                        .map_err(|e| TransportError::BadNodeData(Box::new(e), type_name::<R>()))?;

                    match (response.error, response.result) {
                        (Some(error), _) => Err(RpcRequestError::Method(
                            R::RpcError::try_from(error).map_err(|e| {
                                RpcRequestError::UnexpectedErrorResponse(Box::new(e))
                            })?,
                        )),
                        (None, Some(result)) => match result.to_error() {
                            Ok(r) => Ok(r),
                            Err(e) => Err(RpcRequestError::Method(e)),
                        },
                        (None, None) => Err(RpcRequestError::Transport(
                            TransportError::EmptyResponseBody,
                        )),
                    }
                    // Error
                }
                400..600 => Err(RpcRequestError::Transport(TransportError::ErrorStatusCode(
                    code,
                ))),
            };
        }
    }

    /// Builds a request from a given method, params, and id.
    fn build_request<T: std::fmt::Debug + Serialize>(
        &self,
        method: &str,
        params: T,
        id: i32,
    ) -> serde_json::Result<reqwest::RequestBuilder> {
        let req = RpcRequest {
            jsonrpc: "2.0".to_string(),
            method: method.to_string(),
            params,
            id,
        };

        let mut request_builder = self
            .client
            .post(self.url.clone())
            .header("Content-Type", "application/json");

        if let Some(auth_header) = &self.auth_header {
            request_builder = request_builder.header(auth_header.key(), auth_header.value());
        }

        let request_body = serde_json::to_string(&req)?;
        request_builder = request_builder.body(request_body);

        Ok(request_builder)
    }

    /// Returns software information from the RPC server, as a [`crate::jsonrpsee::connector::GetInfoResponse`] JSON struct.
    ///
    /// zcashd reference: [`getinfo`](https://zcash.github.io/rpc/getinfo.html)
    /// method: post
    /// tags: control
    pub async fn get_info(&self) -> Result<GetInfoResponse, RpcRequestError<Infallible>> {
        self.send_request::<(), GetInfoResponse>("getinfo", ())
            .await
    }

    ///
    /// zcashd reference: [`getblockchaininfo`](https://zcash.github.io/rpc/getblockchaininfo.html)
    /// method: post
    /// tags: blockchain
    pub async fn get_blockchain_info(
        &self,
    ) -> Result<GetBlockchainInfoResponse, RpcRequestError<Infallible>> {
        self.send_request::<(), GetBlockchainInfoResponse>("getblockchaininfo", ())
            .await
    }

    /// Returns details on the active state of the TX memory pool.
    ///
    /// online zcash rpc reference: [`getmempoolinfo`](https://zcash.github.io/rpc/getmempoolinfo.html)
    /// method: post
    /// tags: mempool
    ///
    /// Canonical source code implementation: [`getmempoolinfo`](https://github.com/zcash/zcash/blob/18238d90cd0b810f5b07d5aaa1338126aa128c06/src/rpc/blockchain.cpp#L1555)
    ///
    /// Zebra does not support this RPC directly.
    pub async fn get_mempool_info(
        &self,
    ) -> Result<GetMempoolInfoResponse, RpcRequestError<Infallible>> {
        self.send_request::<(), GetMempoolInfoResponse>("getmempoolinfo", ())
            .await
    }

    /// Returns the proof-of-work difficulty as a multiple of the minimum difficulty.
    ///
    /// zcashd reference: [`getdifficulty`](https://zcash.github.io/rpc/getdifficulty.html)
    /// method: post
    /// tags: blockchain
    pub async fn get_difficulty(
        &self,
    ) -> Result<GetDifficultyResponse, RpcRequestError<Infallible>> {
        self.send_request::<(), GetDifficultyResponse>("getdifficulty", ())
            .await
    }

    /// Returns the total balance of a provided `addresses` in an [`crate::jsonrpsee::response::GetBalanceResponse`] instance.
    ///
    /// zcashd reference: [`getaddressbalance`](https://zcash.github.io/rpc/getaddressbalance.html)
    /// method: post
    /// tags: address
    ///
    /// # Parameters
    ///
    /// - `address_strings`: (object, example={"addresses": ["tmYXBYJj1K7vhejSec5osXK2QsGa5MTisUQ"]}) A JSON map with a single entry
    ///     - `addresses`: (array of strings) A list of base-58 encoded addresses.
    pub async fn get_address_balance(
        &self,
        addresses: Vec<String>,
    ) -> Result<GetBalanceResponse, RpcRequestError<GetBalanceError>> {
        let params = vec![serde_json::json!({ "addresses": addresses })];
        self.send_request("getaddressbalance", params).await
    }

    /// Sends the raw bytes of a signed transaction to the local node's mempool, if the transaction is valid.
    ///
    /// zcashd reference: [`sendrawtransaction`](https://zcash.github.io/rpc/sendrawtransaction.html)
    /// method: post
    /// tags: transaction
    ///
    /// # Parameters
    ///
    /// - `raw_transaction_hex`: (string, required, example="signedhex") The hex-encoded raw transaction bytes.
    pub async fn send_raw_transaction(
        &self,
        raw_transaction_hex: String,
    ) -> Result<SendTransactionResponse, RpcRequestError<SendTransactionError>> {
        let params =
            vec![serde_json::to_value(raw_transaction_hex).map_err(RpcRequestError::JsonRpc)?];
        self.send_request("sendrawtransaction", params).await
    }

    /// Returns the requested block by hash or height, as a [`GetBlockResponse`].
    /// If the block is not in Zebra's state, returns
    /// [error code `-8`.](https://github.com/zcash/zcash/issues/5758)
    ///
    /// zcashd reference: [`getblock`](https://zcash.github.io/rpc/getblock.html)
    /// method: post
    /// tags: blockchain
    ///
    /// # Parameters
    ///
    /// - `hash_or_height`: (string, required, example="1") The hash or height for the block to be returned.
    /// - `verbosity`: (number, optional, default=1, example=1) 0 for hex encoded data, 1 for a json object, and 2 for json object with transaction data.
    pub async fn get_block(
        &self,
        hash_or_height: String,
        verbosity: Option<u8>,
    ) -> Result<GetBlockResponse, RpcRequestError<GetBlockError>> {
        let v = verbosity.unwrap_or(1);
        let params = [
            serde_json::to_value(hash_or_height).map_err(RpcRequestError::JsonRpc)?,
            serde_json::to_value(v).map_err(RpcRequestError::JsonRpc)?,
        ];

        if v == 0 {
            self.send_request("getblock", params)
                .await
                .map(GetBlockResponse::Raw)
        } else {
            self.send_request("getblock", params)
                .await
                .map(GetBlockResponse::Object)
        }
    }

    /// Returns the hash of the best block (tip) of the longest chain.
    /// zcashd reference: [`getbestblockhash`](https://zcash.github.io/rpc/getbestblockhash.html)
    /// method: post
    /// tags: blockchain
    ///
    /// # Notes
    ///
    /// The zcashd doc reference above says there are no parameters and the result is a "hex" (string) of the block hash hex encoded.
    /// The Zcash source code is considered canonical.
    /// [In the rpc definition](https://github.com/zcash/zcash/blob/654a8be2274aa98144c80c1ac459400eaf0eacbe/src/rpc/common.h#L48) there are no required params, or optional params.
    /// [The function in rpc/blockchain.cpp](https://github.com/zcash/zcash/blob/654a8be2274aa98144c80c1ac459400eaf0eacbe/src/rpc/blockchain.cpp#L325)
    /// where `return chainActive.Tip()->GetBlockHash().GetHex();` is the [return expression](https://github.com/zcash/zcash/blob/654a8be2274aa98144c80c1ac459400eaf0eacbe/src/rpc/blockchain.cpp#L339)returning a `std::string`
    pub async fn get_best_blockhash(&self) -> Result<GetBlockHash, RpcRequestError<Infallible>> {
        self.send_request::<(), GetBlockHash>("getbestblockhash", ())
            .await
    }

    /// Returns the height of the most recent block in the best valid block chain
    /// (equivalently, the number of blocks in this chain excluding the genesis block).
    ///
    /// zcashd reference: [`getblockcount`](https://zcash.github.io/rpc/getblockcount.html)
    /// method: post
    /// tags: blockchain
    pub async fn get_block_count(
        &self,
    ) -> Result<GetBlockCountResponse, RpcRequestError<Infallible>> {
        self.send_request::<(), GetBlockCountResponse>("getblockcount", ())
            .await
    }

    /// Return information about the given Zcash address.
    ///
    /// # Parameters
    /// - `address`: (string, required, example="tmHMBeeYRuc2eVicLNfP15YLxbQsooCA6jb") The Zcash transparent address to validate.
    ///
    /// zcashd reference: [`validateaddress`](https://zcash.github.io/rpc/validateaddress.html)
    /// method: post
    /// tags: blockchain
    pub async fn validate_address(
        &self,
        address: String,
    ) -> Result<ValidateAddressResponse, RpcRequestError<Infallible>> {
        let params = vec![serde_json::to_value(address).map_err(RpcRequestError::JsonRpc)?];
        self.send_request("validateaddress", params).await
    }

    /// Returns all transaction ids in the memory pool, as a JSON array.
    ///
    /// zcashd reference: [`getrawmempool`](https://zcash.github.io/rpc/getrawmempool.html)
    /// method: post
    /// tags: blockchain
    pub async fn get_raw_mempool(&self) -> Result<TxidsResponse, RpcRequestError<TxidsError>> {
        self.send_request::<(), TxidsResponse>("getrawmempool", ())
            .await
    }

    /// Returns information about the given block's Sapling & Orchard tree state.
    ///
    /// zcashd reference: [`z_gettreestate`](https://zcash.github.io/rpc/z_gettreestate.html)
    /// method: post
    /// tags: blockchain
    ///
    /// # Parameters
    ///
    /// - `hash | height`: (string, required, example="00000000febc373a1da2bd9f887b105ad79ddc26ac26c2b28652d64e5207c5b5") The block hash or height.
    pub async fn get_treestate(
        &self,
        hash_or_height: String,
    ) -> Result<GetTreestateResponse, RpcRequestError<GetTreestateError>> {
        let params = vec![serde_json::to_value(hash_or_height).map_err(RpcRequestError::JsonRpc)?];
        self.send_request("z_gettreestate", params).await
    }

    /// Returns information about a range of Sapling or Orchard subtrees.
    ///
    /// zcashd reference: [`z_getsubtreesbyindex`](https://zcash.github.io/rpc/z_getsubtreesbyindex.html) - TODO: fix link
    /// method: post
    /// tags: blockchain
    ///
    /// # Parameters
    ///
    /// - `pool`: (string, required) The pool from which subtrees should be returned. Either "sapling" or "orchard".
    /// - `start_index`: (number, required) The index of the first 2^16-leaf subtree to return.
    /// - `limit`: (number, optional) The maximum number of subtree values to return.
    pub async fn get_subtrees_by_index(
        &self,
        pool: String,
        start_index: u16,
        limit: Option<u16>,
    ) -> Result<GetSubtreesResponse, RpcRequestError<GetSubtreesError>> {
        let params = match limit {
            Some(v) => vec![
                serde_json::to_value(pool).map_err(RpcRequestError::JsonRpc)?,
                serde_json::to_value(start_index).map_err(RpcRequestError::JsonRpc)?,
                serde_json::to_value(v).map_err(RpcRequestError::JsonRpc)?,
            ],
            None => vec![
                serde_json::to_value(pool).map_err(RpcRequestError::JsonRpc)?,
                serde_json::to_value(start_index).map_err(RpcRequestError::JsonRpc)?,
            ],
        };
        self.send_request("z_getsubtreesbyindex", params).await
    }

    /// Returns the raw transaction data, as a [`GetTransactionResponse`].
    ///
    /// zcashd reference: [`getrawtransaction`](https://zcash.github.io/rpc/getrawtransaction.html)
    /// method: post
    /// tags: transaction
    ///
    /// # Parameters
    ///
    /// - `txid`: (string, required, example="mytxid") The transaction ID of the transaction to be returned.
    /// - `verbose`: (number, optional, default=0, example=1) If 0, return a string of hex-encoded data, otherwise return a JSON object.
    pub async fn get_raw_transaction(
        &self,
        txid_hex: String,
        verbose: Option<u8>,
    ) -> Result<GetTransactionResponse, RpcRequestError<Infallible>> {
        let params = match verbose {
            Some(v) => vec![
                serde_json::to_value(txid_hex).map_err(RpcRequestError::JsonRpc)?,
                serde_json::to_value(v).map_err(RpcRequestError::JsonRpc)?,
            ],
            None => vec![
                serde_json::to_value(txid_hex).map_err(RpcRequestError::JsonRpc)?,
                serde_json::to_value(0).map_err(RpcRequestError::JsonRpc)?,
            ],
        };

        self.send_request("getrawtransaction", params).await
    }

    /// Returns the transaction ids made by the provided transparent addresses.
    ///
    /// zcashd reference: [`getaddresstxids`](https://zcash.github.io/rpc/getaddresstxids.html)
    /// method: post
    /// tags: address
    ///
    /// # Parameters
    ///
    /// - `request`: (object, required, example={\"addresses\": [\"tmYXBYJj1K7vhejSec5osXK2QsGa5MTisUQ\"], \"start\": 1000, \"end\": 2000}) A struct with the following named fields:
    ///     - `addresses`: (json array of string, required) The addresses to get transactions from.
    ///     - `start`: (numeric, required) The lower height to start looking for transactions (inclusive).
    ///     - `end`: (numeric, required) The top height to stop looking for transactions (inclusive).
    pub async fn get_address_txids(
        &self,
        addresses: Vec<String>,
        start: u32,
        end: u32,
    ) -> Result<TxidsResponse, RpcRequestError<TxidsError>> {
        let params = serde_json::json!({
            "addresses": addresses,
            "start": start,
            "end": end
        });

        self.send_request("getaddresstxids", vec![params]).await
    }

    /// Returns all unspent outputs for a list of addresses.
    ///
    /// zcashd reference: [`getaddressutxos`](https://zcash.github.io/rpc/getaddressutxos.html)
    /// method: post
    /// tags: address
    ///
    /// # Parameters
    ///
    /// - `addresses`: (array, required, example={\"addresses\": [\"tmYXBYJj1K7vhejSec5osXK2QsGa5MTisUQ\"]}) The addresses to get outputs from.
    pub async fn get_address_utxos(
        &self,
        addresses: Vec<String>,
    ) -> Result<Vec<GetUtxosResponse>, RpcRequestError<GetUtxosError>> {
        let params = vec![serde_json::json!({ "addresses": addresses })];
        self.send_request("getaddressutxos", params).await
    }
}
