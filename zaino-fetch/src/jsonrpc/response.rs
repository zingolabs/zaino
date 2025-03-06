//! Response types for jsonRPC client.

use std::num::ParseIntError;

use hex::FromHex;
use serde::{de::Error, Deserialize, Deserializer, Serialize};

use zebra_chain::{
    amount::{Amount, NonNegative},
    block::Height,
    work::difficulty::CompactDifficulty,
};
use zebra_rpc::methods::types::Balance;

/// A helper module to serialize `Option<T: ToHex>` as a hex string.
///
/// *** NOTE / TODO: This code has been copied from zebra to ensure valid serialisation / deserialisation and extended. ***
/// ***            - This code should be made pulic in zebra to avoid deviations in implementation. ***
mod opthex {
    use hex::{FromHex, ToHex};
    use serde::{de, Deserialize, Deserializer, Serializer};

    pub fn serialize<S, T>(data: &Option<T>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
        T: ToHex,
    {
        match data {
            Some(data) => {
                let s = data.encode_hex::<String>();
                serializer.serialize_str(&s)
            }
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
    where
        D: Deserializer<'de>,
        T: FromHex,
    {
        let opt = Option::<String>::deserialize(deserializer)?;
        match opt {
            Some(s) => T::from_hex(&s)
                .map(Some)
                .map_err(|_e| de::Error::custom("failed to convert hex string")),
            None => Ok(None),
        }
    }
}

/// Response to a `getinfo` RPC request.
///
/// This is used for the output parameter of [`JsonRpcConnector::get_info`].
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct GetInfoResponse {
    /// The node version
    #[serde(default)]
    version: u64,
    /// The node version build number
    pub build: String,
    /// The server sub-version identifier, used as the network protocol user-agent
    pub subversion: String,
    /// The protocol version
    #[serde(default)]
    #[serde(rename = "protocolversion")]
    protocol_version: u32,

    /// The current number of blocks processed in the server
    #[serde(default)]
    blocks: u32,

    /// The total (inbound and outbound) number of connections the node has
    #[serde(default)]
    connections: usize,

    /// The proxy (if any) used by the server. Currently always `None` in Zebra.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    proxy: Option<String>,

    /// The current network difficulty
    #[serde(default)]
    difficulty: f64,

    /// True if the server is running in testnet mode, false otherwise
    #[serde(default)]
    testnet: bool,

    /// The minimum transaction fee in ZEC/kB
    #[serde(default)]
    #[serde(rename = "paytxfee")]
    pay_tx_fee: f64,

    /// The minimum relay fee for non-free transactions in ZEC/kB
    #[serde(default)]
    #[serde(rename = "relayfee")]
    relay_fee: f64,

    /// The last error or warning message, or "no errors" if there are no errors
    #[serde(default)]
    errors: String,

    /// The time of the last error or warning message, or "no errors timestamp" if there are no errors
    #[serde(default)]
    #[serde(rename = "errorstimestamp")]
    errors_timestamp: ErrorsTimestamp,
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(untagged)]
/// A wrapper to allow both types of error timestamp
pub enum ErrorsTimestamp {
    /// Returned from zcashd, the timestamp is an integer unix timstamp
    Num(usize),
    /// Returned from zebrad, the timestamp is a string representing a timestamp
    Str(String),
}

impl std::fmt::Display for ErrorsTimestamp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ErrorsTimestamp::Num(n) => f.write_str(&n.to_string()),
            ErrorsTimestamp::Str(s) => f.write_str(s),
        }
    }
}

impl Default for ErrorsTimestamp {
    fn default() -> Self {
        ErrorsTimestamp::Str("Default".to_string())
    }
}

impl From<GetInfoResponse> for zebra_rpc::methods::GetInfo {
    fn from(response: GetInfoResponse) -> Self {
        zebra_rpc::methods::GetInfo::from_parts(
            response.version,
            response.build,
            response.subversion,
            response.protocol_version,
            response.blocks,
            response.connections,
            response.proxy,
            response.difficulty,
            response.testnet,
            response.pay_tx_fee,
            response.relay_fee,
            response.errors,
            response.errors_timestamp.to_string(),
        )
    }
}

