//! Query: fetch the validator's peer-to-peer network view.

use std::future::Future;

use zaino_primitives::types::rpc::NetworkInfo;

use super::{QueryError, ValidatorSource};

/// Domain error for [`GetNetworkInfo`].
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetNetworkInfoError {
    /// The validator is not ready to describe its network (e.g. still starting).
    #[error("validator not ready")]
    NotReady,
}

/// Fetch the validator's network state — protocol identity, connection count,
/// per-network reachability and the relay-fee floor.
///
/// Maps to `getnetworkinfo` over JSON-RPC.
#[zaino_source_macros::resilient_port]
pub trait OneShotGetNetworkInfo: ValidatorSource + Send + Sync {
    /// Fetch network information.
    fn get_network_info(
        &self,
    ) -> impl Future<Output = Result<NetworkInfo, QueryError<GetNetworkInfoError, Self::NonDomain>>> + Send;
}
