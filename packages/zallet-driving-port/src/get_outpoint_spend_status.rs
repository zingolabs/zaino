//! Capability: the spend status of one transparent output.

use std::future::Future;

use crate::error::PortError;
use crate::outpoint::Outpoint;
use crate::spend_status::SpendStatus;

/// Domain error for [`GetOutpointSpendStatus`].
///
/// Empty: an outpoint the pinned view does not know is an answer
/// (`Ok(None)`), not a rejection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum GetOutpointSpendStatusError {}

/// The pinned view's spend status of one transparent output.
///
/// Every engine can implement this without a per-outpoint spend
/// index: spentness comes from the UTXO set, and an engine that
/// cannot name the spender answers
/// [`SpendStatus::SpentSpenderUnknown`]. This is why the capability
/// carries no feature gate — Zallet's `spend-index` build switch
/// selects a detection strategy, not a different contract.
pub trait GetOutpointSpendStatus: Send + Sync {
    /// The spend status of `outpoint`, or `None` when no transaction
    /// in the pinned view created such an output.
    fn get_outpoint_spend_status(
        &self,
        outpoint: Outpoint,
    ) -> impl Future<Output = Result<Option<SpendStatus>, PortError<GetOutpointSpendStatusError>>> + Send;
}
