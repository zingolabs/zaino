//! Spend status, per placement.
//!
//! Dispatched through [`SpendPlacement`] on the placement marker. Only
//! [`Local`] implements it: the validator has no port that answers "who spent
//! this outpoint", so `Spend = Remote` has no impl. Local merges across the
//! seam — the head first, since a spend in the volatile window is the newer
//! fact, then the finalised store.

use std::future::Future;

use zaino_chainview::ChainViewSnapshot;
use zaino_core::{Outpoint, SpendStatus};
use zaino_service::error::SpendReadError;
use zaino_service::routing::{Local, Routing};
use zaino_service::{ChainSegment, CompactBlockRead, SpendRead};

use super::ComposedSnapshot;
use crate::remote::RemoteChainView;

/// How a placement answers spend status over the providers `(F, N, Src)`.
pub trait SpendPlacement<F, N, Src>: Send + Sync + 'static {
    fn spend_status(
        local: &ChainViewSnapshot<F, N>,
        remote: &RemoteChainView<Src>,
        outpoint: Outpoint,
    ) -> impl Future<Output = Result<SpendStatus, SpendReadError>> + Send;
}

impl<F, N, Src, R> SpendRead for ComposedSnapshot<F, N, Src, R>
where
    F: ChainSegment + CompactBlockRead,
    N: ChainSegment + CompactBlockRead,
    Src: Send + Sync + 'static,
    R: Routing,
    R::Spend: SpendPlacement<F, N, Src>,
{
    async fn spend_status(&self, outpoint: Outpoint) -> Result<SpendStatus, SpendReadError> {
        R::Spend::spend_status(self.local(), self.remote(), outpoint).await
    }
}

impl<F, N, Src> SpendPlacement<F, N, Src> for Local
where
    F: ChainSegment + CompactBlockRead + SpendRead,
    N: ChainSegment + CompactBlockRead + SpendRead,
    Src: Send + Sync + 'static,
{
    async fn spend_status(
        local: &ChainViewSnapshot<F, N>,
        _remote: &RemoteChainView<Src>,
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
}
