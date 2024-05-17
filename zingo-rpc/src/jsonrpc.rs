//! JsonRPC client used to send requests to Zebrad.

use hex::{FromHex, ToHex};
use http::Uri;
use hyper::{http, Body, Client, Request};
use hyper_tls::HttpsConnector;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use zebra_chain::{
    block::{self, Height, SerializedBlock},
    subtree::NoteCommitmentSubtreeIndex,
    transaction::{self, SerializedTransaction},
    transparent,
};
use zebra_rpc::methods::{
    trees::{GetSubtrees, SubtreeRpcData},
    AddressBalance, GetAddressUtxos, GetBlock, GetBlockChainInfo, GetBlockHash, GetBlockTrees,
    GetInfo, GetRawTransaction, GetTreestate, SentTransactionHash,
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

/// List of transparent address strings.
///
/// This is used for the input parameter of [`JsonRpcConnector::get_address_balance`] and [`JsonRpcConnector::get_address_utxos`].
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
struct AddressStringsRequest {
    /// A list of transparent address strings.
    addresses: Vec<String>,
}

/// Hex-encoded raw transaction.
///
/// This is used for the input parameter of [`JsonRpcConnector::send_raw_transaction`].
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
struct SendTransactionRequest {
    /// - Hex-encoded raw transaction bytes.
    raw_transaction_hex: String,
}

/// Block to be fetched.
///
/// This is used for the input parameter of [`JsonRpcConnector::get_block`].
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
struct GetBlockRequest {
    /// The hash or height for the block to be returned.
    hash_or_height: String,
    /// 0 for hex encoded data, 1 for a json object, and 2 for json object with transaction data. Default=1.
    verbosity: Option<u8>,
}

/// Block to be examined.
///
/// This is used for the input parameter of [`JsonRpcConnector::get_treestate`].
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
struct GetTreestateRequest {
    /// The block hash or height.
    hash_or_height: String,
}

/// Subtrees to be fetched.
///
/// This is used for the input parameter of [`JsonRpcConnector::get_subtrees_by_index`].
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
struct GetSubtreesRequest {
    /// The pool from which subtrees should be returned. Either "sapling" or "orchard".
    pool: String,
    /// The index of the first 2^16-leaf subtree to return.
    start_index: u16,
    /// The maximum number of subtree values to return.
    limit: Option<u16>,
}

/// Transaction to be fetched.
///
/// This is used for the input parameter of [`JsonRpcConnector::get_raw_transaction`].
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
struct GetTransactionRequest {
    /// The transaction ID of the transaction to be returned.
    txid_hex: String,
    /// If 0, return a string of hex-encoded data, otherwise return a JSON object. Default=0.
    verbose: Option<u8>,
}

/// List of transparent address strings and range of blocks to fetch Txids from.
///
/// This is used for the input parameter of [`JsonRpcConnector::get_address_tx_ids`].
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
struct TxidsByAddressRequest {
    // A list of addresses to get transactions from.
    addresses: Vec<String>,
    // The height to start looking for transactions.
    start: u32,
    // The height to end looking for transactions.
    end: u32,
}

/// Vec of transaction ids, as a JSON array.
///
/// This is used for the output parameter of [`JsonRpcConnector::get_raw_mempool`] and [`JsonRpcConnector::get_address_tx_ids`].
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct TxidsResponse {
    /// Vec of txids.
    transactions: Vec<String>,
}

/// The transparent balance of a set of addresses.
///
/// This is used for the output parameter of [`JsonRpcConnector::get_address_balance`].
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct GetBalanceResponse {
    /// The total transparent balance.
    balance: u64,
}

/// Wrapper for `SerializedBlock` to handle hex serialization/deserialization.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HexSerializedBlock(SerializedBlock);

impl Serialize for HexSerializedBlock {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let hex_string = self.as_ref().encode_hex::<String>();
        serializer.serialize_str(&hex_string)
    }
}

impl<'de> Deserialize<'de> for HexSerializedBlock {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct HexVisitor;

        impl<'de> serde::de::Visitor<'de> for HexVisitor {
            type Value = HexSerializedBlock;

            fn expecting(&self, formatter: &mut core::fmt::Formatter) -> core::fmt::Result {
                formatter.write_str("a hex-encoded string")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                let bytes = hex::decode(value).map_err(serde::de::Error::custom)?;
                Ok(HexSerializedBlock(SerializedBlock::from(bytes)))
            }
        }

