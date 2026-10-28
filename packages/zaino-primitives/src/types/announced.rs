//! Block `inv` as the p2p layer hands it on

use std::net::SocketAddr;

use zcash_protocol::BlockHash;

/// One peer's block `inv`, its sender attached by zebra: the peer's new tip (peer finality counts
/// the netgroups carrying ours)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Announced {
    pub peer: SocketAddr,
    pub hash: BlockHash,
}
