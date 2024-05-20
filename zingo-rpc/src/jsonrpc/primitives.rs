//! Request and response types for jsonRPC client.

use hex::{FromHex, ToHex};
use indexmap::IndexMap;
use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize};

use zebra_chain::{
    block::{self, Height, SerializedBlock},
    subtree::NoteCommitmentSubtreeIndex,
    transaction::{self, SerializedTransaction},
    transparent,
};
use zebra_rpc::methods::{GetBlockHash, GetBlockTrees};

/// List of transparent address strings.
///
/// This is used for the input parameter of [`JsonRpcConnector::get_address_balance`] and [`JsonRpcConnector::get_address_utxos`].
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AddressStringsRequest {
    /// A list of transparent address strings.
    pub addresses: Vec<String>,
}

impl AddressStringsRequest {
    /// Creates a new `AddressStrings` given a vector.
    #[cfg(test)]
    pub fn new(addresses: Vec<String>) -> Self {
        AddressStringsRequest { addresses }
    }

    /// Given a list of addresses as strings:
    /// - check if provided list have all valid transparent addresses.
    /// - return valid addresses as a set of `Address`.
    pub fn valid_addresses(
        self,
    ) -> jsonrpc_core::Result<std::collections::HashSet<zebra_chain::transparent::Address>> {
        let valid_addresses: std::collections::HashSet<zebra_chain::transparent::Address> = self
            .addresses
            .into_iter()
            .map(|address| {
                address.parse().map_err(|error| {
                    jsonrpc_core::Error::invalid_params(format!(
                        "invalid address {address:?}: {error}"
                    ))
                })
            })
            .collect::<jsonrpc_core::Result<_>>()?;

        Ok(valid_addresses)
    }
}

/// Hex-encoded raw transaction.
///
/// This is used for the input parameter of [`JsonRpcConnector::send_raw_transaction`].
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SendTransactionRequest {
    /// - Hex-encoded raw transaction bytes.
    pub raw_transaction_hex: String,
}

/// Block to be fetched.
///
/// This is used for the input parameter of [`JsonRpcConnector::get_block`].
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct GetBlockRequest {
    /// The hash or height for the block to be returned.
    pub hash_or_height: String,
    /// 0 for hex encoded data, 1 for a json object, and 2 for json object with transaction data. Default=1.
    pub verbosity: Option<u8>,
}

/// Block to be examined.
///
/// This is used for the input parameter of [`JsonRpcConnector::get_treestate`].
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct GetTreestateRequest {
    /// The block hash or height.
    pub hash_or_height: String,
}

/// Subtrees to be fetched.
///
/// This is used for the input parameter of [`JsonRpcConnector::get_subtrees_by_index`].
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct GetSubtreesRequest {
    /// The pool from which subtrees should be returned. Either "sapling" or "orchard".
    pub pool: String,
    /// The index of the first 2^16-leaf subtree to return.
    pub start_index: u16,
    /// The maximum number of subtree values to return.
    pub limit: Option<u16>,
}

/// Transaction to be fetched.
///
/// This is used for the input parameter of [`JsonRpcConnector::get_raw_transaction`].
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct GetTransactionRequest {
    /// The transaction ID of the transaction to be returned.
    pub txid_hex: String,
    /// If 0, return a string of hex-encoded data, otherwise return a JSON object. Default=0.
    pub verbose: Option<u8>,
}

/// List of transparent address strings and range of blocks to fetch Txids from.
///
/// This is used for the input parameter of [`JsonRpcConnector::get_address_tx_ids`].
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TxidsByAddressRequest {
    /// A list of addresses to get transactions from.
    pub addresses: Vec<String>,
    /// The height to start looking for transactions.
    pub start: u32,
    /// The height to end looking for transactions.
    pub end: u32,
}

/// Response to a `getinfo` RPC request.
///
/// This is used for the output parameter of [`JsonRpcConnector::get_info`].
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct GetInfoResponse {
    /// The node version build number
    pub build: String,
    /// The server sub-version identifier, used as the network protocol user-agent
    pub subversion: String,
}

