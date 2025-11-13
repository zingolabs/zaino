//! JsonRPSee client implementation.
//!
//! TODO: - Add option for http connector.
//!       - Refactor JsonRPSeecConnectorError into concrete error types and implement fmt::display [<https://github.com/zingolabs/zaino/issues/67>].
use base64::{engine::general_purpose, Engine};
use http::Uri;
use reqwest::{Client, ClientBuilder, Url};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::{
    any::type_name,
    convert::Infallible,
    fmt, fs,
    net::SocketAddr,
    path::Path,
    sync::{
        atomic::{AtomicI32, Ordering},
        Arc,
    },
    time::Duration,
};
use tracing::error;
use zebra_rpc::client::ValidateAddressResponse;

use crate::jsonrpsee::response::address_deltas::GetAddressDeltasError;
use crate::jsonrpsee::{
    error::{JsonRpcError, TransportError},
    response::{
        address_deltas::{GetAddressDeltasParams, GetAddressDeltasResponse},
        block_deltas::{BlockDeltas, BlockDeltasError},
        block_header::{GetBlockHeader, GetBlockHeaderError},
        block_subsidy::GetBlockSubsidy,
        mining_info::GetMiningInfoWire,
        peer_info::GetPeerInfo,
        GetBalanceError, GetBalanceResponse, GetBlockCountResponse, GetBlockError, GetBlockHash,
        GetBlockResponse, GetBlockchainInfoResponse, GetInfoResponse, GetMempoolInfoResponse,
        GetSubtreesError, GetSubtreesResponse, GetTransactionResponse, GetTreestateError,
        GetTreestateResponse, GetUtxosError, GetUtxosResponse, SendTransactionError,
        SendTransactionResponse, TxidsError, TxidsResponse,
    },
};

use super::response::{GetDifficultyResponse, GetNetworkSolPsResponse};

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

// Helper function to read and parse the cookie file content.
// Zebra's RPC server expects Basic Auth with username "__cookie__"
// and the token from the cookie file as the password.
// The cookie file itself is formatted as "__cookie__:<token>".
// This function extracts just the <token> part.
fn read_and_parse_cookie_token(cookie_path: &Path) -> Result<String, TransportError> {
    let cookie_content =
        fs::read_to_string(cookie_path).map_err(TransportError::CookieReadError)?;
    let trimmed_content = cookie_content.trim();
    if let Some(stripped) = trimmed_content.strip_prefix("__cookie__:") {
        Ok(stripped.to_string())
    } else {
        // If the prefix is not present, use the entire trimmed content.
        // This maintains compatibility with older formats or other cookie sources.
        Ok(trimmed_content.to_string())
    }
}

#[derive(Debug, Clone)]
enum AuthMethod {
    Basic { username: String, password: String },
    Cookie { cookie: String },
}

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
    #[error("Internal unrecoverable error: {0}")]
    InternalUnrecoverable(String),

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
    auth_method: AuthMethod,
}

