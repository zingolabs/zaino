//! Spend status, per placement.
//!
//! Dispatched through [`SpendPlacement`] on the placement marker. Only
//! [`Local`] implements it: the validator has no port that answers "who spent
//! this outpoint", so `Spend = Passthrough` has no impl. Local merges across the
//! seam — the head first, since a spend in the volatile window is the newer
//! fact, then the finalised store.

use std::future::Future;

use crate::chain_view::ChainTier;
use crate::chain_view::ChainViewSnapshot;
use crate::routing::{Local, Routing};
use zaino_primitives::types::{Outpoint, TransparentSpend};
use zaino_service::SpendRead;
use zaino_service::SpendStatus;
use zaino_service::error::SpendReadError;

use super::EngineSnapshot;
use crate::passthrough::PassthroughProvider;

/// How a placement answers spend status over the providers `(F, N, Src)`.
pub trait SpendPlacement<F, N, Src>: Send + Sync + 'static {
    fn spend_status(
        local: &ChainViewSnapshot<F, N>,
        passthrough: &PassthroughProvider<Src>,
        outpoint: Outpoint,
    ) -> impl Future<Output = Result<SpendStatus, SpendReadError>> + Send;

    fn spend_info(
        local: &ChainViewSnapshot<F, N>,
        passthrough: &PassthroughProvider<Src>,
        outpoint: Outpoint,
    ) -> impl Future<Output = Result<Option<TransparentSpend>, SpendReadError>> + Send;
}

impl<F, N, Src, R> SpendRead for EngineSnapshot<F, N, Src, R>
where
    F: ChainTier,
    N: ChainTier,
    Src: Send + Sync + 'static,
    R: Routing,
    R::Spend: SpendPlacement<F, N, Src>,
{
    async fn spend_status(&self, outpoint: Outpoint) -> Result<SpendStatus, SpendReadError> {
        R::Spend::spend_status(self.local(), self.passthrough(), outpoint).await
    }

    async fn spend_info(
        &self,
        outpoint: Outpoint,
    ) -> Result<Option<TransparentSpend>, SpendReadError> {
        R::Spend::spend_info(self.local(), self.passthrough(), outpoint).await
    }
}

impl<F, N, Src> SpendPlacement<F, N, Src> for Local
where
    F: ChainTier + SpendRead,
    N: ChainTier + SpendRead,
    Src: Send + Sync + 'static,
{
    async fn spend_status(
        local: &ChainViewSnapshot<F, N>,
        _passthrough: &PassthroughProvider<Src>,
        outpoint: Outpoint,
    ) -> Result<SpendStatus, SpendReadError> {
        // The head knows spends in its window and outputs created there; for
        // anything else it answers `NoSuchOutput`, and the finalised store is
        // asked. An `Unspent` from the head is authoritative only for outputs
        // the head created; for an output it did not create it cannot say the
        // store has not seen a spend, so the store is asked then too.
        match local.non_finalised().spend_status(outpoint).await? {
            spent @ (SpendStatus::Spent { .. } | SpendStatus::SpentSpenderUnknown) => Ok(spent),
            SpendStatus::Unspent | SpendStatus::NoSuchOutput => {
                local.finalised().spend_status(outpoint).await
            }
        }
    }

    async fn spend_info(
        local: &ChainViewSnapshot<F, N>,
        _passthrough: &PassthroughProvider<Src>,
        outpoint: Outpoint,
    ) -> Result<Option<TransparentSpend>, SpendReadError> {
        // The head holds the newer fact: a spend in the volatile window of an
        // output created anywhere below it. It answers `None` for an outpoint it
        // saw no spend of — including one created in its window but still
        // unspent — so the finalised store is asked then, which reports a spend
        // at or below the watermark. The seam case (created below, spent above)
        // is covered because the head recognises a spend from the outpoint
        // alone, without the output that created it.
        match local.non_finalised().spend_info(outpoint).await? {
            Some(spend) => Ok(Some(spend)),
            None => local.finalised().spend_info(outpoint).await,
        }
    }
}