/// Response to a `getblockchaininfo` RPC request.
///
/// This is used for the output parameter of [`JsonRpcConnector::get_blockchain_info`].
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
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

    /// Chain supply balance
    #[serde(default)]
    #[serde(rename = "chainSupply")]
    chain_supply: ChainBalance,

    /// Status of network upgrades
    pub upgrades: indexmap::IndexMap<
        zebra_rpc::methods::ConsensusBranchIdHex,
        zebra_rpc::methods::NetworkUpgradeInfo,
    >,

    /// Value pool balances
    #[serde(rename = "valuePools")]
    value_pools: [ChainBalance; 5],

    /// Branch IDs of the current and upcoming consensus rules
    pub consensus: zebra_rpc::methods::TipConsensusBranch,

    /// The current number of headers we have validated in the best chain, that is,
    /// the height of the best chain.
    #[serde(default = "default_header")]
    headers: Height,

    /// The estimated network solution rate in Sol/s.
    #[serde(default)]
    difficulty: f64,

    /// The verification progress relative to the estimated network chain tip.
    #[serde(default)]
    #[serde(rename = "verificationprogress")]
    verification_progress: f64,

    /// The total amount of work in the best chain, hex-encoded.
    #[serde(default)]
    #[serde(rename = "chainwork")]
    chain_work: ChainWork,

    /// Whether this node is pruned, currently always false in Zebra.
    #[serde(default)]
    pruned: bool,

    /// The estimated size of the block and undo files on disk
    #[serde(default)]
    size_on_disk: u64,

    /// The current number of note commitments in the commitment tree
    #[serde(default)]
    commitments: u64,
}

fn default_header() -> Height {
    Height(0)
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(untagged)]
/// A wrapper type to allow both kinds of ChainWork
pub enum ChainWork {
    /// Returned from zcashd, a chainwork is a String representing a
    /// base-16 integer
    Str(String),
    /// Returned from zebrad, a chainwork is an integer
    Num(u64),
}

impl TryFrom<ChainWork> for u64 {
    type Error = ParseIntError;

    fn try_from(value: ChainWork) -> Result<Self, Self::Error> {
        match value {
            ChainWork::Str(s) => u64::from_str_radix(&s, 16),
            ChainWork::Num(u) => Ok(u),
        }
    }
}

impl Default for ChainWork {
    fn default() -> Self {
        ChainWork::Num(0)
    }
}

/// Wrapper struct for a Zebra [`Balance`], enabling custom deserialisation logic to handle both zebrad and zcashd.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ChainBalance(Balance);

impl<'de> Deserialize<'de> for ChainBalance {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize, Debug)]
        struct TempBalance {
            #[serde(default)]
            id: String,
            #[serde(rename = "chainValue")]
            chain_value: f64,
            #[serde(rename = "chainValueZat")]
            chain_value_zat: u64,
            #[allow(dead_code)]
            #[serde(default)]
            monitored: bool,
        }
        let temp = TempBalance::deserialize(deserializer)?;
        let computed_zat = (temp.chain_value * 100_000_000.0).round() as u64;
        if computed_zat != temp.chain_value_zat {
            return Err(D::Error::custom(format!(
                "chainValue and chainValueZat mismatch: computed {} but got {}",
                computed_zat, temp.chain_value_zat
            )));
        }
        let amount = Amount::<NonNegative>::from_bytes(temp.chain_value_zat.to_le_bytes())
            .map_err(|e| serde::de::Error::custom(e.to_string()))?;
        let balance = Balance::new(temp.id, amount);
        Ok(ChainBalance(balance))
    }
}

impl Default for ChainBalance {
    fn default() -> Self {
        Self(Balance::new("default", Amount::zero()))
    }
}

impl TryFrom<GetBlockchainInfoResponse> for zebra_rpc::methods::GetBlockChainInfo {
    fn try_from(response: GetBlockchainInfoResponse) -> Result<Self, ParseIntError> {
        Ok(zebra_rpc::methods::GetBlockChainInfo::new(
            response.chain,
            response.blocks,
            response.best_block_hash,
            response.estimated_height,
            response.chain_supply.0,
            response.value_pools.map(|pool| pool.0),
            response.upgrades,
            response.consensus,
            response.headers,
            response.difficulty,
            response.verification_progress,
            response.chain_work.try_into()?,
            response.pruned,
            response.size_on_disk,
            response.commitments,
        ))
    }

