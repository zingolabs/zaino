//! Query: estimate the network solution rate.

use std::future::Future;

use zaino_primitives::types::Height;

use super::QueryError;

/// Domain error for [`GetNetworkSolPs`].
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetNetworkSolPsError {
    /// The validator has no chain tip to measure against.
    #[error("validator not ready")]
    NotReady,
}

/// Estimate the network solution rate in solutions per second, averaged over a
/// window of blocks ending at a given height.
///
/// Returns a bare `u64` rather than a newtype: the value is a rate with no
/// invariant to protect and no other quantity it could be confused with at
/// these call sites.
pub trait GetNetworkSolPs: Send + Sync {
    /// Estimate the network solution rate.
    ///
    /// `blocks` is the averaging window; `height` the block to measure at.
    /// `None` for either asks the validator for its own default — the window
    /// and the tip respectively.
    fn get_network_sol_ps(
        &self,
        blocks: Option<u32>,
        height: Option<Height>,
    ) -> impl Future<Output = Result<u64, QueryError<GetNetworkSolPsError>>> + Send;
}