/// A hex-encoded [`ConsensusBranchId`] string.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ProxyConsensusBranchIdHex(
    #[serde(with = "hex")] pub zebra_chain::parameters::ConsensusBranchId,
);

/// The activation status of a [`NetworkUpgrade`].
#[derive(Copy, Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ProxyNetworkUpgradeStatus {
    /// The network upgrade is currently active.
    ///
    /// Includes all network upgrades that have previously activated,
    /// even if they are not the most recent network upgrade.
    #[serde(rename = "active")]
    Active,

    /// The network upgrade does not have an activation height.
    #[serde(rename = "disabled")]
    Disabled,

    /// The network upgrade has an activation height, but we haven't reached it yet.
    #[serde(rename = "pending")]
    Pending,
}

/// Information about [`NetworkUpgrade`] activation.
#[derive(Copy, Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ProxyNetworkUpgradeInfo {
    /// Name of upgrade, string.
    pub name: zebra_chain::parameters::NetworkUpgrade,

    /// Block height of activation, numeric.
    #[serde(rename = "activationheight")]
    pub activation_height: Height,

    /// Status of upgrade, string.
    pub status: ProxyNetworkUpgradeStatus,
}

/// The [`ConsensusBranchId`]s for the tip and the next block.
///
/// These branch IDs are different when the next block is a network upgrade activation block.
#[derive(Copy, Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ProxyTipConsensusBranch {
    /// Branch ID used to validate the current chain tip, big-endian, hex-encoded.
    #[serde(rename = "chaintip")]
    pub chain_tip: ProxyConsensusBranchIdHex,

    /// Branch ID used to validate the next block, big-endian, hex-encoded.
    #[serde(rename = "nextblock")]
    pub next_block: ProxyConsensusBranchIdHex,
}

/// Response to a `getblockchaininfo` RPC request.
///
/// This is used for the output parameter of [`JsonRpcConnector::get_blockchain_info`].
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct GetBlockchainInfoResponse {
    /// Current network name as defined in BIP70 (main, test, regtest)
    pub chain: String,

    /// The current number of blocks processed in the server, numeric
    pub blocks: Height,

    /// The hash of the currently best block, in big-endian order, hex-encoded
    #[serde(rename = "bestblockhash", with = "hex")]
    pub best_block_hash: block::Hash,

    /// If syncing, the estimated height of the chain, else the current best height, numeric.
    ///
    /// In Zebra, this is always the height estimate, so it might be a little inaccurate.
    #[serde(rename = "estimatedheight")]
    pub estimated_height: Height,

    /// Status of network upgrades
    pub upgrades: IndexMap<ProxyConsensusBranchIdHex, ProxyNetworkUpgradeInfo>,

    /// Branch IDs of the current and upcoming consensus rules
    pub consensus: ProxyTipConsensusBranch,
}

/// The transparent balance of a set of addresses.
///
/// This is used for the output parameter of [`JsonRpcConnector::get_address_balance`].
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct GetBalanceResponse {
    /// The total transparent balance.
    pub balance: u64,
}

/// Contains the hex-encoded hash of the sent transaction.
///
/// This is used for the output parameter of [`JsonRpcConnector::send_raw_transaction`].
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct SendTransactionResponse(#[serde(with = "hex")] transaction::Hash);

/// Wrapper for `SerializedBlock` to handle hex serialization/deserialization.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProxySerializedBlock(SerializedBlock);

impl Serialize for ProxySerializedBlock {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let hex_string = self.as_ref().encode_hex::<String>();
        serializer.serialize_str(&hex_string)
    }
}

impl<'de> Deserialize<'de> for ProxySerializedBlock {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct HexVisitor;

        impl<'de> serde::de::Visitor<'de> for HexVisitor {
            type Value = ProxySerializedBlock;

