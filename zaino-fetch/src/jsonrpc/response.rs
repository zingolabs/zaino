//! Response types for jsonRPC client.

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

/// Response to a `getblockchaininfo` RPC request.
///
/// This is used for the output parameter of [`JsonRpcConnector::get_blockchain_info`].
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct GetBlockchainInfoResponse {
    /// Current network name as defined in BIP70 (main, test, regtest)
    pub chain: String,

    /// The current number of blocks processed in the server, numeric
    pub blocks: zebra_chain::block::Height,

    /// The hash of the currently best block, in big-endian order, hex-encoded
    #[serde(rename = "bestblockhash", with = "hex")]
    pub best_block_hash: zebra_chain::block::Hash,

    /// If syncing, the estimated height of the chain, else the current best height, numeric.
    ///
    /// In Zebra, this is always the height estimate, so it might be a little inaccurate.
    #[serde(rename = "estimatedheight")]
    pub estimated_height: zebra_chain::block::Height,

    /// Status of network upgrades
    pub upgrades: indexmap::IndexMap<
        zebra_rpc::methods::ConsensusBranchIdHex,
        zebra_rpc::methods::NetworkUpgradeInfo,
    >,

    /// Branch IDs of the current and upcoming consensus rules
    pub consensus: zebra_rpc::methods::TipConsensusBranch,
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
pub struct SendTransactionResponse(#[serde(with = "hex")] pub zebra_chain::transaction::Hash);

/// Response to a `getbestblockhash` and `getblockhash` RPC request.
///
/// Contains the hex-encoded hash of the requested block.
///
/// Also see the notes for the [`Rpc::get_best_block_hash`] and `get_block_hash` methods.
#[derive(Copy, Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(transparent)]
pub struct GetBlockHash(#[serde(with = "hex")] pub zebra_chain::block::Hash);

impl Default for GetBlockHash {
    fn default() -> Self {
        GetBlockHash(zebra_chain::block::Hash([0; 32]))
    }
}

/// A wrapper struct for a zebra serialized block.
///
/// Stores bytes that are guaranteed to be deserializable into a [`Block`].
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SerializedBlock {
    inner: zebra_chain::block::SerializedBlock,
}

impl std::ops::Deref for SerializedBlock {
    type Target = zebra_chain::block::SerializedBlock;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl AsRef<[u8]> for SerializedBlock {
    fn as_ref(&self) -> &[u8] {
        self.inner.as_ref()
    }
}

impl From<Vec<u8>> for SerializedBlock {
    fn from(bytes: Vec<u8>) -> Self {
        Self {
            inner: zebra_chain::block::SerializedBlock::from(bytes),
        }
    }
}

impl From<zebra_chain::block::SerializedBlock> for SerializedBlock {
    fn from(inner: zebra_chain::block::SerializedBlock) -> Self {
        SerializedBlock { inner }
    }
}

impl hex::FromHex for SerializedBlock {
    type Error = hex::FromHexError;

    fn from_hex<T: AsRef<[u8]>>(hex: T) -> Result<Self, Self::Error> {
        hex::decode(hex).map(SerializedBlock::from)
    }
}

impl<'de> serde::Deserialize<'de> for SerializedBlock {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct HexVisitor;

        impl<'de> serde::de::Visitor<'de> for HexVisitor {
            type Value = SerializedBlock;

            fn expecting(&self, formatter: &mut core::fmt::Formatter) -> core::fmt::Result {
                formatter.write_str("a hex-encoded string")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                let bytes = hex::decode(value).map_err(serde::de::Error::custom)?;
                Ok(SerializedBlock::from(bytes))
            }
        }

        deserializer.deserialize_str(HexVisitor)
    }
}

/// Contains the hex-encoded hash of the sent transaction.
///
/// This is used for the output parameter of [`JsonRpcConnector::get_block`].
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(untagged)]
pub enum GetBlockResponse {
    /// The request block, hex-encoded.
    Raw(#[serde(with = "hex")] SerializedBlock),
    /// The block object.
    Object {
        /// The hash of the requested block.
        hash: GetBlockHash,

        /// The number of confirmations of this block in the best chain,
        /// or -1 if it is not in the best chain.
        confirmations: i64,

        /// The height of the requested block.
        #[serde(skip_serializing_if = "Option::is_none")]
        height: Option<zebra_chain::block::Height>,

        /// The height of the requested block.
        #[serde(skip_serializing_if = "Option::is_none")]
        time: Option<i64>,

        /// List of transaction IDs in block order, hex-encoded.
        tx: Vec<String>,

        /// Information about the note commitment trees.
        trees: zebra_rpc::methods::GetBlockTrees,
    },
}

/// Contains the hex-encoded hash of the requested block.
///
/// This is used for the output parameter of [`JsonRpcConnector::get_best_block_hash`].
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(transparent)]
pub struct BestBlockHashResponse(#[serde(with = "hex")] pub zebra_chain::block::Hash);

/// Vec of transaction ids, as a JSON array.
///
/// This is used for the output parameter of [`JsonRpcConnector::get_raw_mempool`] and [`JsonRpcConnector::get_address_txids`].
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct TxidsResponse {
    /// Vec of txids.
    pub transactions: Vec<String>,
}

impl<'de> serde::Deserialize<'de> for TxidsResponse {
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
    pub sapling: zebra_rpc::methods::trees::Treestate<String>,

    /// A treestate containing an Orchard note commitment tree, hex-encoded.
    pub orchard: zebra_rpc::methods::trees::Treestate<String>,
}

impl<'de> serde::Deserialize<'de> for GetTreestateResponse {
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
            sapling: zebra_rpc::methods::trees::Treestate::new(
                zebra_rpc::methods::trees::Commitments::new(sapling_final_state),
            ),
            orchard: zebra_rpc::methods::trees::Treestate::new(
                zebra_rpc::methods::trees::Commitments::new(orchard_final_state),
            ),
        })
    }
}

