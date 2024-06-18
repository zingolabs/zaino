//! Request and response types for jsonRPC client.

use hex::{FromHex, ToHex};
use indexmap::IndexMap;
use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize};

use zebra_chain::{
    block::{self, Height, SerializedBlock},
    subtree::NoteCommitmentSubtreeIndex,
    transaction::{self},
    transparent,
};
use zebra_rpc::methods::GetBlockHash;

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
pub struct SendTransactionResponse(#[serde(with = "hex")] pub transaction::Hash);

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

/// Sapling note commitment tree information.
#[derive(Copy, Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ProxyTree {
    /// Commitment tree size.
    pub size: u64,
}

/// Information about the note commitment trees.
#[derive(Copy, Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ProxyBlockTrees {
    /// Sapling commitment tree size.
    pub sapling: ProxyTree,
    /// Orchard commitment tree size.
    pub orchard: ProxyTree,
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
        trees: ProxyBlockTrees, //zebra_rpc::methods::GetBlockTrees,
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
/// This is used for the output parameter of [`JsonRpcConnector::get_raw_mempool`] and [`JsonRpcConnector::get_address_txids`].
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct TxidsResponse {
    /// Vec of txids.
    pub transactions: Vec<String>,
}

impl<'de> Deserialize<'de> for TxidsResponse {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let v = serde_json::Value::deserialize(deserializer)?;

        let transactions = v
            .as_array()
            .ok_or_else(|| serde::de::Error::custom("Expected the JSON to be an array"))?
            .iter()
            .filter_map(|item| item.as_str().map(String::from))
            .collect::<Vec<String>>();

        Ok(TxidsResponse { transactions })
    }
}

/// Zingo-Proxy commitment tree structure replicating functionality in Zebra.
///
/// A wrapper that contains either an Orchard or Sapling note commitment tree.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ProxyCommitments {
    /// Commitment tree state
    pub final_state: String,
}

/// Zingo-Proxy sapling treestate.
///
/// A treestate that is included in the [`z_gettreestate`][1] RPC response.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ProxySaplingTreestate {
    /// Sapling note commitment tree.
    pub commitments: ProxyCommitments,
}

/// Zingo-Proxy orchard treestate.
///
/// A treestate that is included in the [`z_gettreestate`][1] RPC response.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ProxyOrchardTreestate {
    /// Sapling note commitment tree.
    pub commitments: ProxyCommitments,
}

/// Contains the hex-encoded Sapling & Orchard note commitment trees, and their
/// corresponding [`block::Hash`], [`Height`], and block time.
///
/// This is used for the output parameter of [`JsonRpcConnector::get_treestate`].
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct GetTreestateResponse {
    /// The block height corresponding to the treestate, numeric.
    pub height: i32,

    /// The block hash corresponding to the treestate, hex-encoded.
    pub hash: String,

    /// Unix time when the block corresponding to the treestate was mined, numeric.
    ///
    /// UTC seconds since the Unix 1970-01-01 epoch.
    pub time: u32,

    /// A treestate containing a Sapling note commitment tree, hex-encoded.
    pub sapling: ProxySaplingTreestate,

    /// A treestate containing an Orchard note commitment tree, hex-encoded.
    pub orchard: ProxyOrchardTreestate,
}

impl<'de> Deserialize<'de> for GetTreestateResponse {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let v = serde_json::Value::deserialize(deserializer)?;
        let height = v["height"]
            .as_i64()
            .ok_or_else(|| serde::de::Error::missing_field("height"))? as i32;
        let hash = v["hash"]
            .as_str() // This directly accesses the string value
            .ok_or_else(|| serde::de::Error::missing_field("hash"))? // Converts Option to Result
            .to_string();
        let time = v["time"]
            .as_i64()
            .ok_or_else(|| serde::de::Error::missing_field("time"))? as u32;
        let sapling_final_state = v["sapling"]["commitments"]["finalState"]
            .as_str()
            .ok_or_else(|| serde::de::Error::missing_field("sapling final state"))?
            .to_string();
        let orchard_final_state = v["orchard"]["commitments"]["finalState"]
            .as_str()
            .ok_or_else(|| serde::de::Error::missing_field("orchard final state"))?
            .to_string();
        Ok(GetTreestateResponse {
            height,
            hash,
            time,
            sapling: ProxySaplingTreestate {
                commitments: ProxyCommitments {
                    final_state: sapling_final_state,
                },
            },
            orchard: ProxyOrchardTreestate {
                commitments: ProxyCommitments {
                    final_state: orchard_final_state,
                },
            },
        })
    }
}

