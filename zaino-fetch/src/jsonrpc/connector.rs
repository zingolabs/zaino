//! JsonRPC client implementation.
//!
//! TODO: - Add option for http connector.
//!       - Refactor JsonRpcConnectorError into concrete error types and implement fmt::display [https://github.com/zingolabs/zaino/issues/67].

use base64::{engine::general_purpose, Engine};
use http::Uri;
use reqwest::{Client, ClientBuilder, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
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

use crate::jsonrpc::{
    error::JsonRpcConnectorError,
    response::{
        GetBalanceResponse, GetBlockResponse, GetBlockchainInfoResponse, GetInfoResponse,
        GetSubtreesResponse, GetTransactionResponse, GetTreestateResponse, GetUtxosResponse,
        SendTransactionResponse, TxidsResponse,
    },
};

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
    result: T,
    error: Option<RpcError>,
}

/// Json RPC Error type.
#[derive(Serialize, Deserialize, Debug)]
pub struct RpcError {
    /// Error Code.
    pub code: i64,
    /// Error Message.
    pub message: String,
    /// Error Data.
    pub data: Option<Value>,
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
                .map(|raw| serde_json::from_str(raw.get()).unwrap_or_default()),
        }
    }
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RPC Error (code: {}): {}", self.code, self.message)
    }
}

impl std::error::Error for RpcError {}

#[derive(Debug, Clone)]
enum AuthMethod {
    Basic { username: String, password: String },
    Cookie { cookie: String },
}

/// JsonRPC Client config data.
#[derive(Debug, Clone)]
pub struct JsonRpcConnector {
    url: Url,
    id_counter: Arc<AtomicI32>,
    client: Client,
    auth_method: AuthMethod,
}

impl JsonRpcConnector {
    /// Creates a new JsonRpcConnector with Basic Authentication.
    pub fn new_with_basic_auth(
        url: Url,
        username: String,
        password: String,
    ) -> Result<Self, JsonRpcConnectorError> {
        let client = ClientBuilder::new()
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(JsonRpcConnectorError::ReqwestError)?;

        Ok(Self {
            url,
            id_counter: Arc::new(AtomicI32::new(0)),
            client,
            auth_method: AuthMethod::Basic { username, password },
        })
    }

    /// Creates a new JsonRpcConnector with Cookie Authentication.
    pub fn new_with_cookie_auth(
        url: Url,
        cookie_path: &Path,
    ) -> Result<Self, JsonRpcConnectorError> {
        let cookie_content =
            fs::read_to_string(cookie_path).map_err(JsonRpcConnectorError::IoError)?;
        let cookie = cookie_content.trim().to_string();

        let client = ClientBuilder::new()
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .cookie_store(true)
            .build()
            .map_err(JsonRpcConnectorError::ReqwestError)?;

        Ok(Self {
            url,
            id_counter: Arc::new(AtomicI32::new(0)),
            client,
            auth_method: AuthMethod::Cookie { cookie },
        })
    }