        deserializer.deserialize_str(HexVisitor)
    }
}

impl FromHex for HexSerializedBlock {
    type Error = hex::FromHexError;

    fn from_hex<T: AsRef<[u8]>>(hex: T) -> Result<Self, Self::Error> {
        hex::decode(hex)
            .map(|bytes| HexSerializedBlock(SerializedBlock::from(bytes)))
            .map_err(|e| e.into())
    }
}

impl AsRef<[u8]> for HexSerializedBlock {
    fn as_ref(&self) -> &[u8] {
        &self.0.as_ref()
    }
}

/// Contains the hex-encoded hash of the sent transaction.
///
/// This is used for the output parameter of [`JsonRpcConnector::get_block`].
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(untagged)]
pub enum GetBlockResponse {
    /// The request block, hex-encoded.
    Raw(#[serde(with = "hex")] HexSerializedBlock),
    /// The block object.
    Object {
        /// The hash of the requested block.
        hash: GetBlockHash,

        /// The number of confirmations of this block in the best chain,
        /// or -1 if it is not in the best chain.
        confirmations: i64,

        /// The height of the requested block.
        #[serde(skip_serializing_if = "Option::is_none")]
        height: Option<Height>,

        /// The height of the requested block.
        #[serde(skip_serializing_if = "Option::is_none")]
        time: Option<i64>,

        /// List of transaction IDs in block order, hex-encoded.
        tx: Vec<String>,

        /// Information about the note commitment trees.
        trees: GetBlockTrees,
    },
}

/// Zingo-Proxy commitment tree structure replicating functionality in Zebra.
///
/// A wrapper that contains either an Orchard or Sapling note commitment tree.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProxyCommitments<Tree: AsRef<[u8]>> {
    #[serde(with = "hex")]
    #[serde(rename = "finalState")]
    final_state: Tree,
}

impl<Tree: AsRef<[u8]> + FromHex<Error = hex::FromHexError>> ProxyCommitments<Tree> {
    /// Creates a new instance of `ProxyCommitments` from a hex string.
    pub fn new_from_hex(hex_encoded_data: &str) -> Result<Self, hex::FromHexError> {
        let tree = Tree::from_hex(hex_encoded_data)?;
        Ok(Self { final_state: tree })
    }

    /// Checks if the internal tree is empty.
    pub fn is_empty(&self) -> bool {
        self.final_state.as_ref().is_empty()
    }
}

/// Zingo-Proxy treestate structure replicating functionality in Zebra.
///
/// A treestate that is included in the [`z_gettreestate`][1] RPC response.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProxyTreestate<Tree: AsRef<[u8]>> {
    commitments: ProxyCommitments<Tree>,
}

impl<Tree: AsRef<[u8]> + FromHex<Error = hex::FromHexError>> ProxyTreestate<Tree> {
    /// Creates a new instance of `ProxyTreestate`.
    pub fn new(commitments: ProxyCommitments<Tree>) -> Self {
        Self { commitments }
    }

    /// Checks if the internal tree is empty.
    pub fn is_empty(&self) -> bool {
        self.commitments.is_empty()
    }
}

impl<'de, Tree: AsRef<[u8]> + FromHex<Error = hex::FromHexError> + Deserialize<'de>>
    Deserialize<'de> for ProxyTreestate<Tree>
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let hex_string: String = Deserialize::deserialize(deserializer)?;
        let tree = Tree::from_hex(&hex_string).map_err(serde::de::Error::custom)?;
        Ok(ProxyTreestate::new(ProxyCommitments { final_state: tree }))
    }
}

/// Wrapper struct for trees that need to be serialized and deserialized from hex.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HexEncodedSerializedTree<T> {
    inner: T,
}

impl<T> HexEncodedSerializedTree<T>
where
    T: From<Vec<u8>> + AsRef<[u8]>,
{
    pub fn new(inner: T) -> Self {
        Self { inner }
    }
}

impl<T> Serialize for HexEncodedSerializedTree<T>
where
    T: AsRef<[u8]> + Serialize,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let hex_string = hex::encode(self.inner.as_ref());
        serializer.serialize_str(&hex_string)
    }
}

