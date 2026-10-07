//! [`ValidatorP2pSource`], and what this crate asks a validator through the balancer
//! (`zaino-traffic`: cadence, retries, blame)
//!
//! - poll (the balancer's, folded here): `getblockchaininfo` (its claim, the estimate and schedule
//!   `GetLightdInfo` serves) + `getrawmempool true` (the listing the diff runs over, fees
//!   included) + `getblockhash` at the final boundary and the best (what it holds: `holders.rs`);
//!   every 60 s, also `getpeerinfo` + `getinfo` + `getdeprecationinfo` (telemetry)
//! - `bytes(..)`: `getrawtransaction <txid> 0` for what the diff added, fetched once per txid
//! - `headers(Pinned)`: `getblockheader <h> false` batches (its headers)
//! - `submit(..)`: `sendrawtransaction`, one submission attempt or the verdict (§6)

use std::net::SocketAddr;

use bytes::Bytes;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use zaino_primitives::types::{Height, TransactionId};
use zaino_source::NonDomainError;

/// Txids one peer announced (`inv`), attributed by the p2p layer
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heard {
    pub peer: SocketAddr,
    pub txids: Vec<TransactionId>,
}

/// Zcash p2p network as the view uses it (§5, §6): sightings + submission entries, never the tip
/// or finality; validators' RPC = [`ChainDataSource`](zaino_source::ChainDataSource)
///
/// - object-safe (`dyn`): the view's type stays the same with or without peers
pub trait ValidatorP2pSource: Send + Sync + 'static {
    /// Fresh subscription per call: every transaction `inv` from now on (lag drops some:
    /// telemetry undercounts)
    fn heard(&self) -> BoxStream<'static, Heard>;
    /// Connected peers whose announcements arrive now (`peers: x/y`'s `y`)
    fn live(&self) -> Vec<SocketAddr>;
    /// Submission entry candidates at `tip`: live, on a protocol version accepted there
    fn entries(&self, tip: Option<Height>) -> Vec<SocketAddr>;
    /// One isolated push (delivery only: a peer answers a push with nothing)
    fn push(&self, entry: SocketAddr, raw: Bytes)
        -> BoxFuture<'static, Result<(), NonDomainError>>;
}