    /// Helper function to create from parts of a StateServiceConfig or
    /// FetchServiceConfig
    pub async fn new_from_config_parts(
        validator_cookie_auth: bool,
        validator_rpc_address: SocketAddr,
        validator_rpc_user: String,
        validator_rpc_password: String,
        validator_cookie_path: Option<String>,
    ) -> Result<Self, JsonRpcConnectorError> {
        match validator_cookie_auth {
            true => JsonRpcConnector::new_with_cookie_auth(
                test_node_and_return_url(
                    validator_rpc_address,
                    validator_cookie_auth,
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
            false => JsonRpcConnector::new_with_basic_auth(
                test_node_and_return_url(
                    validator_rpc_address,
                    false,
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

    /// Returns the http::uri the JsonRpcConnector is configured to send requests to.
    pub fn uri(&self) -> Result<Uri, JsonRpcConnectorError> {
        Ok(self.url.as_str().parse()?)
    }

    /// Returns the reqwest::url the JsonRpcConnector is configured to send requests to.
    pub fn url(&self) -> Url {
        self.url.clone()
    }

    /// Sends a jsonRPC request and returns the response.
    ///
    /// NOTE: This function currently resends the call up to 5 times on a server response of "Work queue depth exceeded".
    ///       This is because the node's queue can become overloaded and stop servicing RPCs.
    async fn send_request<
        T: std::fmt::Debug + Serialize,
        R: std::fmt::Debug + for<'de> Deserialize<'de>,
    >(
        &self,
        method: &str,
        params: T,
    ) -> Result<R, JsonRpcConnectorError> {
        let id = self.id_counter.fetch_add(1, Ordering::SeqCst);
        let req = RpcRequest {
            jsonrpc: "2.0".to_string(),
            method: method.to_string(),
            params,
            id,
        };
        let max_attempts = 5;
        let mut attempts = 0;
        loop {
            attempts += 1;
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

            let request_body =
                serde_json::to_string(&req).map_err(JsonRpcConnectorError::SerdeJsonError)?;

            let response = request_builder
                .body(request_body)
                .send()
                .await
                .map_err(JsonRpcConnectorError::ReqwestError)?;

            let status = response.status();

            let body_bytes = response
                .bytes()
                .await
                .map_err(JsonRpcConnectorError::ReqwestError)?;

            let body_str = String::from_utf8_lossy(&body_bytes);

            if body_str.contains("Work queue depth exceeded") {
                if attempts >= max_attempts {
                    return Err(JsonRpcConnectorError::new(
                        "Error: The node's rpc queue depth was exceeded after multiple attempts",
                    ));
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                continue;
            }

            if !status.is_success() {
                return Err(JsonRpcConnectorError::new(format!(
                    "Error: Error status from node's rpc server: {}, {}",
                    status, body_str
                )));
            }

            let response: RpcResponse<R> = serde_json::from_slice(&body_bytes)
                .map_err(JsonRpcConnectorError::SerdeJsonError)?;

            return match response.error {
                Some(error) => Err(JsonRpcConnectorError::new(format!(
                    "Error: Error from node's rpc server: {} - {}",
                    error.code, error.message
                ))),
                None => Ok(response.result),
            };
        }
    }

    /// Returns software information from the RPC server, as a [`GetInfo`] JSON struct.
    ///
    /// zcashd reference: [`getinfo`](https://zcash.github.io/rpc/getinfo.html)
    /// method: post
    /// tags: control
    pub async fn get_info(&self) -> Result<GetInfoResponse, JsonRpcConnectorError> {
        self.send_request::<(), GetInfoResponse>("getinfo", ())
            .await
    }

    /// Returns blockchain state information, as a [`GetBlockChainInfo`] JSON struct.
    ///
    /// zcashd reference: [`getblockchaininfo`](https://zcash.github.io/rpc/getblockchaininfo.html)
    /// method: post
    /// tags: blockchain
    pub async fn get_blockchain_info(
        &self,
    ) -> Result<GetBlockchainInfoResponse, JsonRpcConnectorError> {
        self.send_request::<(), GetBlockchainInfoResponse>("getblockchaininfo", ())
            .await
    }

    /// Returns the total balance of a provided `addresses` in an [`AddressBalance`] instance.
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
    ) -> Result<GetBalanceResponse, JsonRpcConnectorError> {
        let params = vec![serde_json::json!({ "addresses": addresses })];
        self.send_request("getaddressbalance", params).await
    }

    /// Sends the raw bytes of a signed transaction to the local node's mempool, if the transaction is valid.
    /// Returns the [`SentTransactionHash`] for the transaction, as a JSON string.
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
    ) -> Result<SendTransactionResponse, JsonRpcConnectorError> {
        let params = vec![serde_json::to_value(raw_transaction_hex)?];
        self.send_request("sendrawtransaction", params).await
    }

    /// Returns the requested block by hash or height, as a [`GetBlock`] JSON string.
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
    ) -> Result<GetBlockResponse, JsonRpcConnectorError> {
        let params = match verbosity {
            Some(v) => vec![
                serde_json::to_value(hash_or_height)?,
                serde_json::to_value(v)?,
            ],
            None => vec![
                serde_json::to_value(hash_or_height)?,
                serde_json::to_value(1)?,
            ],
        };
        self.send_request("getblock", params).await
    }

    /// Returns all transaction ids in the memory pool, as a JSON array.
    ///
    /// zcashd reference: [`getrawmempool`](https://zcash.github.io/rpc/getrawmempool.html)
    /// method: post
    /// tags: blockchain
    pub async fn get_raw_mempool(&self) -> Result<TxidsResponse, JsonRpcConnectorError> {
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
    ) -> Result<GetTreestateResponse, JsonRpcConnectorError> {
        let params = vec![serde_json::to_value(hash_or_height)?];
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
    ) -> Result<GetSubtreesResponse, JsonRpcConnectorError> {
        let params = match limit {
            Some(v) => vec![
                serde_json::to_value(pool)?,
                serde_json::to_value(start_index)?,
                serde_json::to_value(v)?,
            ],
            None => vec![
                serde_json::to_value(pool)?,
                serde_json::to_value(start_index)?,
            ],
        };
        self.send_request("z_getsubtreesbyindex", params).await
    }

    /// Returns the raw transaction data, as a [`GetRawTransaction`] JSON string or structure.
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
    ) -> Result<GetTransactionResponse, JsonRpcConnectorError> {
        let params = match verbose {
            Some(v) => vec![serde_json::to_value(txid_hex)?, serde_json::to_value(v)?],
            None => vec![serde_json::to_value(txid_hex)?, serde_json::to_value(0)?],
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
    ) -> Result<TxidsResponse, JsonRpcConnectorError> {
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
    ) -> Result<Vec<GetUtxosResponse>, JsonRpcConnectorError> {
        let params = vec![serde_json::json!({ "addresses": addresses })];
        self.send_request("getaddressutxos", params).await
    }
}

/// Tests connection with zebrad / zebrad.
async fn test_node_connection(
    url: Url,
    auth_method: AuthMethod,
) -> Result<(), JsonRpcConnectorError> {
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
        .map_err(JsonRpcConnectorError::ReqwestError)?;
    let body_bytes = response
        .bytes()
        .await
        .map_err(JsonRpcConnectorError::ReqwestError)?;
    let _response: RpcResponse<serde_json::Value> =
        serde_json::from_slice(&body_bytes).map_err(JsonRpcConnectorError::SerdeJsonError)?;
    Ok(())
}

/// Tries to connect to zebrad/zcashd using the provided SocketAddr and returns the correct URL.
pub async fn test_node_and_return_url(
    addr: SocketAddr,
    rpc_cookie_auth: bool,
    cookie_path: Option<String>,
    user: Option<String>,
    password: Option<String>,
) -> Result<Url, JsonRpcConnectorError> {
    let auth_method = match rpc_cookie_auth {
        true => {
            let cookie_content =
                fs::read_to_string(cookie_path.expect("validator rpc cookie path missing"))
                    .map_err(JsonRpcConnectorError::IoError)?;
            let cookie = cookie_content.trim().to_string();

            AuthMethod::Cookie { cookie }
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