impl<'de, T> Deserialize<'de> for HexEncodedSerializedTree<T>
where
    T: AsRef<[u8]> + From<Vec<u8>> + Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let hex_string = String::deserialize(deserializer)?;
        let bytes = hex::decode(&hex_string).map_err(serde::de::Error::custom)?;
        Ok(HexEncodedSerializedTree {
            inner: T::from(bytes),
        })
    }
}

impl<T> FromHex for HexEncodedSerializedTree<T>
where
    T: From<Vec<u8>>,
{
    type Error = hex::FromHexError;

    fn from_hex<S: AsRef<[u8]>>(hex: S) -> Result<Self, Self::Error> {
        let bytes = hex::decode(hex).map_err(serde::de::Error::custom)?;
        Ok(HexEncodedSerializedTree {
            inner: T::from(bytes),
        })
    }
}

impl<T> AsRef<[u8]> for HexEncodedSerializedTree<T>
where
    T: AsRef<[u8]>,
{
    fn as_ref(&self) -> &[u8] {
        self.inner.as_ref()
    }
}

/// Contains the hex-encoded Sapling & Orchard note commitment trees, and their
/// corresponding [`block::Hash`], [`Height`], and block time.
///
/// This is used for the output parameter of [`JsonRpcConnector::get_treestate`].
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct GetTreestateResponse {
    /// The block hash corresponding to the treestate, hex-encoded.
    #[serde(with = "hex")]
    hash: block::Hash,

    /// The block height corresponding to the treestate, numeric.
    height: Height,

    /// Unix time when the block corresponding to the treestate was mined,
    /// numeric.
    ///
    /// UTC seconds since the Unix 1970-01-01 epoch.
    time: u32,

    /// A treestate containing a Sapling note commitment tree, hex-encoded.
    #[serde(skip_serializing_if = "ProxyTreestate::is_empty")]
    sapling: ProxyTreestate<HexEncodedSerializedTree<zebra_chain::sapling::tree::SerializedTree>>,

    /// A treestate containing an Orchard note commitment tree, hex-encoded.
    #[serde(skip_serializing_if = "ProxyTreestate::is_empty")]
    orchard: ProxyTreestate<HexEncodedSerializedTree<zebra_chain::orchard::tree::SerializedTree>>,
}

/// Contains the Sapling or Orchard pool label, the index of the first subtree in the list,
/// and a list of subtree roots and end heights.
///
/// This is used for the output parameter of [`JsonRpcConnector::get_subtrees_by_index`].
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct GetSubtreesResponse {
    /// The shielded pool to which the subtrees belong.
    pub pool: String,

    /// The index of the first subtree.
    pub start_index: NoteCommitmentSubtreeIndex,

    /// A sequential list of complete subtrees, in `index` order.
    ///
    /// The generic subtree root type is a hex-encoded Sapling or Orchard subtree root string.
    // #[serde(skip_serializing_if = "Vec::is_empty")]
    pub subtrees: Vec<SubtreeRpcData>,
}

/// Contains raw transaction, encoded as hex bytes.
///
/// This is used for the output parameter of [`JsonRpcConnector::get_raw_transaction`].
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub enum GetTransactionResponse {
    /// The raw transaction, encoded as hex bytes.
    Raw(#[serde(with = "hex")] SerializedTransaction),
    /// The transaction object.
    Object {
        /// The raw transaction, encoded as hex bytes.
        #[serde(with = "hex")]
        hex: SerializedTransaction,
        /// The height of the block in the best chain that contains the transaction, or -1 if
        /// the transaction is in the mempool.
        height: i32,
        /// The confirmations of the block in the best chain that contains the transaction,
        /// or 0 if the transaction is in the mempool.
        confirmations: u32,
    },
}

/// .
///
/// This is used for the output parameter of [`JsonRpcConnector::get_address_utxos`].
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct GetUtxosResponse {
    /// The transparent address, base58check encoded
    address: transparent::Address,

    /// The output txid, in big-endian order, hex-encoded
    #[serde(with = "hex")]
    txid: transaction::Hash,

    /// The transparent output index, numeric
    #[serde(rename = "outputIndex")]
    output_index: zebra_state::OutputIndex,

    /// The transparent output script, hex encoded
    #[serde(with = "hex")]
    script: transparent::Script,

    /// The amount of zatoshis in the transparent output
    satoshis: u64,

    /// The block height, numeric.
    height: Height,
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