    type Error = ParseIntError;
}

/// The transparent balance of a set of addresses.
///
/// This is used for the output parameter of [`JsonRpcConnector::get_address_balance`].
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct GetBalanceResponse {
    /// The total transparent balance.
    pub balance: u64,
}

impl From<GetBalanceResponse> for zebra_rpc::methods::AddressBalance {
    fn from(response: GetBalanceResponse) -> Self {
        zebra_rpc::methods::AddressBalance {
            balance: response.balance,
        }
    }
}

/// Contains the hex-encoded hash of the sent transaction.
///
/// This is used for the output parameter of [`JsonRpcConnector::send_raw_transaction`].
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct SendTransactionResponse(#[serde(with = "hex")] pub zebra_chain::transaction::Hash);

impl From<SendTransactionResponse> for zebra_rpc::methods::SentTransactionHash {
    fn from(value: SendTransactionResponse) -> Self {
        zebra_rpc::methods::SentTransactionHash::new(value.0)
    }
}

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

impl From<GetBlockHash> for zebra_rpc::methods::GetBlockHash {
    fn from(value: GetBlockHash) -> Self {
        zebra_rpc::methods::GetBlockHash(value.0)
    }
}

/// A wrapper struct for a zebra serialized block.
///
/// Stores bytes that are guaranteed to be deserializable into a [`Block`].
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SerializedBlock(zebra_chain::block::SerializedBlock);

impl std::ops::Deref for SerializedBlock {
    type Target = zebra_chain::block::SerializedBlock;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl AsRef<[u8]> for SerializedBlock {
    fn as_ref(&self) -> &[u8] {
        self.0.as_ref()
    }
}

impl From<Vec<u8>> for SerializedBlock {
    fn from(bytes: Vec<u8>) -> Self {
        Self(zebra_chain::block::SerializedBlock::from(bytes))
    }
}

impl From<zebra_chain::block::SerializedBlock> for SerializedBlock {
    fn from(inner: zebra_chain::block::SerializedBlock) -> Self {
        SerializedBlock(inner)
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

        impl serde::de::Visitor<'_> for HexVisitor {
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

/// Sapling note commitment tree information.
///
/// Wrapper struct for zebra's SaplingTrees
#[derive(Copy, Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct SaplingTrees {
    size: u64,
}

/// Orchard note commitment tree information.
///
/// Wrapper struct for zebra's OrchardTrees
#[derive(Copy, Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct OrchardTrees {
    size: u64,
}

/// Information about the sapling and orchard note commitment trees if any.
///
/// Wrapper struct for zebra's GetBlockTrees
#[derive(Copy, Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct GetBlockTrees {
    sapling: Option<SaplingTrees>,
    orchard: Option<OrchardTrees>,
}

impl GetBlockTrees {
    /// Returns sapling data held by ['GetBlockTrees'].
    pub fn sapling(&self) -> u64 {
        self.sapling.map_or(0, |s| s.size)
    }

    /// Returns orchard data held by ['GetBlockTrees'].
    pub fn orchard(&self) -> u64 {
        self.orchard.map_or(0, |o| o.size)
    }
}

impl From<GetBlockTrees> for zebra_rpc::methods::GetBlockTrees {
    fn from(val: GetBlockTrees) -> Self {
        zebra_rpc::methods::GetBlockTrees::new(val.sapling(), val.orchard())
    }
}

/// Wrapper struct for a zebra `Solution`.
///
/// *** NOTE / TODO: ToHex should be inmlemented in zebra to avoid the use of a wrapper struct. ***
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Solution(pub zebra_chain::work::equihash::Solution);

impl std::ops::Deref for Solution {
    type Target = zebra_chain::work::equihash::Solution;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl hex::ToHex for Solution {
    fn encode_hex<T: std::iter::FromIterator<char>>(&self) -> T {
        self.0.encode_hex()
    }

    fn encode_hex_upper<T: std::iter::FromIterator<char>>(&self) -> T {
        self.0.encode_hex_upper()
    }
}

impl hex::FromHex for Solution {
    type Error = zebra_chain::serialization::SerializationError;

    fn from_hex<T: AsRef<[u8]>>(hex: T) -> Result<Self, Self::Error> {
        let hex_str = std::str::from_utf8(hex.as_ref()).map_err(|_| {
            zebra_chain::serialization::SerializationError::Parse("invalid UTF-8 in hex input")
        })?;
        let bytes = hex::decode(hex_str).map_err(|_| {
            zebra_chain::serialization::SerializationError::Parse("failed to decode hex string")
        })?;
        zebra_chain::work::equihash::Solution::from_bytes(&bytes).map(Solution)
    }
}

impl From<Solution> for zebra_chain::work::equihash::Solution {
    fn from(value: Solution) -> Self {
        value.0
    }
}

/// Contains the hex-encoded hash of the sent transaction.
///
/// This is used for the output parameter of [`JsonRpcConnector::get_block`].
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(untagged)]
#[allow(clippy::large_enum_variant)]
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

        /// The block size. TODO: fill it
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size: Option<i64>,

        /// The height of the requested block.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        height: Option<zebra_chain::block::Height>,

        /// The version field of the requested block.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        version: Option<u32>,

        /// The merkle root of the requested block.
        #[serde(with = "opthex", rename = "merkleroot")]
        #[serde(default, skip_serializing_if = "Option::is_none")]
        merkle_root: Option<zebra_chain::block::merkle::Root>,

        /// The blockcommitments field of the requested block. Its interpretation changes
        /// depending on the network and height.
        #[serde(with = "opthex", rename = "blockcommitments")]
        #[serde(default, skip_serializing_if = "Option::is_none")]
        block_commitments: Option<[u8; 32]>,

        /// The root of the Sapling commitment tree after applying this block.
        #[serde(with = "opthex", rename = "finalsaplingroot")]
        #[serde(default, skip_serializing_if = "Option::is_none")]
        final_sapling_root: Option<[u8; 32]>,

        /// The root of the Orchard commitment tree after applying this block.
        #[serde(with = "opthex", rename = "finalorchardroot")]
        #[serde(default, skip_serializing_if = "Option::is_none")]
        final_orchard_root: Option<[u8; 32]>,

        /// The height of the requested block.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        time: Option<i64>,

        /// The nonce of the requested block header.
        #[serde(with = "opthex")]
        #[serde(default, skip_serializing_if = "Option::is_none")]
        nonce: Option<[u8; 32]>,

        /// The Equihash solution in the requested block header.
        /// Note: presence of this field in getblock is not documented in zcashd.
        #[serde(with = "opthex")]
        #[serde(default, skip_serializing_if = "Option::is_none")]
        solution: Option<Solution>,

        /// The difficulty threshold of the requested block header displayed in compact form.
        #[serde(with = "opthex")]
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bits: Option<CompactDifficulty>,

        /// Floating point number that represents the difficulty limit for this block as a multiple
        /// of the minimum difficulty for the network.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        difficulty: Option<f64>,

        /// List of transaction IDs in block order, hex-encoded.
        tx: Vec<String>,

        /// Information about the note commitment trees.
        trees: GetBlockTrees,

        /// The previous block hash of the requested block header.
        #[serde(
            rename = "previousblockhash",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        previous_block_hash: Option<GetBlockHash>,

        /// The next block hash after the requested block header.
        #[serde(
            rename = "nextblockhash",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        next_block_hash: Option<GetBlockHash>,
    },
}