/// A wrapper struct for a zebra serialized transaction.
///
/// Stores bytes that are guaranteed to be deserializable into a [`Transaction`].
///
/// Sorts in lexicographic order of the transaction's serialized data.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct SerializedTransaction {
    /// Transaction bytes.
    pub inner: zebra_chain::transaction::SerializedTransaction,
}

impl std::ops::Deref for SerializedTransaction {
    type Target = zebra_chain::transaction::SerializedTransaction;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl AsRef<[u8]> for SerializedTransaction {
    fn as_ref(&self) -> &[u8] {
        self.inner.as_ref()
    }
}

impl From<Vec<u8>> for SerializedTransaction {
    fn from(bytes: Vec<u8>) -> Self {
        Self {
            inner: zebra_chain::transaction::SerializedTransaction::from(bytes),
        }
    }
}

impl From<zebra_chain::transaction::SerializedTransaction> for SerializedTransaction {
    fn from(inner: zebra_chain::transaction::SerializedTransaction) -> Self {
        SerializedTransaction { inner }
    }
}

impl hex::FromHex for SerializedTransaction {
    type Error = <Vec<u8> as hex::FromHex>::Error;

    fn from_hex<T: AsRef<[u8]>>(hex: T) -> Result<Self, Self::Error> {
        let bytes = <Vec<u8>>::from_hex(hex)?;

        Ok(bytes.into())
    }
}

impl<'de> serde::Deserialize<'de> for SerializedTransaction {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let v = serde_json::Value::deserialize(deserializer)?;
        if let Some(hex_str) = v.as_str() {
            let bytes = hex::decode(hex_str).map_err(serde::de::Error::custom)?;
            Ok(SerializedTransaction {
                inner: zebra_chain::transaction::SerializedTransaction::from(bytes),
            })
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

impl<'de> serde::Deserialize<'de> for GetTransactionResponse {
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
/// ***                           TEST BEFORE USE                           ***

/// Wrapper struct for a zebra SubtreeRpcData.
///
/// *** UNTESTED - TEST BEFORE USE ***
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SubtreeRpcData {
    /// A subtree data type that can hold Sapling or Orchard subtree roots.
    pub inner: zebra_rpc::methods::trees::SubtreeRpcData,
}

impl std::ops::Deref for SubtreeRpcData {
    type Target = zebra_rpc::methods::trees::SubtreeRpcData;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl From<zebra_rpc::methods::trees::SubtreeRpcData> for SubtreeRpcData {
    fn from(inner: zebra_rpc::methods::trees::SubtreeRpcData) -> Self {
        SubtreeRpcData { inner }
    }
}

impl hex::FromHex for SubtreeRpcData {
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

        Ok(SubtreeRpcData {
            inner: zebra_rpc::methods::trees::SubtreeRpcData {
                root,
                end_height: zebra_chain::block::Height(height),
            },
        })
    }
}

impl<'de> serde::Deserialize<'de> for SubtreeRpcData {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let hex_str = String::deserialize(deserializer)?;

        if hex_str.len() < 8 {
            return Err(serde::de::Error::custom("Hex string is too short"));
        }

        let root_end_index = hex_str.len() - 8;
        let (root_hex, height_hex) = hex_str.split_at(root_end_index);

        let root = root_hex.to_string();
        let height = u32::from_str_radix(height_hex, 16)
            .map_err(|_| serde::de::Error::custom("Failed to parse height"))?;

        Ok(SubtreeRpcData {
            inner: zebra_rpc::methods::trees::SubtreeRpcData {
                root,
                end_height: zebra_chain::block::Height(height),
            },
        })
    }
}

/// Contains the Sapling or Orchard pool label, the index of the first subtree in the list,
/// and a list of subtree roots and end heights.
///
/// This is used for the output parameter of [`JsonRpcConnector::get_subtrees_by_index`].
///
/// *** UNTESTED - TEST BEFORE USE ***
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct GetSubtreesResponse {
    /// The shielded pool to which the subtrees belong.
    pub pool: String,

    /// The index of the first subtree.
    pub start_index: zebra_chain::subtree::NoteCommitmentSubtreeIndex,

    /// A sequential list of complete subtrees, in `index` order.
    ///
    /// The generic subtree root type is a hex-encoded Sapling or Orchard subtree root string.
    // #[serde(skip_serializing_if = "Vec::is_empty")]
    pub subtrees: Vec<SubtreeRpcData>,
}

/// Wrapper struct for a zebra Scrypt.
///
/// *** UNTESTED - TEST BEFORE USE ***
#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize)]
pub struct Script {
    /// # Correctness
    ///
    /// Consensus-critical serialization uses [`ZcashSerialize`].
    /// [`serde`]-based hex serialization must only be used for RPCs and testing.
    pub inner: zebra_chain::transparent::Script,
}

impl std::ops::Deref for Script {
    type Target = zebra_chain::transparent::Script;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl AsRef<[u8]> for Script {
    fn as_ref(&self) -> &[u8] {
        self.inner.as_raw_bytes()
    }
}

impl From<Vec<u8>> for Script {
    fn from(bytes: Vec<u8>) -> Self {
        Self {
            inner: zebra_chain::transparent::Script::new(bytes.as_ref()),
        }
    }
}

impl From<zebra_chain::transparent::Script> for Script {
    fn from(inner: zebra_chain::transparent::Script) -> Self {
        Script { inner }
    }
}

impl hex::FromHex for Script {
    type Error = <Vec<u8> as hex::FromHex>::Error;

    fn from_hex<T: AsRef<[u8]>>(hex: T) -> Result<Self, Self::Error> {
        let bytes = Vec::from_hex(hex)?;
        let inner = zebra_chain::transparent::Script::new(&bytes);
        Ok(Script { inner })
    }
}

impl<'de> serde::Deserialize<'de> for Script {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let v = serde_json::Value::deserialize(deserializer)?;
        if let Some(hex_str) = v.as_str() {
            let bytes = hex::decode(hex_str).map_err(serde::de::Error::custom)?;
            let inner = zebra_chain::transparent::Script::new(&bytes);
            Ok(Script { inner })
        } else {
            Err(serde::de::Error::custom("expected a hex string"))
        }
    }
}

/// This is used for the output parameter of [`JsonRpcConnector::get_address_utxos`].
///
/// *** UNTESTED - TEST BEFORE USE ***
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct GetUtxosResponse {
    /// The transparent address, base58check encoded
    pub address: zebra_chain::transparent::Address,

    /// The output txid, in big-endian order, hex-encoded
    #[serde(with = "hex")]
    pub txid: zebra_chain::transaction::Hash,

    /// The transparent output index, numeric
    #[serde(rename = "outputIndex")]
    pub output_index: u32,

    /// The transparent output script, hex encoded
    #[serde(with = "hex")]
    pub script: Script,

    /// The amount of zatoshis in the transparent output
    pub satoshis: u64,

    /// The block height, numeric.
    pub height: zebra_chain::block::Height,
}