impl JsonRpSeeConnector {
    /// Creates a new JsonRpSeeConnector with Basic Authentication.
    pub fn new_with_basic_auth(
        url: Url,
        username: String,
        password: String,
    ) -> Result<Self, TransportError> {
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
            auth_method: AuthMethod::Basic { username, password },
        })
    }

    /// Creates a new JsonRpSeeConnector with Cookie Authentication.
    pub fn new_with_cookie_auth(url: Url, cookie_path: &Path) -> Result<Self, TransportError> {
        let cookie_password = read_and_parse_cookie_token(cookie_path)?;

        let client = ClientBuilder::new()
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .cookie_store(true)
            .build()
            .map_err(TransportError::ReqwestError)?;

        Ok(Self {
            url,
            id_counter: Arc::new(AtomicI32::new(0)),
            client,
            auth_method: AuthMethod::Cookie {
                cookie: cookie_password,
            },
        })
    }

    /// Helper function to create from parts of a StateServiceConfig or FetchServiceConfig
    pub async fn new_from_config_parts(
        validator_rpc_address: SocketAddr,
        validator_rpc_user: String,
        validator_rpc_password: String,
        validator_cookie_path: Option<PathBuf>,
    ) -> Result<Self, TransportError> {
        match validator_cookie_path.is_some() {
            true => JsonRpSeeConnector::new_with_cookie_auth(
                test_node_and_return_url(
                    validator_rpc_address,
                    validator_cookie_path.clone(),
                    None,
                    None,
                )
                .await?,
                Path::new(
                    &validator_cookie_path
                        .clone()
                        .expect("validator cookie authentication path missing"),
                ),
            ),
            false => JsonRpSeeConnector::new_with_basic_auth(
                test_node_and_return_url(
                    validator_rpc_address,
                    None,
                    Some(validator_rpc_user.clone()),
                    Some(validator_rpc_password.clone()),
                )
                .await?,
                validator_rpc_user.clone(),
                validator_rpc_password.clone(),
            ),
        }
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

        match &self.auth_method {
            AuthMethod::Basic { username, password } => {
                request_builder = request_builder.basic_auth(username, Some(password));
            }
            AuthMethod::Cookie { cookie } => {
                request_builder = request_builder.header(
                    reqwest::header::AUTHORIZATION,
                    format!(
                        "Basic {}",
                        general_purpose::STANDARD.encode(format!("__cookie__:{cookie}"))
                    ),
                );
            }
        }

        let request_body = serde_json::to_string(&req)?;
        request_builder = request_builder.body(request_body);

        Ok(request_builder)
    }

    /// Returns all changes for an address.
    ///
    /// Returns information about all changes to the given transparent addresses within the given block range (inclusive)
    ///
    /// block height range, default is the full blockchain.
    /// If start or end are not specified, they default to zero.
    /// If start is greater than the latest block height, it's interpreted as that height.
    ///
    /// If end is zero, it's interpreted as the latest block height.
    ///
    /// [Original zcashd implementation](https://github.com/zcash/zcash/blob/18238d90cd0b810f5b07d5aaa1338126aa128c06/src/rpc/misc.cpp#L881)
    ///
    /// zcashd reference: [`getaddressdeltas`](https://zcash.github.io/rpc/getaddressdeltas.html)
    /// method: post
    /// tags: address
    pub async fn get_address_deltas(
        &self,
        params: GetAddressDeltasParams,
    ) -> Result<GetAddressDeltasResponse, RpcRequestError<GetAddressDeltasError>> {
        let params = vec![serde_json::to_value(params).map_err(RpcRequestError::JsonRpc)?];
        self.send_request("getaddressdeltas", params).await
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

    /// Returns data about each connected network node as a json array of objects.
    ///
    /// zcashd reference: [`getpeerinfo`](https://zcash.github.io/rpc/getpeerinfo.html)
    /// tags: network
    ///
    /// Current `zebrad` does not include the same fields as `zcashd`.
    pub async fn get_peer_info(&self) -> Result<GetPeerInfo, RpcRequestError<Infallible>> {
        self.send_request::<(), GetPeerInfo>("getpeerinfo", ())
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

    /// Returns block subsidy reward, taking into account the mining slow start and the founders reward, of block at index provided.
    ///
    /// zcashd reference: [`getblocksubsidy`](https://zcash.github.io/rpc/getblocksubsidy.html)
    /// method: post
    /// tags: blockchain
    ///
    /// # Parameters
    ///
    /// - `height`: (number, optional) The block height. If not provided, defaults to the current height of the chain.
    pub async fn get_block_subsidy(
        &self,
        height: u32,
    ) -> Result<GetBlockSubsidy, RpcRequestError<Infallible>> {
        let params = vec![serde_json::to_value(height).map_err(RpcRequestError::JsonRpc)?];
        self.send_request("getblocksubsidy", params).await
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

    /// Returns information about the given block and its transactions.
    ///
    /// zcashd reference: [`getblockdeltas`](https://zcash.github.io/rpc/getblockdeltas.html)
    /// method: post
    /// tags: blockchain
    pub async fn get_block_deltas(
        &self,
        hash: String,
    ) -> Result<BlockDeltas, RpcRequestError<BlockDeltasError>> {
        let params = vec![serde_json::to_value(hash).map_err(RpcRequestError::JsonRpc)?];
        self.send_request("getblockdeltas", params).await
    }

    /// If verbose is false, returns a string that is serialized, hex-encoded data for blockheader `hash`.
    /// If verbose is true, returns an Object with information about blockheader `hash`.
    ///
    /// # Parameters
    ///
    /// - hash: (string, required) The block hash
    /// - verbose: (boolean, optional, default=true) true for a json object, false for the hex encoded data
    ///
    /// zcashd reference: [`getblockheader`](https://zcash.github.io/rpc/getblockheader.html)
    /// method: post
    /// tags: blockchain
    pub async fn get_block_header(
        &self,
        hash: String,
        verbose: bool,
    ) -> Result<GetBlockHeader, RpcRequestError<GetBlockHeaderError>> {
        let params = [
            serde_json::to_value(hash).map_err(RpcRequestError::JsonRpc)?,
            serde_json::to_value(verbose).map_err(RpcRequestError::JsonRpc)?,
        ];
        self.send_request("getblockheader", params).await
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

    /// Returns a json object containing mining-related information.
    ///
    /// `zcashd` reference (may be outdated): [`getmininginfo`](https://zcash.github.io/rpc/getmininginfo.html)
    pub async fn get_mining_info(&self) -> Result<GetMiningInfoWire, RpcRequestError<Infallible>> {
        self.send_request("getmininginfo", ()).await
    }

    /// Returns the estimated network solutions per second based on the last n blocks.
    ///
    /// zcashd reference: [`getnetworksolps`](https://zcash.github.io/rpc/getnetworksolps.html)
    /// method: post
    /// tags: blockchain
    ///
    /// This RPC is implemented in the [mining.cpp](https://github.com/zcash/zcash/blob/d00fc6f4365048339c83f463874e4d6c240b63af/src/rpc/mining.cpp#L104)
    /// file of the Zcash repository. The Zebra implementation can be found [here](https://github.com/ZcashFoundation/zebra/blob/19bca3f1159f9cb9344c9944f7e1cb8d6a82a07f/zebra-rpc/src/methods.rs#L2687).
    ///
    /// # Parameters
    ///
    /// - `blocks`: (number, optional, default=120) Number of blocks, or -1 for blocks over difficulty averaging window.
    /// - `height`: (number, optional, default=-1) To estimate network speed at the time of a specific block height.
    pub async fn get_network_sol_ps(
        &self,
        blocks: Option<i32>,
        height: Option<i32>,
    ) -> Result<GetNetworkSolPsResponse, RpcRequestError<Infallible>> {
        let mut params = Vec::new();

        // check whether the blocks parameter is present
        if let Some(b) = blocks {
            params.push(serde_json::json!(b));
        } else {
            params.push(serde_json::json!(120_i32))
        }

        // check whether the height parameter is present
        if let Some(h) = height {
            params.push(serde_json::json!(h));
        } else {
            // default to -1
            params.push(serde_json::json!(-1_i32))
        }

        self.send_request("getnetworksolps", params).await
    }
}

/// Tests connection with zebrad / zebrad.
async fn test_node_connection(url: Url, auth_method: AuthMethod) -> Result<(), TransportError> {
    let client = Client::builder()
        .connect_timeout(std::time::Duration::from_secs(2))
        .timeout(std::time::Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;

    let request_body = r#"{"jsonrpc":"2.0","method":"getinfo","params":[],"id":1}"#;
    let mut request_builder = client
        .post(url.clone())
        .header("Content-Type", "application/json")
        .body(request_body);

    match &auth_method {
        AuthMethod::Basic { username, password } => {
            request_builder = request_builder.basic_auth(username, Some(password));
        }
        AuthMethod::Cookie { cookie } => {
            request_builder = request_builder.header(
                reqwest::header::AUTHORIZATION,
                format!(
                    "Basic {}",
                    general_purpose::STANDARD.encode(format!("__cookie__:{cookie}"))
                ),
            );
        }
    }

    let response = request_builder
        .send()
        .await
        .map_err(TransportError::ReqwestError)?;
    let body_bytes = response
        .bytes()
        .await
        .map_err(TransportError::ReqwestError)?;
    let _response: RpcResponse<serde_json::Value> = serde_json::from_slice(&body_bytes)
        .map_err(|e| TransportError::BadNodeData(Box::new(e), ""))?;
    Ok(())
}

/// Tries to connect to zebrad/zcashd using the provided SocketAddr and returns the correct URL.
pub async fn test_node_and_return_url(
    addr: SocketAddr,
    cookie_path: Option<PathBuf>,
    user: Option<String>,
    password: Option<String>,
) -> Result<Url, TransportError> {
    let auth_method = match cookie_path.is_some() {
        true => {
            let cookie_file_path_str = cookie_path.expect("validator rpc cookie path missing");
            let cookie_password = read_and_parse_cookie_token(Path::new(&cookie_file_path_str))?;
            AuthMethod::Cookie {
                cookie: cookie_password,
            }
        }
        false => AuthMethod::Basic {
            username: user.unwrap_or_else(|| "xxxxxx".to_string()),
            password: password.unwrap_or_else(|| "xxxxxx".to_string()),
        },
    };

    let host = match addr {
        SocketAddr::V4(_) => addr.ip().to_string(),
        SocketAddr::V6(_) => format!("[{}]", addr.ip()),
    };

    let url: Url = format!("http://{}:{}", host, addr.port()).parse()?;

    let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(500));
    for _ in 0..3 {
        match test_node_connection(url.clone(), auth_method.clone()).await {
            Ok(_) => {
                return Ok(url);
            }
            Err(_) => {
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            }
        }
        interval.tick().await;
    }
    error!("Error: Could not establish connection with node. Please check config and confirm node is listening at the correct address and the correct authorisation details have been entered. Exiting..");
    std::process::exit(1);
}