impl TryFrom<GetBlockResponse> for zebra_rpc::methods::GetBlock {
    type Error = zebra_chain::serialization::SerializationError;

    fn try_from(response: GetBlockResponse) -> Result<Self, Self::Error> {
        match response {
            GetBlockResponse::Raw(serialized_block) => {
                Ok(zebra_rpc::methods::GetBlock::Raw(serialized_block.0))
            }
            GetBlockResponse::Object {
                hash,
                block_commitments,
                confirmations,
                size,
                height,
                version,
                merkle_root,
                final_sapling_root,
                final_orchard_root,
                tx,
                time,
                nonce,
                solution,
                bits,
                difficulty,
                trees,
                previous_block_hash,
                next_block_hash,
            } => {
                let tx_ids: Result<Vec<_>, _> = tx
                    .into_iter()
                    .map(|txid| {
                        txid.parse::<zebra_chain::transaction::Hash>()
                            .map(zebra_rpc::methods::GetBlockTransaction::Hash)
                    })
                    .collect();

                Ok(zebra_rpc::methods::GetBlock::Object {
                    hash: zebra_rpc::methods::GetBlockHash(hash.0),
                    block_commitments,
                    confirmations,
                    size,
                    height,
                    version,
                    merkle_root,
                    final_sapling_root,
                    final_orchard_root,
                    tx: tx_ids?,
                    time,
                    nonce,
                    solution: solution.map(Into::into),
                    bits,
                    difficulty,
                    trees: trees.into(),
                    previous_block_hash: previous_block_hash.map(Into::into),
                    next_block_hash: next_block_hash.map(Into::into),
                })
            }
        }
    }
}

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

