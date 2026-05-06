//! Types associated with the `getchaintips` RPC request.

use std::convert::Infallible;

use serde::{Deserialize, Serialize};

use crate::jsonrpsee::connector::ResponseToError;

/// Response to a `getchaintips` RPC request.
pub type GetChainTipsResponse = Vec<ChainTip>;

/// Information about a known chain tip.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct ChainTip {
    /// Height of the chain tip.
    pub height: u32,
    /// Block hash of the tip, in RPC display order.
    pub hash: String,
    /// Length of the branch connecting the tip to the active chain.
    pub branchlen: u32,
    /// Status of the chain tip.
    pub status: ChainTipStatus,
}

impl ChainTip {
    /// Creates a new chain tip response item.
    pub fn new(height: u32, hash: String, branchlen: u32, status: ChainTipStatus) -> Self {
        Self {
            height,
            hash,
            branchlen,
            status,
        }
    }
}

/// Status values returned by zcashd's `getchaintips`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChainTipStatus {
    /// This branch contains at least one invalid block.
    Invalid,
    /// Not all blocks for this branch are available, but the headers are valid.
    HeadersOnly,
    /// All blocks are available for this branch, but they were never fully validated.
    ValidHeaders,
    /// This branch is not part of the active chain, but is fully validated.
    ValidFork,
    /// This is the tip of the active main chain.
    Active,
    /// The validation state is unknown.
    Unknown,
}

impl ResponseToError for GetChainTipsResponse {
    type RpcError = Infallible;
}
