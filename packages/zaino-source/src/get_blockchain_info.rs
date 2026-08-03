//! Query: fetch the validator's view of the chain.

use std::future::Future;

use zaino_primitives::types::BlockchainInfo;

use super::QueryError;

/// Domain error for [`GetBlockchainInfo`].
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetBlockchainInfoError {
    /// The validator is not ready to describe its chain (e.g. still starting).
    #[error("validator not ready")]
    NotReady,
}

/// Fetch the validator's chain state, including its network upgrade schedule.
///
/// Not purely informational: Zaino adopts the activation heights the validator
/// reports here rather than relying on a compiled-in schedule, so an indexer
/// and its validator cannot disagree about where an upgrade activates. Treat
/// the response as a consensus input.
///
/// Maps to `getblockchaininfo` over JSON-RPC.
pub trait GetBlockchainInfo: Send + Sync {
    /// Fetch chain state and the upgrade schedule.
    fn get_blockchain_info(
        &self,
    ) -> impl Future<Output = Result<BlockchainInfo, QueryError<GetBlockchainInfoError>>> + Send;
}
