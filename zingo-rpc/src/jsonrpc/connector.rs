//! JsonRPC client implementation.

use http::Uri;
use hyper::{http, Body, Client, Request};
use hyper_tls::HttpsConnector;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use zebra_rpc::methods::{GetBlockChainInfo, GetBlockHash, GetInfo, SentTransactionHash};

use super::primitives::{
    AddressStringsRequest, GetBalanceResponse, GetBlockRequest, GetBlockResponse,
    GetSubtreesRequest, GetSubtreesResponse, GetTransactionRequest, GetTransactionResponse,
    GetTreestateRequest, GetTreestateResponse, GetUtxosResponse, SendTransactionRequest,
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

/// JsonRPC Client config data.
pub struct JsonRpcConnector {
    uri: http::Uri,
}

impl JsonRpcConnector {
    /// Returns a new JsonRpcConnector instance.
    pub fn new(uri: http::Uri) -> Self {
        Self { uri }
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
        id: i32,
    ) -> Result<R, Box<dyn std::error::Error>> {
        let client = Client::builder().build(HttpsConnector::new());
        let req = RpcRequest {
            jsonrpc: "2.0".to_string(),
            method: method.to_string(),
            params,
            id,
        };
        let request = Request::builder()
            .method("POST")
            .uri(self.uri.clone())
            .header("Content-Type", "application/json")
            .body(Body::from(serde_json::to_string(&req)?))?;

        let response = client.request(request).await?;
        let body_bytes = hyper::body::to_bytes(response.into_body()).await?;
        let response: RpcResponse<R> = serde_json::from_slice(&body_bytes)?;

        match response.error {
            Some(error) => Err(format!("RPC Error {}: {}", error.code, error.message).into()),
            None => Ok(response.result),
        }
    }

    /// Returns software information from the RPC server, as a [`GetInfo`] JSON struct.
    ///
    /// zcashd reference: [`getinfo`](https://zcash.github.io/rpc/getinfo.html)
    /// method: post
    /// tags: control
    pub async fn get_info(&self, id: i32) -> Result<GetInfo, Box<dyn std::error::Error>> {
        self.send_request::<(), GetInfo>("getinfo", (), id).await
    }

    /// Returns blockchain state information, as a [`GetBlockChainInfo`] JSON struct.
    ///
    /// zcashd reference: [`getblockchaininfo`](https://zcash.github.io/rpc/getblockchaininfo.html)
    /// method: post
    /// tags: blockchain
    pub async fn get_blockchain_info(
        &self,
        id: i32,
    ) -> Result<GetBlockChainInfo, Box<dyn std::error::Error>> {
        self.send_request::<(), GetBlockChainInfo>("getblockchaininfo", (), id)
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
        id: i32,
    ) -> Result<GetBalanceResponse, Box<dyn std::error::Error>> {
        let params = AddressStringsRequest { addresses };
        self.send_request("getaddressbalance", params, id).await
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
        id: i32,
    ) -> Result<SentTransactionHash, Box<dyn std::error::Error>> {
        let params = SendTransactionRequest {
            raw_transaction_hex,
        };
        self.send_request("sendrawtransaction", params, id).await
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
        id: i32,
    ) -> Result<GetBlockResponse, Box<dyn std::error::Error>> {
        let params = GetBlockRequest {
            hash_or_height,
            verbosity,
        };
        self.send_request("getblock", params, id).await
    }

    /// Returns the hash of the current best blockchain tip block, as a [`GetBlockHash`] JSON string.
    ///
    /// zcashd reference: [`getbestblockhash`](https://zcash.github.io/rpc/getbestblockhash.html)
    /// method: post
    /// tags: blockchain
    pub async fn get_best_block_hash(
        &self,
        id: i32,
    ) -> Result<GetBlockHash, Box<dyn std::error::Error>> {
        self.send_request::<(), GetBlockHash>("getbestblockhash", (), id)
            .await
    }

    /// Returns all transaction ids in the memory pool, as a JSON array.
    ///
    /// zcashd reference: [`getrawmempool`](https://zcash.github.io/rpc/getrawmempool.html)
    /// method: post
    /// tags: blockchain
    pub async fn get_raw_mempool(
        &self,
        id: i32,
    ) -> Result<TxidsResponse, Box<dyn std::error::Error>> {
        self.send_request::<(), TxidsResponse>("getrawmempool", (), id)
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
        id: i32,
    ) -> Result<GetTreestateResponse, Box<dyn std::error::Error>> {
        let params = GetTreestateRequest { hash_or_height };
        self.send_request("z_gettreestate", params, id).await
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
        id: i32,
    ) -> Result<GetSubtreesResponse, Box<dyn std::error::Error>> {
        let params = GetSubtreesRequest {
            pool,
            start_index,
            limit,
        };
        self.send_request("z_getsubtreesbyindex", params, id).await
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
        id: i32,
    ) -> Result<GetTransactionResponse, Box<dyn std::error::Error>> {
        let params = GetTransactionRequest { txid_hex, verbose };
        self.send_request("getrawtransaction", params, id).await
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
        id: i32,
    ) -> Result<TxidsResponse, Box<dyn std::error::Error>> {
        let params = TxidsByAddressRequest {
            addresses,
            start,
            end,
        };

        self.send_request("getaddresstxids", params, id).await
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
        id: i32,
    ) -> Result<Vec<GetUtxosResponse>, Box<dyn std::error::Error>> {
        let params = AddressStringsRequest { addresses };
        self.send_request("getaddressutxos", params, id).await
    }
}
