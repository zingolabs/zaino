//! Query: fetch general information about the validator.

use std::future::Future;

use zaino_primitives::types::rpc::NodeInfo;

use super::QueryError;

/// Domain error for [`GetNodeInfo`].
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetNodeInfoError {
    /// The validator is not ready to describe itself (e.g. still starting).
    #[error("validator not ready")]
    NotReady,
}

/// Fetch version, connection count, fee floors and health of the validator.
///
/// Maps to `getinfo` over JSON-RPC.
pub trait GetNodeInfo: Send + Sync {
    /// Fetch validator information.
    fn get_node_info(
        &self,
    ) -> impl Future<Output = Result<NodeInfo, QueryError<GetNodeInfoError>>> + Send;
}
