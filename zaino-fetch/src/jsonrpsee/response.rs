//! Response types for jsonRPSeeConnector.
//!
//! These types are redefined rather than imported from zebra_rpc
//! to prevent locking consumers into a zebra_rpc version

pub mod address_deltas;
pub mod block_deltas;
pub mod block_header;
pub mod block_subsidy;
pub mod common;
pub mod mining_info;
pub mod peer_info;

use std::{convert::Infallible, num::ParseIntError};

use hex::FromHex;
use serde::{de::Error as DeserError, Deserialize, Deserializer, Serialize};

use zebra_chain::{
    amount::{Amount, NonNegative},
    block::Height,
    value_balance::ValueBalance,
    work::difficulty::CompactDifficulty,
};
use zebra_rpc::{
    client::{GetBlockchainInfoBalance, ValidateAddressResponse},
    methods::opthex,
};

use crate::jsonrpsee::connector::ResponseToError;

use super::connector::RpcError;

impl TryFrom<RpcError> for Infallible {
    type Error = RpcError;

    fn try_from(err: RpcError) -> Result<Self, Self::Error> {
        Err(err)
    }
}

/// Response to a `getinfo` RPC request.
///
/// This is used for the output parameter of [`crate::jsonrpsee::connector::JsonRpSeeConnector::get_info`].
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

impl ResponseToError for GetInfoResponse {
    type RpcError = Infallible;
}

