//! Aggregate chain/node info — the domain shape behind `getblockchaininfo`.

use crate::{BlockId, Height};

/// A domain-typed summary of the chain's position, distinct from the wire
/// `getblockchaininfo` response an adapter renders from it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainInfo {
    /// The current best tip, or `None` before any block is indexed.
    pub tip: Option<BlockId>,
    /// The validator's best-known height, which may lead the indexed `tip`.
    pub estimated_height: Height,
}
