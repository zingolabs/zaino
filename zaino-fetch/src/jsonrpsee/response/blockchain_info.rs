//! Types associated with the `getinfo` RPC request.

use zebra_chain::block::Height;

use crate::jsonrpsee::{
    connector::{ResponseToError, RpcError},
    response::common::balance::ChainBalance,
};

use std::{convert::Infallible, num::ParseIntError};

fn default_header() -> Height {
    Height(0)
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
    pub(super) chain_supply: ChainBalance,

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

impl TryFrom<GetBlockchainInfoResponse> for zebra_rpc::methods::GetBlockchainInfoResponse {
    fn try_from(response: GetBlockchainInfoResponse) -> Result<Self, ParseIntError> {
        Ok(zebra_rpc::methods::GetBlockchainInfoResponse::new(
            response.chain,
            response.blocks,
            response.best_block_hash,
            response.estimated_height,
            response.chain_supply.into_inner(),
            response.value_pools.map(|pool| pool.into_inner()),
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

/// Error type used for the `chainwork` field of the `getblockchaininfo` RPC request.
#[derive(Debug, thiserror::Error)]
pub enum ChainWorkError {}

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