/// A serialized transaction.
///
/// Stores bytes that are guaranteed to be deserializable into a [`Transaction`].
///
/// Sorts in lexicographic order of the transaction's serialized data.
#[derive(Clone, Eq, PartialEq, serde::Serialize)]
pub struct ProxySerializedTransaction {
    /// Transaction bytes.
    pub bytes: Vec<u8>,
}

impl std::fmt::Display for ProxySerializedTransaction {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(&hex::encode(&self.bytes))
    }
}

impl std::fmt::Debug for ProxySerializedTransaction {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let data_hex = hex::encode(&self.bytes);

        f.debug_tuple("ProxySerializedTransaction")
            .field(&data_hex)
            .finish()
    }
}

impl AsRef<[u8]> for ProxySerializedTransaction {
    fn as_ref(&self) -> &[u8] {
        self.bytes.as_ref()
    }
}

impl From<Vec<u8>> for ProxySerializedTransaction {
    fn from(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }
}

impl FromHex for ProxySerializedTransaction {
    type Error = <Vec<u8> as FromHex>::Error;

    fn from_hex<T: AsRef<[u8]>>(hex: T) -> Result<Self, Self::Error> {
        let bytes = <Vec<u8>>::from_hex(hex)?;

        Ok(bytes.into())
    }
}

impl<'de> Deserialize<'de> for ProxySerializedTransaction {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let v = serde_json::Value::deserialize(deserializer)?;
        if let Some(hex_str) = v.as_str() {
            let bytes = hex::decode(hex_str).map_err(serde::de::Error::custom)?;
            Ok(ProxySerializedTransaction { bytes })
        } else {
            Err(serde::de::Error::custom("expected a hex string"))
        }
    }
}

/// Contains raw transaction, encoded as hex bytes.
///
/// This is used for the output parameter of [`JsonRpcConnector::get_raw_transaction`].
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub enum GetTransactionResponse {
    /// The raw transaction, encoded as hex bytes.
    Raw(#[serde(with = "hex")] ProxySerializedTransaction),
    /// The transaction object.
    Object {
        /// The raw transaction, encoded as hex bytes.
        #[serde(with = "hex")]
        hex: ProxySerializedTransaction,
        /// The height of the block in the best chain that contains the transaction, or -1 if
        /// the transaction is in the mempool.
        height: i32,
        /// The confirmations of the block in the best chain that contains the transaction,
        /// or 0 if the transaction is in the mempool.
        confirmations: u32,
    },
}

impl<'de> Deserialize<'de> for GetTransactionResponse {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let v = serde_json::Value::deserialize(deserializer)?;
        if v.get("height").is_some() && v.get("confirmations").is_some() {
            let hex = serde_json::from_value(v["hex"].clone()).map_err(serde::de::Error::custom)?;
            let height = v["height"]
                .as_i64()
                .ok_or_else(|| serde::de::Error::custom("Missing or invalid height"))?
                as i32;
            let confirmations = v["confirmations"]
                .as_u64()
                .ok_or_else(|| serde::de::Error::custom("Missing or invalid confirmations"))?
                as u32;
            let obj = GetTransactionResponse::Object {
                hex,
                height,
                confirmations,
            };
            Ok(obj)
        } else if v.get("hex").is_some() && v.get("txid").is_some() {
            let hex = serde_json::from_value(v["hex"].clone()).map_err(serde::de::Error::custom)?;
            let obj = GetTransactionResponse::Object {
                hex,
                height: -1,
                confirmations: 0,
            };
            Ok(obj)
        } else {
            let raw = GetTransactionResponse::Raw(
                serde_json::from_value(v.clone()).map_err(serde::de::Error::custom)?,
            );
            Ok(raw)
        }
    }
}

/// *** THE FOLLOWING CODE IS CURRENTLY UNUSED BY ZINGO-PROXY AND UNTESTED! ***

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
