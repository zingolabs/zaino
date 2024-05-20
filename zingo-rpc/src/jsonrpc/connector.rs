//! JsonRPC client implementation.

use http::Uri;
use hyper::{http, Body, Client, Request};
use hyper_tls::HttpsConnector;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::atomic::{AtomicI32, Ordering};

use super::primitives::{
    AddressStringsRequest, BestBlockHashResponse, GetBalanceResponse, GetBlockRequest,
    GetBlockResponse, GetBlockchainInfoResponse, GetInfoResponse, GetSubtreesRequest,
    GetSubtreesResponse, GetTransactionRequest, GetTransactionResponse, GetTreestateRequest,
    GetTreestateResponse, GetUtxosResponse, SendTransactionRequest, SendTransactionResponse,
    TxidsByAddressRequest, TxidsResponse,
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
    id: i32,
    jsonrpc: String,
    result: T,
    error: Option<RpcError>,
}

#[derive(Serialize, Deserialize, Debug)]
struct RpcError {
    code: i32,
    message: String,
    data: Option<Value>,
}

/// General error type for handling JsonRpcConnector errors.
#[derive(Debug)]
pub struct JsonRpcConnectorError {
    details: String,
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl JsonRpcConnectorError {
    /// Constructor for errors without an underlying source
    pub fn new(msg: impl Into<String>) -> Self {
        Self {
            details: msg.into(),
            source: None,
        }
    }

    /// Constructor for errors with an underlying source
    pub fn new_with_source(
        msg: impl Into<String>,
        source: Box<dyn std::error::Error + Send + Sync>,
    ) -> Self {
        Self {
            details: msg.into(),
            source: Some(source),
        }
    }

    /// Maps JsonRpcConnectorError to tonic::Status
    pub fn to_grpc_status(&self) -> tonic::Status {
        eprintln!("Error occurred: {}", self);

        if let Some(source) = &self.source {
            if source.is::<serde_json::Error>() {
                return tonic::Status::invalid_argument(self.to_string());
            } else if source.is::<hyper::Error>() {
                return tonic::Status::unavailable(self.to_string());
            } else if source.is::<http::Error>() {
                return tonic::Status::internal(self.to_string());
            }
        }

        tonic::Status::internal(self.to_string())
    }
}

impl std::error::Error for JsonRpcConnectorError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|e| e as &(dyn std::error::Error + 'static))
    }
}

impl std::fmt::Display for JsonRpcConnectorError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}", self.details)
    }
}

impl From<serde_json::Error> for JsonRpcConnectorError {
    fn from(err: serde_json::Error) -> Self {
        JsonRpcConnectorError::new_with_source(
            format!("Serialization/Deserialization Error: {}", err),
            Box::new(err),
        )
    }
}

impl From<hyper::Error> for JsonRpcConnectorError {
    fn from(err: hyper::Error) -> Self {
        JsonRpcConnectorError::new_with_source(
            format!("HTTP Request Error: {}", err),
            Box::new(err),
        )
    }
}

impl From<http::Error> for JsonRpcConnectorError {
    fn from(err: http::Error) -> Self {
        JsonRpcConnectorError::new_with_source(format!("HTTP Error: {}", err), Box::new(err))
    }
}

impl From<String> for JsonRpcConnectorError {
    fn from(err: String) -> Self {
        JsonRpcConnectorError::new(err)
    }
}

/// JsonRPC Client config data.
pub struct JsonRpcConnector {
    uri: http::Uri,
    id_counter: AtomicI32,
}

impl JsonRpcConnector {
    /// Returns a new JsonRpcConnector instance.
    pub fn new(uri: http::Uri) -> Self {
        Self {
            uri,
            id_counter: AtomicI32::new(0),
        }
    }

    /// Returns the uri the JsonRpcConnector is configured to send requests to.
    pub fn uri(&self) -> &Uri {
        &self.uri
    }

    /// Sends a jsonRPC request and returns the response.``
    pub async fn send_request<T: Serialize, R: for<'de> Deserialize<'de>>(
        &self,
        method: &str,
        params: T,
    ) -> Result<R, JsonRpcConnectorError> {
        let id = self.id_counter.fetch_add(1, Ordering::SeqCst);
        let client = Client::builder().build(HttpsConnector::new());
        let req = RpcRequest {
            jsonrpc: "2.0".to_string(),
            method: method.to_string(),
            params,
            id,
        };
        let request_body = serde_json::to_string(&req).map_err(|e| {
            JsonRpcConnectorError::new_with_source("Failed to serialize request", Box::new(e))
        })?;
        let request = Request::builder()
            .method("POST")
            .uri(self.uri.clone())
            .header("Content-Type", "application/json")
            .body(Body::from(request_body))
            .map_err(|e| {
                JsonRpcConnectorError::new_with_source("Failed to build request", Box::new(e))
            })?;
        let response = client.request(request).await.map_err(|e| {
            JsonRpcConnectorError::new_with_source("HTTP request failed", Box::new(e))
        })?;
        let body_bytes = hyper::body::to_bytes(response.into_body())
            .await
            .map_err(|e| {
                JsonRpcConnectorError::new_with_source("Failed to read response body", Box::new(e))
            })?;

        // Test Code!!!
        let body_str = String::from_utf8(body_bytes.to_vec()).map_err(|e| {
            JsonRpcConnectorError::new_with_source(
                "Failed to convert response body to string",
                Box::new(e),
            )
        })?;
        println!("Raw response body: {}", body_str);

        let response: RpcResponse<R> = serde_json::from_slice(&body_bytes).map_err(|e| {
            JsonRpcConnectorError::new_with_source("Failed to deserialize response", Box::new(e))
        })?;

        match response.error {
            Some(error) => Err(JsonRpcConnectorError::new(format!(
                "RPC Error {}: {}",
                error.code, error.message
            ))),
            None => Ok(response.result),
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
        let params = AddressStringsRequest { addresses };
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
        let params = SendTransactionRequest {
            raw_transaction_hex,
        };
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
        let params = GetBlockRequest {
            hash_or_height,
            verbosity,
        };
        self.send_request("getblock", params).await
    }

    /// Returns the hash of the current best blockchain tip block, as a [`GetBlockHash`] JSON string.
    ///
    /// zcashd reference: [`getbestblockhash`](https://zcash.github.io/rpc/getbestblockhash.html)
    /// method: post
    /// tags: blockchain
    pub async fn get_best_block_hash(
        &self,
    ) -> Result<BestBlockHashResponse, JsonRpcConnectorError> {
        self.send_request::<(), BestBlockHashResponse>("getbestblockhash", ())
            .await
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
        let params = GetTreestateRequest { hash_or_height };
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
        let params = GetSubtreesRequest {
            pool,
            start_index,
            limit,
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
        let params = GetTransactionRequest { txid_hex, verbose };
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
    pub async fn get_address_tx_ids(
        &self,
        addresses: Vec<String>,
        start: u32,
        end: u32,
    ) -> Result<TxidsResponse, JsonRpcConnectorError> {
        let params = TxidsByAddressRequest {
            addresses,
            start,
            end,
        };

        self.send_request("getaddresstxids", params).await
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
        let params = AddressStringsRequest { addresses };
        self.send_request("getaddressutxos", params).await
    }
}
