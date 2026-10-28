//! [`PeerSource`], [`UnspentOutputs`], and what this crate asks a validator through the
//! balancer (`zaino-traffic`: cadence, retries, blame)
//!
//! - poll (the balancer's, merged here; its calls: `ChainDataSource::get_poll_reading`): its
//!   claim, the estimate and schedule `GetLightdInfo` serves, the listing the diff runs over,
//!   whether it holds the view's best as the poll starts
//! - every 60 s, also its `Telemetry`
//! - `bytes(..)`: `getrawtransaction <txid> 0` for what the diff added, fetched once per txid
//! - `headers(Pinned)`: `getblockheader <h> false` batches (its headers)
//! - `check_admission(..)`: `checkadmission`, a submission's verdict before any push
//! - `submit(..)`: `sendrawtransaction`, one submission attempt or the fail-safe

use std::net::SocketAddr;

use bytes::Bytes;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use zaino_primitives::types::{Announced, PeerTxId};
use zaino_source::NonDomainError;
use zcash_protocol::value::Zatoshis;
use zcash_transparent::bundle::OutPoint;

/// Zcash p2p network as the view uses it: mempool listings, submission relays, which chain each
/// peer carries
///
/// - validators' RPC = [`ChainDataSource`](zaino_source::ChainDataSource)
/// - object-safe (`dyn`): the view's type stays the same with or without peers
pub trait PeerSource: Send + Sync + 'static {
    /// Fresh subscription per call: every block `inv` from now on (lag drops some: peer finality
    /// waits for the next block)
    fn heard(&self) -> BoxStream<'static, Announced>;
    /// Outbound peers live now (each one's mempool polled, `peers: x/y`'s `y`)
    fn live(&self) -> Vec<SocketAddr>;
    /// One relay to ~a third of the live peers, unattributed (delivery only: a peer answers a
    /// push with nothing)
    fn push(&self, raw: Bytes) -> BoxFuture<'static, Result<(), NonDomainError>>;
    /// `peer`'s whole mempool, that peer alone answering (every transaction it admitted)
    fn mempool(
        &self,
        peer: SocketAddr,
    ) -> BoxFuture<'static, Result<Vec<PeerTxId>, NonDomainError>>;
    /// Each id's own bytes, in `ids` order (`None` = no peer supplied it)
    fn transactions(
        &self,
        ids: Vec<PeerTxId>,
    ) -> BoxFuture<'static, Result<Vec<Option<Bytes>>, NonDomainError>>;
}

/// Zaino's transparent UTXO set, as of the served tip (the utxo-set index)
pub trait UnspentOutputs: Send + Sync + 'static {
    /// Each outpoint's value, in order (`None` = never created, or spent)
    ///
    /// - outer `None` = no set yet
    fn values(&self, outpoints: &[OutPoint]) -> Option<Vec<Option<Zatoshis>>>;
}
