//! Query: list the validator's peer connections.

use std::future::Future;

use zaino_primitives::types::rpc::PeerInfo;

use super::QueryError;

/// Domain error for [`GetPeerInfo`].
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetPeerInfoError {
    /// The validator is not ready to report peers (e.g. still starting).
    #[error("validator not ready")]
    NotReady,
}

/// List the peers the validator is connected to.
///
/// These are the *validator's* peers — Zaino is not a p2p node and has none of
/// its own. An empty list is a valid answer from an isolated validator, not an
/// error.
///
/// Richer per-peer data than [`PeerInfo`] carries would be a separate
/// capability trait, so that an adapter unable to supply it simply does not
/// implement that trait rather than returning fields that are always absent.
///
/// Maps to `getpeerinfo` over JSON-RPC.
pub trait GetPeerInfo: Send + Sync {
    /// List peer connections.
    fn get_peer_info(
        &self,
    ) -> impl Future<Output = Result<Vec<PeerInfo>, QueryError<GetPeerInfoError>>> + Send;
}
