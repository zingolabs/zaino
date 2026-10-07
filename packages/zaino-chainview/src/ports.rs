//! [`ValidatorP2pSource`], and how this crate uses a validator's
//! [`ChainDataSource`](zaino_source::ChainDataSource) (two round trips at most per tick)
//!
//! - poll batch: `getblockchaininfo` (its claim, the estimate and schedule `GetLightdInfo`
//!   serves) + `getrawmempool true` (the listing the diff runs over, fees included) +
//!   `getblockhash` at the final boundary and the best (what it holds: `holders.rs`); every
//!   `METADATA_REFRESH`, also `getpeerinfo` + `getinfo` + `getdeprecationinfo` (telemetry)
//! - bytes batch: `getrawtransaction <txid> 0` for what the diff added, fetched once per txid
//! - header sync: `getblockheader <h> false` batches (its headers)
//! - `sendrawtransaction`: one submission attempt or the verdict (§6)
//!
//! - Retry = this crate's own per-endpoint ladder (`config.rs`)

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
