//! [`ValidatorP2pSource`], and what this crate asks a validator through the balancer
//! (`zaino-traffic`: cadence, retries, blame)
//!
//! - poll (the balancer's, folded here): `getblockchaininfo` (its claim, the estimate and schedule
//!   `GetLightdInfo` serves) + `getrawmempool true` (the listing the diff runs over, fees
//!   included) + `getblockhash` at the view's best as the poll starts (whether it holds it);
//!   every 60 s, also `getpeerinfo` + `getinfo` + `getdeprecationinfo` (telemetry)
//! - `bytes(..)`: `getrawtransaction <txid> 0` for what the diff added, fetched once per txid
//! - `headers(Pinned)`: `getblockheader <h> false` batches (its headers)
//! - `submit(..)`: `sendrawtransaction`, one submission attempt or the verdict

use std::net::SocketAddr;

use bytes::Bytes;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use zaino_primitives::types::{BlockHash, Height, TransactionId};
use zaino_source::NonDomainError;

/// One peer `inv`, attributed by the p2p layer
///
/// - `Transactions` = mempool sightings; `Block` = a new tip on the peer's chain (peer finality
///   counts the netgroups carrying ours)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Heard {
    Transactions { peer: SocketAddr, txids: Vec<TransactionId> },
    Block { peer: SocketAddr, hash: BlockHash },
}

/// Zcash p2p network as the view uses it: sightings, submission entries, and which chain each
/// peer carries; validators' RPC = [`ChainDataSource`](zaino_source::ChainDataSource)
///
/// - object-safe (`dyn`): the view's type stays the same with or without peers
pub trait ValidatorP2pSource: Send + Sync + 'static {
    /// Fresh subscription per call: every transaction and block `inv` from now on (lag drops
    /// some: telemetry undercounts, peer finality waits for the next block)
    fn heard(&self) -> BoxStream<'static, Heard>;
    /// Connected peers whose announcements arrive now (`peers: x/y`'s `y`)
    fn live(&self) -> Vec<SocketAddr>;
    /// Submission entry candidates at `tip`: live, on a protocol version accepted there
    fn entries(&self, tip: Option<Height>) -> Vec<SocketAddr>;
    /// One isolated push (delivery only: a peer answers a push with nothing)
    fn push(&self, entry: SocketAddr, raw: Bytes)
        -> BoxFuture<'static, Result<(), NonDomainError>>;
}