impl TryFrom<GetTreestateResponse> for zebra_rpc::methods::trees::GetTreestate {
    type Error = zebra_chain::serialization::SerializationError;

    fn try_from(value: GetTreestateResponse) -> Result<Self, Self::Error> {
        let parsed_hash = zebra_chain::block::Hash::from_hex(&value.hash)?;
        let sapling_bytes = hex::decode(value.sapling.inner().inner().as_bytes())?;
        let orchard_bytes = hex::decode(value.orchard.inner().inner().as_bytes())?;

        Ok(zebra_rpc::methods::trees::GetTreestate::from_parts(
            parsed_hash,
            zebra_chain::block::Height(value.height as u32),
            value.time,
            Some(sapling_bytes),
            Some(orchard_bytes),
        ))
    }
}

/// A wrapper struct for a zebra serialized transaction.
///
/// Stores bytes that are guaranteed to be deserializable into a [`Transaction`].
///
/// Sorts in lexicographic order of the transaction's serialized data.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct SerializedTransaction(zebra_chain::transaction::SerializedTransaction);

impl std::ops::Deref for SerializedTransaction {
    type Target = zebra_chain::transaction::SerializedTransaction;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl AsRef<[u8]> for SerializedTransaction {
    fn as_ref(&self) -> &[u8] {
        self.0.as_ref()
    }
}

impl From<Vec<u8>> for SerializedTransaction {
    fn from(bytes: Vec<u8>) -> Self {
        Self(zebra_chain::transaction::SerializedTransaction::from(bytes))
    }
}

impl From<zebra_chain::transaction::SerializedTransaction> for SerializedTransaction {
    fn from(inner: zebra_chain::transaction::SerializedTransaction) -> Self {
        SerializedTransaction(inner)
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
            Ok(SerializedTransaction(
                zebra_chain::transaction::SerializedTransaction::from(bytes),
            ))
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
        height: Option<i32>,
        /// The confirmations of the block in the best chain that contains the transaction,
        /// or 0 if the transaction is in the mempool.
        confirmations: Option<u32>,
    },
}

impl<'de> serde::Deserialize<'de> for GetTransactionResponse {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let tx_value = serde_json::Value::deserialize(deserializer)?;

        if let Some(hex_value) = tx_value.get("hex") {
            let hex =
                serde_json::from_value(hex_value.clone()).map_err(serde::de::Error::custom)?;

            // Convert `mempool tx height = -1` (Zcashd) to `None` (Zebrad).
            let height = match tx_value.get("height").and_then(|v| v.as_i64()) {
                Some(-1) | None => None,
                Some(h) if h < -1 => {
                    return Err(serde::de::Error::custom("invalid height returned in block"))
                }
                Some(h) => Some(h as i32),
            };

            let confirmations = tx_value
                .get("confirmations")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);

            Ok(GetTransactionResponse::Object {
                hex,
                height,
                confirmations,
            })
        } else if let Some(hex_str) = tx_value.as_str() {
            let raw = SerializedTransaction::from_hex(hex_str).map_err(serde::de::Error::custom)?;
            Ok(GetTransactionResponse::Raw(raw))
        } else {
            Err(serde::de::Error::custom("Unexpected transaction format"))
        }
    }
}