impl ResponseToError for GetDifficultyResponse {
    type RpcError = Infallible;
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

impl ResponseToError for ErrorsTimestamp {
    type RpcError = Infallible;
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
        zebra_rpc::methods::GetInfo::new(
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
/// This is used for the output parameter of [`crate::jsonrpsee::connector::JsonRpSeeConnector::get_blockchain_info`].
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

impl ResponseToError for GetBlockchainInfoResponse {
    type RpcError = Infallible;
}

/// Response to a `getdifficulty` RPC request.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct GetDifficultyResponse(pub f64);

/// Response to a `getnetworksolps` RPC request.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct GetNetworkSolPsResponse(pub u64);

impl ResponseToError for GetNetworkSolPsResponse {
    type RpcError = Infallible;
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

/// Error type used for the `chainwork` field of the `getblockchaininfo` RPC request.
#[derive(Debug, thiserror::Error)]
pub enum ChainWorkError {}

impl ResponseToError for ChainWork {
    type RpcError = ChainWorkError;
}
impl TryFrom<RpcError> for ChainWorkError {
    type Error = RpcError;

    fn try_from(value: RpcError) -> Result<Self, Self::Error> {
        // TODO: attempt to convert RpcError into errors specific to this RPC response
        Err(value)
    }
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

/// Wrapper struct for a Zebra [`GetBlockchainInfoBalance`], enabling custom
/// deserialisation logic to handle both zebrad and zcashd.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ChainBalance(GetBlockchainInfoBalance);

impl ResponseToError for ChainBalance {
    type RpcError = Infallible;
}

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
            .map_err(|e| DeserError::custom(e.to_string()))?;
        match temp.id.as_str() {
            "transparent" => Ok(ChainBalance(GetBlockchainInfoBalance::transparent(
                amount, None, /*TODO: handle optional delta*/
            ))),
            "sprout" => Ok(ChainBalance(GetBlockchainInfoBalance::sprout(
                amount, None, /*TODO: handle optional delta*/
            ))),
            "sapling" => Ok(ChainBalance(GetBlockchainInfoBalance::sapling(
                amount, None, /*TODO: handle optional delta*/
            ))),
            "orchard" => Ok(ChainBalance(GetBlockchainInfoBalance::orchard(
                amount, None, /*TODO: handle optional delta*/
            ))),
            // TODO: Investigate source of undocument 'lockbox' value
            // that likely is intended to be 'deferred'
            "lockbox" | "deferred" => Ok(ChainBalance(GetBlockchainInfoBalance::deferred(
                amount, None,
            ))),
            "" => Ok(ChainBalance(GetBlockchainInfoBalance::chain_supply(
                // The pools are immediately summed internally, which pool we pick doesn't matter here
                ValueBalance::from_transparent_amount(amount),
            ))),
            otherwise => todo!("error: invalid chain id deser {otherwise}"),
        }
    }
}

impl Default for ChainBalance {
    fn default() -> Self {
        Self(GetBlockchainInfoBalance::chain_supply(ValueBalance::zero()))
    }
}

impl TryFrom<GetBlockchainInfoResponse> for zebra_rpc::methods::GetBlockchainInfoResponse {
    fn try_from(response: GetBlockchainInfoResponse) -> Result<Self, ParseIntError> {
        Ok(zebra_rpc::methods::GetBlockchainInfoResponse::new(
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
/// This is used for the output parameter of [`crate::jsonrpsee::connector::JsonRpSeeConnector::get_address_balance`].
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct GetBalanceResponse {
    /// The total transparent balance.
    pub balance: u64,
    #[serde(default)]
    /// The total balance received, including change
    pub received: u64,
}

/// Error type for the `get_address_balance` RPC request.
#[derive(Debug, thiserror::Error)]
pub enum GetBalanceError {
    /// Invalid number of provided addresses.
    #[error("Invalid number of addresses: {0}")]
    InvalidAddressesAmount(i16),

    /// Invalid encoding.
    #[error("Invalid encoding: {0}")]
    InvalidEncoding(String),
}

impl ResponseToError for GetBalanceResponse {
    type RpcError = GetBalanceError;
}
impl TryFrom<RpcError> for GetBalanceError {
    type Error = RpcError;

    fn try_from(value: RpcError) -> Result<Self, Self::Error> {
        // TODO: attempt to convert RpcError into errors specific to this RPC response
        Err(value)
    }
}

impl From<GetBalanceResponse> for zebra_rpc::methods::AddressBalance {
    fn from(response: GetBalanceResponse) -> Self {
        zebra_rpc::methods::GetAddressBalanceResponse::new(response.balance, response.received)
    }
}

/// Contains the hex-encoded hash of the sent transaction.
///
/// This is used for the output parameter of [`crate::jsonrpsee::connector::JsonRpSeeConnector::send_raw_transaction`].
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct SendTransactionResponse(#[serde(with = "hex")] pub zebra_chain::transaction::Hash);

/// Error type for the `sendrawtransaction` RPC request.
/// TODO: should we track state here? (`Rejected`, `MissingInputs`)
#[derive(Debug, thiserror::Error)]
pub enum SendTransactionError {
    /// Decoding failed.
    #[error("Decoding failed")]
    DeserializationError,

    /// Transaction rejected due to `expiryheight` being under `TX_EXPIRING_SOON_THRESHOLD`.
    /// This is used for DoS mitigation.
    #[error("Transaction expiring soon: {0}")]
    ExpiringSoon(u64),

    /// Transaction has no inputs.
    #[error("Missing inputs")]
    MissingInputs,

    /// Transaction already in the blockchain.
    #[error("Already in chain")]
    AlreadyInChain,

    /// Transaction rejected.
    #[error("Transaction rejected")]
    Rejected(String),
}

impl ResponseToError for SendTransactionResponse {
    type RpcError = SendTransactionError;
}
impl TryFrom<RpcError> for SendTransactionError {
    type Error = RpcError;

    fn try_from(value: RpcError) -> Result<Self, Self::Error> {
        // TODO: attempt to convert RpcError into errors specific to this RPC response
        Err(value)
    }
}

impl From<SendTransactionResponse> for zebra_rpc::methods::SentTransactionHash {
    fn from(value: SendTransactionResponse) -> Self {
        zebra_rpc::methods::SentTransactionHash::new(value.0)
    }
}

/// Response to a `getbestblockhash` and `getblockhash` RPC request.
///
/// Contains the hex-encoded hash of the requested block.
#[derive(
    Copy, Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize, derive_more::From,
)]
#[serde(transparent)]
pub struct GetBlockHash(#[serde(with = "hex")] pub zebra_chain::block::Hash);

impl ResponseToError for GetBlockHash {
    type RpcError = Infallible;
}

impl Default for GetBlockHash {
    fn default() -> Self {
        GetBlockHash(zebra_chain::block::Hash([0; 32]))
    }
}

impl From<GetBlockHash> for zebra_rpc::methods::GetBlockHash {
    fn from(value: GetBlockHash) -> Self {
        zebra_rpc::methods::GetBlockHashResponse::new(value.0)
    }
}

/// A wrapper struct for a zebra serialized block.
///
/// Stores bytes that are guaranteed to be deserializable into a [`zebra_chain::block::Block`].
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
                E: DeserError,
            {
                let bytes = hex::decode(value).map_err(DeserError::custom)?;
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
    #[serde(default)]
    sapling: Option<SaplingTrees>,
    #[serde(default)]
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
/// This is used for the output parameter of [`crate::jsonrpsee::connector::JsonRpSeeConnector::get_block`].
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(untagged)]
pub enum GetBlockResponse {
    /// The request block, hex-encoded.
    Raw(#[serde(with = "hex")] SerializedBlock),
    /// The block object.
    Object(Box<BlockObject>),
}

impl ResponseToError for SerializedBlock {
    type RpcError = GetBlockError;
}
impl TryFrom<RpcError> for GetBlockError {
    type Error = RpcError;

    fn try_from(value: RpcError) -> Result<Self, Self::Error> {
        // If the block is not in Zebra's state, returns
        // [error code `-8`.](https://github.com/zcash/zcash/issues/5758)
        if value.code == -8 {
            Ok(Self::MissingBlock(value.message))
        } else {
            Err(value)
        }
    }
}

impl ResponseToError for BlockObject {
    type RpcError = GetBlockError;
}

/// Error type for the `getblock` RPC request.
#[derive(Debug, thiserror::Error)]
pub enum GetBlockError {
    /// Verbosity not in range from 0 to 2.
    #[error("Invalid verbosity: {0}")]
    InvalidVerbosity(i8),

    /// Not found.
    #[error("Block not found")]
    BlockNotFound,

    /// Block was pruned.
    #[error("Block not available, pruned data: {0}")]
    BlockNotAvailable(String),

    /// TODO: Cannot read block from disk.
    #[error("Cannot read block")]
    CannotReadBlock,
    /// TODO: temporary variant
    #[error("Custom error: {0}")]
    Custom(String),
    /// The requested block hash or height could not be found
    #[error("Block not found: {0}")]
    MissingBlock(String),
}

// impl std::fmt::Display for GetBlockError {
//     fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
//         f.write_str("block not found")
//     }
// }

impl ResponseToError for GetBlockResponse {
    type RpcError = GetBlockError;
}

/// Contains the height of the most recent block in the best valid block chain
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct GetBlockCountResponse(Height);

impl ResponseToError for GetBlockCountResponse {
    type RpcError = Infallible;
}

impl From<GetBlockCountResponse> for Height {
    fn from(value: GetBlockCountResponse) -> Self {
        value.0
    }
}

impl ResponseToError for ValidateAddressResponse {
    type RpcError = Infallible;
}

/// A block object containing data and metadata about a block.
///
/// This is used for the output parameter of [`crate::jsonrpsee::connector::JsonRpSeeConnector::get_block`].
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct BlockObject {
    /// The hash of the requested block.
    pub hash: GetBlockHash,

    /// The number of confirmations of this block in the best chain,
    /// or -1 if it is not in the best chain.
    pub confirmations: i64,

    /// The block size. TODO: fill it
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<i64>,

    /// The height of the requested block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<zebra_chain::block::Height>,

    /// The version field of the requested block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,

    /// The merkle root of the requested block.
    #[serde(with = "opthex", rename = "merkleroot")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merkle_root: Option<zebra_chain::block::merkle::Root>,

    /// The blockcommitments field of the requested block. Its interpretation changes
    /// depending on the network and height.
    #[serde(with = "opthex", rename = "blockcommitments")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_commitments: Option<[u8; 32]>,

    /// The root of the Sapling commitment tree after applying this block.
    #[serde(with = "opthex", rename = "finalsaplingroot")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_sapling_root: Option<[u8; 32]>,

    /// The root of the Orchard commitment tree after applying this block.
    #[serde(with = "opthex", rename = "finalorchardroot")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_orchard_root: Option<[u8; 32]>,

    /// The height of the requested block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<i64>,

    /// The nonce of the requested block header.
    #[serde(with = "opthex")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nonce: Option<[u8; 32]>,

    /// The Equihash solution in the requested block header.
    /// Note: presence of this field in getblock is not documented in zcashd.
    #[serde(with = "opthex")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub solution: Option<Solution>,

    /// The difficulty threshold of the requested block header displayed in compact form.
    #[serde(with = "opthex")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bits: Option<CompactDifficulty>,

    /// Floating point number that represents the difficulty limit for this block as a multiple
    /// of the minimum difficulty for the network.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub difficulty: Option<f64>,

    /// List of transaction IDs in block order, hex-encoded.
    pub tx: Vec<String>,

    /// Chain supply balance
    #[serde(default)]
    #[serde(rename = "chainSupply")]
    chain_supply: Option<ChainBalance>,
    /// Value pool balances
    ///
    #[serde(rename = "valuePools")]
    value_pools: Option<[ChainBalance; 5]>,

    /// Information about the note commitment trees.
    pub trees: GetBlockTrees,

    /// The previous block hash of the requested block header.
    #[serde(
        rename = "previousblockhash",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub previous_block_hash: Option<GetBlockHash>,

    /// The next block hash after the requested block header.
    #[serde(
        rename = "nextblockhash",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub next_block_hash: Option<GetBlockHash>,
}

impl TryFrom<GetBlockResponse> for zebra_rpc::methods::GetBlock {
    type Error = zebra_chain::serialization::SerializationError;

    fn try_from(response: GetBlockResponse) -> Result<Self, Self::Error> {
        match response {
            GetBlockResponse::Raw(serialized_block) => {
                Ok(zebra_rpc::methods::GetBlock::Raw(serialized_block.0))
            }
            GetBlockResponse::Object(block) => {
                let tx_ids: Result<Vec<_>, _> = block
                    .tx
                    .into_iter()
                    .map(|txid| {
                        txid.parse::<zebra_chain::transaction::Hash>()
                            .map(zebra_rpc::methods::GetBlockTransaction::Hash)
                    })
                    .collect();

                Ok(zebra_rpc::methods::GetBlock::Object(Box::new(
                    zebra_rpc::client::BlockObject::new(
                        block.hash.0,
                        block.confirmations,
                        block.size,
                        block.height,
                        block.version,
                        block.merkle_root,
                        block.block_commitments,
                        block.final_sapling_root,
                        block.final_orchard_root,
                        tx_ids?,
                        block.time,
                        block.nonce,
                        block.solution.map(Into::into),
                        block.bits,
                        block.difficulty,
                        block.chain_supply.map(|supply| supply.0),
                        block.value_pools.map(
                            |[transparent, sprout, sapling, orchard, deferred]| {
                                [transparent.0, sprout.0, sapling.0, orchard.0, deferred.0]
                            },
                        ),
                        block.trees.into(),
                        block.previous_block_hash.map(|hash| hash.0),
                        block.next_block_hash.map(|hash| hash.0),
                    ),
                )))
            }
        }
    }
}

/// Vec of transaction ids, as a JSON array.
///
/// This is used for the output parameter of [`crate::jsonrpsee::connector::JsonRpSeeConnector::get_raw_mempool`] and [`crate::jsonrpsee::connector::JsonRpSeeConnector::get_address_txids`].
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct TxidsResponse {
    /// Vec of txids.
    pub transactions: Vec<String>,
}

/// Error type for the `get_address_txids` RPC method.
#[derive(Debug, thiserror::Error)]
pub enum TxidsError {
    /// TODO: double check.
    ///
    /// If start is greater than the latest block height,
    /// it's interpreted as that height.
    #[error("invalid start block height: {0}")]
    InvalidStartBlockHeight(i64),

    /// TODO: check which cases this can happen.
    #[error("invalid end block height: {0}")]
    InvalidEndBlockHeight(i64),

    /// Invalid address encoding.
    #[error("Invalid encoding: {0}")]
    InvalidEncoding(String),
}

impl ResponseToError for TxidsResponse {
    type RpcError = TxidsError;
}
impl TryFrom<RpcError> for TxidsError {
    type Error = RpcError;

    fn try_from(value: RpcError) -> Result<Self, Self::Error> {
        // TODO: attempt to convert RpcError into errors specific to this RPC response
        Err(value)
    }
}

/// Separate response for the `get_raw_mempool` RPC method.
///
/// Even though the output type is the same as [`TxidsResponse`],
/// errors are different.
pub struct RawMempoolResponse {
    /// Vec of txids.
    pub transactions: Vec<String>,
}

impl ResponseToError for RawMempoolResponse {
    type RpcError = Infallible;

    fn to_error(self) -> Result<Self, Self::RpcError> {
        Ok(self)
    }
}

impl<'de> serde::Deserialize<'de> for TxidsResponse {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let v = serde_json::Value::deserialize(deserializer)?;

        let transactions = v
            .as_array()
            .ok_or_else(|| DeserError::custom("Expected the JSON to be an array"))?
            .iter()
            .filter_map(|item| item.as_str().map(String::from))
            .collect::<Vec<String>>();

        Ok(TxidsResponse { transactions })
    }
}

/// Contains the hex-encoded Sapling & Orchard note commitment trees, and their
/// corresponding `block::Hash`, `Height`, and block time.
///
/// Encoded using v0 frontier encoding.
///
/// This is used for the output parameter of [`crate::jsonrpsee::connector::JsonRpSeeConnector::get_treestate`].
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
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
    pub sapling: zebra_rpc::client::Treestate,

    /// A treestate containing an Orchard note commitment tree, hex-encoded.
    pub orchard: zebra_rpc::client::Treestate,
}

/// Error type for the `get_treestate` RPC request.
#[derive(Debug, thiserror::Error)]
pub enum GetTreestateError {
    /// Invalid hash or height.
    #[error("invalid hash or height: {0}")]
    InvalidHashOrHeight(String),
}

impl ResponseToError for GetTreestateResponse {
    type RpcError = GetTreestateError;
}
impl TryFrom<RpcError> for GetTreestateError {
    type Error = RpcError;

    fn try_from(value: RpcError) -> Result<Self, Self::Error> {
        // TODO: attempt to convert RpcError into errors specific to this RPC response
        Err(value)
    }
}

impl TryFrom<GetTreestateResponse> for zebra_rpc::client::GetTreestateResponse {
    type Error = zebra_chain::serialization::SerializationError;

    fn try_from(value: GetTreestateResponse) -> Result<Self, Self::Error> {
        let parsed_hash = zebra_chain::block::Hash::from_hex(&value.hash)?;
        let height_u32 = u32::try_from(value.height).map_err(|_| {
            zebra_chain::serialization::SerializationError::Parse("negative block height")
        })?;

        let sapling_bytes = value.sapling.commitments().final_state();

        let orchard_bytes = value.orchard.commitments().final_state();

        Ok(zebra_rpc::client::GetTreestateResponse::from_parts(
            parsed_hash,
            zebra_chain::block::Height(height_u32),
            value.time,
            sapling_bytes.clone(),
            orchard_bytes.clone(),
        ))
    }
}

/// Contains raw transaction, encoded as hex bytes.
///
/// This is used for the output parameter of [`crate::jsonrpsee::connector::JsonRpSeeConnector::get_raw_transaction`].
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub enum GetTransactionResponse {
    /// The raw transaction, encoded as hex bytes.
    Raw(#[serde(with = "hex")] zebra_chain::transaction::SerializedTransaction),
    /// The transaction object.
    Object(Box<zebra_rpc::client::TransactionObject>),
}

impl ResponseToError for GetTransactionResponse {
    type RpcError = Infallible;
}

impl<'de> serde::Deserialize<'de> for GetTransactionResponse {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use zebra_rpc::client::{
            Input, Orchard, Output, ShieldedOutput, ShieldedSpend, TransactionObject,
        };

        let tx_value = serde_json::Value::deserialize(deserializer)?;

        if let Some(hex_value) = tx_value.get("hex") {
            let hex_str = hex_value
                .as_str()
                .ok_or_else(|| DeserError::custom("expected hex to be a string"))?;

            let hex = zebra_chain::transaction::SerializedTransaction::from_hex(hex_str)
                .map_err(DeserError::custom)?;

            // Convert `mempool tx height = -1` (Zcashd) to `None` (Zebrad).
            let height = match tx_value.get("height").and_then(|v| v.as_i64()) {
                Some(-1) | None => None,
                Some(h) if h < -1 => {
                    return Err(DeserError::custom("invalid height returned in block"))
                }
                Some(h) => Some(h as u32),
            };

            macro_rules! get_tx_value_fields{
                ($(let $field:ident: $kind:ty = $transaction_json:ident[$field_name:literal]; )+) => {
                    $(let $field = $transaction_json
                        .get($field_name)
                        .map(|v| ::serde_json::from_value::<$kind>(v.clone()))
                        .transpose()
                        .map_err(::serde::de::Error::custom)?;
                    )+
                }
            }

            let confirmations = tx_value
                .get("confirmations")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);

            // if let Some(vin_value) = tx_value.get("vin") {
            //     match serde_json::from_value::<Vec<Input>>(vin_value.clone()) {
            //         Ok(_inputs) => { /* continue */ }
            //         Err(err) => {
            //             eprintln!("Failed to parse vin: {err}");
            //             eprintln!(
            //                 "Offending JSON:\n{}",
            //                 serde_json::to_string_pretty(vin_value).unwrap()
            //             );
            //             return Err(serde::de::Error::custom("Failed to deserialize vin"));
            //         }
            //     }
            // }
            get_tx_value_fields! {
                // We don't need this, as it should always be true if and only if height is Some
                // There's no reason to rely on this field being present when we can determine
                // it correctly in all cases
                let _in_active_chain: bool = tx_value["in_active_chain"];
                let inputs: Vec<Input> = tx_value["vin"];
                let outputs: Vec<Output> = tx_value["vout"];
                let shielded_spends: Vec<ShieldedSpend> = tx_value["vShieldedSpend"];
                let shielded_outputs: Vec<ShieldedOutput> = tx_value["vShieldedOutput"];
                let orchard: Orchard = tx_value["orchard"];
                let value_balance: f64 = tx_value["valueBalance"];
                let value_balance_zat: i64 = tx_value["valueBalanceZat"];
                let size: i64 = tx_value["size"];
                let time: i64 = tx_value["time"];
                let txid: String = tx_value["txid"];
                let auth_digest: String = tx_value["authdigest"];
                let overwintered: bool = tx_value["overwintered"];
                let version: u32 = tx_value["version"];
                let version_group_id: String = tx_value["versiongroupid"];
                let lock_time: u32 = tx_value["locktime"];
                let expiry_height: Height = tx_value["expiryheight"];
                let block_hash: String = tx_value["blockhash"];
                let block_time: i64 = tx_value["blocktime"];
            }

            let txid = txid.ok_or(DeserError::missing_field("txid"))?;

            let txid = zebra_chain::transaction::Hash::from_hex(txid)
                .map_err(|e| DeserError::custom(format!("txid was not valid hash: {e}")))?;
            let block_hash = block_hash
                .map(|bh| {
                    zebra_chain::block::Hash::from_hex(bh).map_err(|e| {
                        DeserError::custom(format!("blockhash was not valid hash: {e}"))
                    })
                })
                .transpose()?;
            let auth_digest = auth_digest
                .map(|ad| {
                    zebra_chain::transaction::AuthDigest::from_hex(ad).map_err(|e| {
                        DeserError::custom(format!("authdigest was not valid hash: {e}"))
                    })
                })
                .transpose()?;
            let version_group_id = version_group_id
                .map(hex::decode)
                .transpose()
                .map_err(|e| DeserError::custom(format!("txid was not valid hash: {e}")))?;

            Ok(GetTransactionResponse::Object(Box::new(
                TransactionObject::new(
                    // optional, but we can infer from height
                    Some(height.is_some()),
                    hex,
                    // optional
                    height,
                    // optional
                    confirmations,
                    inputs.unwrap_or_default(),
                    outputs.unwrap_or_default(),
                    shielded_spends.unwrap_or_default(),
                    shielded_outputs.unwrap_or_default(),
                    // TODO: sprout joinsplits
                    None,
                    None,
                    None,
                    // optional
                    orchard,
                    // optional
                    value_balance,
                    // optional
                    value_balance_zat,
                    // optional
                    size,
                    // optional
                    time,
                    txid,
                    // optional
                    auth_digest,
                    overwintered.unwrap_or(false),
                    version.ok_or(DeserError::missing_field("version"))?,
                    // optional
                    version_group_id,
                    lock_time.ok_or(DeserError::missing_field("locktime"))?,
                    // optional
                    expiry_height,
                    // optional
                    block_hash,
                    // optional
                    block_time,
                ),
            )))
        } else if let Some(hex_str) = tx_value.as_str() {
            let raw = zebra_chain::transaction::SerializedTransaction::from_hex(hex_str)
                .map_err(DeserError::custom)?;
            Ok(GetTransactionResponse::Raw(raw))
        } else {
            Err(DeserError::custom("Unexpected transaction format"))
        }
    }
}

impl From<GetTransactionResponse> for zebra_rpc::methods::GetRawTransaction {
    fn from(value: GetTransactionResponse) -> Self {
        match value {
            GetTransactionResponse::Raw(serialized_transaction) => {
                zebra_rpc::methods::GetRawTransaction::Raw(serialized_transaction)
            }

            GetTransactionResponse::Object(obj) => zebra_rpc::methods::GetRawTransaction::Object(
                Box::new(zebra_rpc::client::TransactionObject::new(
                    obj.in_active_chain(),
                    obj.hex().clone(),
                    obj.height(),
                    obj.confirmations(),
                    obj.inputs().clone(),
                    obj.outputs().clone(),
                    obj.shielded_spends().clone(),
                    obj.shielded_outputs().clone(),
                    //TODO: sprout joinspits
                    None,
                    None,
                    None,
                    obj.orchard().clone(),
                    obj.value_balance(),
                    obj.value_balance_zat(),
                    obj.size(),
                    obj.time(),
                    obj.txid(),
                    obj.auth_digest(),
                    obj.overwintered(),
                    obj.version(),
                    obj.version_group_id().clone(),
                    obj.lock_time(),
                    obj.expiry_height(),
                    obj.block_hash(),
                    obj.block_time(),
                )),
            ),
        }
    }
}

/// Wrapper struct for a zebra SubtreeRpcData.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SubtreeRpcData(zebra_rpc::client::SubtreeRpcData);

impl std::ops::Deref for SubtreeRpcData {
    type Target = zebra_rpc::client::SubtreeRpcData;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<zebra_rpc::client::SubtreeRpcData> for SubtreeRpcData {
    fn from(inner: zebra_rpc::client::SubtreeRpcData) -> Self {
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

        Ok(SubtreeRpcData(zebra_rpc::client::SubtreeRpcData {
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
        Ok(SubtreeRpcData(zebra_rpc::client::SubtreeRpcData {
            root: helper.root,
            end_height: zebra_chain::block::Height(helper.end_height),
        }))
    }
}

/// Contains the Sapling or Orchard pool label, the index of the first subtree in the list,
/// and a list of subtree roots and end heights.
///
/// This is used for the output parameter of [`crate::jsonrpsee::connector::JsonRpSeeConnector::get_subtrees_by_index`].
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

/// Error type for the `z_getsubtreesbyindex` RPC request.
#[derive(Debug, thiserror::Error)]
pub enum GetSubtreesError {
    /// Invalid pool
    #[error("Invalid pool: {0}")]
    InvalidPool(String),

    /// Invalid start index
    #[error("Invalid start index")]
    InvalidStartIndex,

    /// Invalid limit
    #[error("Invalid limit")]
    InvalidLimit,
}

impl ResponseToError for GetSubtreesResponse {
    type RpcError = GetSubtreesError;
}
impl TryFrom<RpcError> for GetSubtreesError {
    type Error = RpcError;

    fn try_from(value: RpcError) -> Result<Self, Self::Error> {
        // TODO: attempt to convert RpcError into errors specific to this RPC response
        Err(value)
    }
}

impl From<GetSubtreesResponse> for zebra_rpc::client::GetSubtreesByIndexResponse {
    fn from(value: GetSubtreesResponse) -> Self {
        zebra_rpc::client::GetSubtreesByIndexResponse::new(
            value.pool,
            value.start_index,
            value
                .subtrees
                .into_iter()
                .map(|wrapped_subtree| wrapped_subtree.0)
                .collect(),
        )
    }
}

/// Wrapper struct for a zebra Scrypt.
///
/// # Correctness
///
/// Consensus-critical serialization uses `ZcashSerialize`.
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
            let bytes = hex::decode(hex_str).map_err(DeserError::custom)?;
            let inner = zebra_chain::transparent::Script::new(&bytes);
            Ok(Script(inner))
        } else {
            Err(DeserError::custom("expected a hex string"))
        }
    }
}

/// This is used for the output parameter of [`crate::jsonrpsee::connector::JsonRpSeeConnector::get_address_utxos`].
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

/// Error type for the `getaddressutxos` RPC request.
#[derive(Debug, thiserror::Error)]
pub enum GetUtxosError {
    /// Invalid encoding
    #[error("Invalid encoding: {0}")]
    InvalidEncoding(String),
}

impl ResponseToError for GetUtxosResponse {
    type RpcError = GetUtxosError;
}
impl TryFrom<RpcError> for GetUtxosError {
    type Error = RpcError;

    fn try_from(value: RpcError) -> Result<Self, Self::Error> {
        // TODO: attempt to convert RpcError into errors specific to this RPC response
        Err(value)
    }
}

impl ResponseToError for Vec<GetUtxosResponse> {
    type RpcError = GetUtxosError;
}

impl From<GetUtxosResponse> for zebra_rpc::methods::GetAddressUtxos {
    fn from(value: GetUtxosResponse) -> Self {
        zebra_rpc::methods::GetAddressUtxos::new(
            value.address,
            value.txid,
            zebra_chain::transparent::OutputIndex::from_index(value.output_index),
            value.script.0,
            value.satoshis,
            value.height,
        )
    }
}

impl<T: ResponseToError> ResponseToError for Box<T>
where
    T::RpcError: Send + Sync + 'static,
{
    type RpcError = T::RpcError;
}

/// Response type for the `getmempoolinfo` RPC request
/// Details on the state of the TX memory pool.
/// In Zaino, this RPC call information is gathered from the local Zaino state instead of directly reflecting the full node's mempool. This state is populated from a gRPC stream, sourced from the full node.
/// The Zcash source code is considered canonical:
/// [from the rpc definition](<https://github.com/zcash/zcash/blob/654a8be2274aa98144c80c1ac459400eaf0eacbe/src/rpc/blockchain.cpp#L1555>), [this function is called to produce the return value](<https://github.com/zcash/zcash/blob/654a8be2274aa98144c80c1ac459400eaf0eacbe/src/rpc/blockchain.cpp#L1541>>).
/// the `size` field is called by [this line of code](<https://github.com/zcash/zcash/blob/654a8be2274aa98144c80c1ac459400eaf0eacbe/src/rpc/blockchain.cpp#L1544>), and returns an int64.
/// `size` represents the number of transactions currently in the mempool.
/// the `bytes` field is called by [this line of code](<https://github.com/zcash/zcash/blob/654a8be2274aa98144c80c1ac459400eaf0eacbe/src/rpc/blockchain.cpp#L1545>), and returns an int64 from [this variable](<https://github.com/zcash/zcash/blob/654a8be2274aa98144c80c1ac459400eaf0eacbe/src/txmempool.h#L349>).
/// `bytes` is the sum memory size in bytes of all transactions in the mempool: the sum of all transaction byte sizes.
/// the `usage` field is called by [this line of code](<https://github.com/zcash/zcash/blob/654a8be2274aa98144c80c1ac459400eaf0eacbe/src/rpc/blockchain.cpp#L1546>), and returns an int64 derived from the return of this function(<https://github.com/zcash/zcash/blob/654a8be2274aa98144c80c1ac459400eaf0eacbe/src/txmempool.h#L1199>), which includes a number of elements.
/// `usage` is the total memory usage for the mempool, in bytes.
/// the [optional `fullyNotified` field](<https://github.com/zcash/zcash/blob/654a8be2274aa98144c80c1ac459400eaf0eacbe/src/rpc/blockchain.cpp#L1549>), is only utilized for zcashd regtests, is deprecated, and is not included.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct GetMempoolInfoResponse {
    /// Current tx count
    pub size: u64,
    /// Sum of all tx sizes
    pub bytes: u64,
    /// Total memory usage for the mempool
    pub usage: u64,
}

impl ResponseToError for GetMempoolInfoResponse {
    type RpcError = Infallible;
}
