//! Query: fetch network mining statistics.

use std::future::Future;

use zaino_primitives::types::rpc::MiningInfo;

use super::QueryError;

/// Domain error for [`GetMiningInfo`].
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetMiningInfoError {
    /// The validator is not ready to report mining statistics.
    #[error("validator not ready")]
    NotReady,
}

/// Fetch network-wide mining statistics: tip height, solution and hash rates,
/// difficulty, and the validator's health.
///
/// Reports the *network*, not a local miner. Zaino does not mine, and the
/// local-mining fields some validators include are not modelled.
///
/// Maps to `getmininginfo` over JSON-RPC.
pub trait GetMiningInfo: Send + Sync {
    /// Fetch network mining statistics.
    fn get_mining_info(
        &self,
    ) -> impl Future<Output = Result<MiningInfo, QueryError<GetMiningInfoError>>> + Send;
}