impl From<GetTransactionResponse> for zebra_rpc::methods::GetRawTransaction {
    fn from(value: GetTransactionResponse) -> Self {
        match value {
            GetTransactionResponse::Raw(serialized_transaction) => {
                zebra_rpc::methods::GetRawTransaction::Raw(serialized_transaction.0)
            }
            GetTransactionResponse::Object {
                hex,
                height,
                confirmations,
            } => zebra_rpc::methods::GetRawTransaction::Object(
                zebra_rpc::methods::TransactionObject {
                    hex: hex.0,
                    // Deserialised height is always positive or None so it is safe to convert here.
                    // (see [`impl<'de> serde::Deserialize<'de> for GetTransactionResponse`]).
                    height: height.map(|h| h as u32),
                    confirmations,
                },
            ),
        }
    }
}

/// Wrapper struct for a zebra SubtreeRpcData.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SubtreeRpcData(zebra_rpc::methods::trees::SubtreeRpcData);

impl std::ops::Deref for SubtreeRpcData {
    type Target = zebra_rpc::methods::trees::SubtreeRpcData;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<zebra_rpc::methods::trees::SubtreeRpcData> for SubtreeRpcData {
    fn from(inner: zebra_rpc::methods::trees::SubtreeRpcData) -> Self {
        SubtreeRpcData(inner)
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

        Ok(SubtreeRpcData(zebra_rpc::methods::trees::SubtreeRpcData {
            root,
            end_height: zebra_chain::block::Height(height),
        }))
    }
}

impl<'de> serde::Deserialize<'de> for SubtreeRpcData {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        struct SubtreeDataHelper {
            root: String,
            end_height: u32,
        }
        let helper = SubtreeDataHelper::deserialize(deserializer)?;
        Ok(SubtreeRpcData(zebra_rpc::methods::trees::SubtreeRpcData {
            root: helper.root,
            end_height: zebra_chain::block::Height(helper.end_height),
        }))
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
    pub start_index: zebra_chain::subtree::NoteCommitmentSubtreeIndex,

    /// A sequential list of complete subtrees, in `index` order.
    ///
    /// The generic subtree root type is a hex-encoded Sapling or Orchard subtree root string.
    // #[serde(skip_serializing_if = "Vec::is_empty")]
    pub subtrees: Vec<SubtreeRpcData>,
}

impl From<GetSubtreesResponse> for zebra_rpc::methods::trees::GetSubtrees {
    fn from(value: GetSubtreesResponse) -> Self {
        zebra_rpc::methods::trees::GetSubtrees {
            pool: value.pool,
            start_index: value.start_index,
            subtrees: value
                .subtrees
                .into_iter()
                .map(|wrapped_subtree| wrapped_subtree.0)
                .collect(),
        }
    }
}

/// Wrapper struct for a zebra Scrypt.
///
/// # Correctness
///
/// Consensus-critical serialization uses [`ZcashSerialize`].
/// [`serde`]-based hex serialization must only be used for RPCs and testing.
#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize)]
pub struct Script(zebra_chain::transparent::Script);

impl std::ops::Deref for Script {
    type Target = zebra_chain::transparent::Script;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl AsRef<[u8]> for Script {
    fn as_ref(&self) -> &[u8] {
        self.0.as_raw_bytes()
    }
}

impl From<Vec<u8>> for Script {
    fn from(bytes: Vec<u8>) -> Self {
        Self(zebra_chain::transparent::Script::new(bytes.as_ref()))
    }
}

impl From<zebra_chain::transparent::Script> for Script {
    fn from(inner: zebra_chain::transparent::Script) -> Self {
        Script(inner)
    }
}

impl hex::FromHex for Script {
    type Error = <Vec<u8> as hex::FromHex>::Error;

    fn from_hex<T: AsRef<[u8]>>(hex: T) -> Result<Self, Self::Error> {
        let bytes = Vec::from_hex(hex)?;
        let inner = zebra_chain::transparent::Script::new(&bytes);
        Ok(Script(inner))
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
            Ok(Script(inner))
        } else {
            Err(serde::de::Error::custom("expected a hex string"))
        }
    }
}

/// This is used for the output parameter of [`JsonRpcConnector::get_address_utxos`].
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

impl From<GetUtxosResponse> for zebra_rpc::methods::GetAddressUtxos {
    fn from(value: GetUtxosResponse) -> Self {
        zebra_rpc::methods::GetAddressUtxos::from_parts(
            value.address,
            value.txid,
            zebra_state::OutputIndex::from_index(value.output_index),
            value.script.0,
            value.satoshis,
            value.height,
        )
    }
}