            fn expecting(&self, formatter: &mut core::fmt::Formatter) -> core::fmt::Result {
                formatter.write_str("a hex-encoded string")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                let bytes = hex::decode(value).map_err(serde::de::Error::custom)?;
                Ok(ProxySerializedBlock(SerializedBlock::from(bytes)))
            }
        }

        deserializer.deserialize_str(HexVisitor)
    }
}

impl FromHex for ProxySerializedBlock {
    type Error = hex::FromHexError;

    fn from_hex<T: AsRef<[u8]>>(hex: T) -> Result<Self, Self::Error> {
        hex::decode(hex)
            .map(|bytes| ProxySerializedBlock(SerializedBlock::from(bytes)))
            .map_err(|e| e.into())
    }
}

impl AsRef<[u8]> for ProxySerializedBlock {
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
    Raw(#[serde(with = "hex")] ProxySerializedBlock),
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

/// Contains the hex-encoded hash of the requested block.
///
/// This is used for the output parameter of [`JsonRpcConnector::get_best_block_hash`].
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(transparent)]
pub struct BestBlockHashResponse(#[serde(with = "hex")] pub block::Hash);

/// Vec of transaction ids, as a JSON array.
///
/// This is used for the output parameter of [`JsonRpcConnector::get_raw_mempool`] and [`JsonRpcConnector::get_address_tx_ids`].
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct TxidsResponse {
    /// Vec of txids.
    pub transactions: Vec<String>,
}

/// Zingo-Proxy commitment tree structure replicating functionality in Zebra.
///
/// A wrapper that contains either an Orchard or Sapling note commitment tree.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProxyCommitments<Tree: AsRef<[u8]>> {
    #[serde(with = "hex")]
    #[serde(rename = "finalState")]
    pub final_state: Tree,
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

/// A serialized Sapling note commitment tree
///
/// Replicates functionality used in Zebra.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ProxySerializedTree(Vec<u8>);

impl FromHex for ProxySerializedTree {
    type Error = hex::FromHexError;

    fn from_hex<T: AsRef<[u8]>>(hex: T) -> Result<Self, Self::Error> {
        let bytes = hex::decode(hex)?;
        Ok(ProxySerializedTree(bytes))
    }
}

impl AsRef<[u8]> for ProxySerializedTree {
    fn as_ref(&self) -> &[u8] {
        &self.0
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
    pub hash: block::Hash,

    /// The block height corresponding to the treestate, numeric.
    pub height: Height,

    /// Unix time when the block corresponding to the treestate was mined,
    /// numeric.
    ///
    /// UTC seconds since the Unix 1970-01-01 epoch.
    pub time: u32,

    /// A treestate containing a Sapling note commitment tree, hex-encoded.
    #[serde(skip_serializing_if = "ProxyTreestate::is_empty")]
    pub sapling: ProxyTreestate<ProxySerializedTree>,

    /// A treestate containing an Orchard note commitment tree, hex-encoded.
    #[serde(skip_serializing_if = "ProxyTreestate::is_empty")]
    pub orchard: ProxyTreestate<ProxySerializedTree>,
}

/// Wrapper type that can hold Sapling or Orchard subtree roots with hex encoding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProxySubtreeRpcData {
    /// Merkle root of the 2^16-leaf subtree.
    pub root: String,
    /// Height of the block containing the note that completed this subtree.
    pub height: Height,
}

impl ProxySubtreeRpcData {
    /// Returns new instance of ProxySubtreeRpcData
    pub fn new(root: String, height: Height) -> Self {
        Self { root, height }
    }
}

impl serde::Serialize for ProxySubtreeRpcData {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut state = serializer.serialize_struct("ProxySubtreeRpcData", 2)?;
        state.serialize_field("root", &self.root)?;
        state.serialize_field("height", &self.height)?;
        state.end()
    }
}

impl<'de> serde::Deserialize<'de> for ProxySubtreeRpcData {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Inner {
            root: String,
            height: Height,
        }

        let inner = Inner::deserialize(deserializer)?;
        Ok(ProxySubtreeRpcData {
            root: inner.root,
            height: inner.height,
        })
    }
}

impl FromHex for ProxySubtreeRpcData {
    type Error = hex::FromHexError;

    fn from_hex<T: AsRef<[u8]>>(hex: T) -> Result<Self, Self::Error> {
        let hex_str = std::str::from_utf8(hex.as_ref())
            .map_err(|_| hex::FromHexError::InvalidHexCharacter { c: '�', index: 0 })?;

        if hex_str.len() < 8 {
            return Err(hex::FromHexError::OddLength);
        }

        let root_end_index = hex_str.len() - 8;
        let (root_hex, height_hex) = hex_str.split_at(root_end_index);

        let root = root_hex.to_string();
        let height = u32::from_str_radix(height_hex, 16)
            .map_err(|_| hex::FromHexError::InvalidHexCharacter { c: '�', index: 0 })?;

        Ok(ProxySubtreeRpcData {
            root,
            height: Height(height),
        })
    }
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
    pub subtrees: Vec<ProxySubtreeRpcData>,
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

/// Zingo-Proxy encoding of a Bitcoin script.
#[derive(Clone, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ProxyScript {
    /// # Correctness
    ///
    /// Consensus-critical serialization uses [`ZcashSerialize`].
    /// [`serde`]-based hex serialization must only be used for RPCs and testing.
    #[serde(with = "hex")]
    pub script: Vec<u8>,
}

impl ProxyScript {
    /// Create a new Bitcoin script from its raw bytes.
    /// The raw bytes must not contain the length prefix.
    pub fn new(raw_bytes: &[u8]) -> Self {
        Self {
            script: raw_bytes.to_vec(),
        }
    }

    /// Return the raw bytes of the script without the length prefix.
    ///
    /// # Correctness
    ///
    /// These raw bytes do not have a length prefix.
    /// The Zcash serialization format requires a length prefix; use `zcash_serialize`
    /// and `zcash_deserialize` to create byte data with a length prefix.
    pub fn as_raw_bytes(&self) -> &[u8] {
        &self.script
    }
}

impl core::fmt::Display for ProxyScript {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        f.write_str(&self.encode_hex::<String>())
    }
}

impl core::fmt::Debug for ProxyScript {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        f.debug_tuple("Script")
            .field(&hex::encode(&self.script))
            .finish()
    }
}

impl ToHex for &ProxyScript {
    fn encode_hex<T: FromIterator<char>>(&self) -> T {
        self.as_raw_bytes().encode_hex()
    }

    fn encode_hex_upper<T: FromIterator<char>>(&self) -> T {
        self.as_raw_bytes().encode_hex_upper()
    }
}

impl ToHex for ProxyScript {
    fn encode_hex<T: FromIterator<char>>(&self) -> T {
        (&self).encode_hex()
    }

    fn encode_hex_upper<T: FromIterator<char>>(&self) -> T {
        (&self).encode_hex_upper()
    }
}

impl FromHex for ProxyScript {
    type Error = hex::FromHexError;

    fn from_hex<T: AsRef<[u8]>>(hex: T) -> Result<Self, Self::Error> {
        let bytes = Vec::from_hex(hex)?;
        Ok(Self { script: bytes })
    }
}

/// .
///
/// This is used for the output parameter of [`JsonRpcConnector::get_address_utxos`].
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct GetUtxosResponse {
    /// The transparent address, base58check encoded
    pub address: transparent::Address,

    /// The output txid, in big-endian order, hex-encoded
    #[serde(with = "hex")]
    pub txid: transaction::Hash,

    /// The transparent output index, numeric
    #[serde(rename = "outputIndex")]
    pub output_index: zebra_state::OutputIndex,

    /// The transparent output script, hex encoded
    #[serde(with = "hex")]
    pub script: ProxyScript,

    /// The amount of zatoshis in the transparent output
    pub satoshis: u64,

    /// The block height, numeric.
    pub height: Height,
}
