//! The raw transaction read, per transaction-location placement.
//!
//! A raw transaction is two things: its bytes, and where it was mined. The
//! bytes always come from the validator — no local index holds them, and a txid
//! names one immutable object, so relaying them cannot disagree with the pinned
//! view. Where it was mined is a chain-position fact, and that is what the
//! placement decides:
//!
//! ```text
//! Local:     location = where the pinned tiers hold the transaction
//! otherwise: location = where the validator says it is, as of its own tip
//! ```
//!
//! Under [`Local`] the answer describes the snapshot it was read through. The
//! head is asked first, since a position in the volatile window is the newer
//! fact, then the finalised store. A transaction neither tier holds is, for
//! this view, not mined: the validator may already have it in a block above the
//! pinned tip, and reporting that height would name a block the snapshot does
//! not contain.

use std::future::Future;

use zaino_primitives::types::{RawTransaction, TransactionId, TransactionLocation};
use zaino_service::error::TxReadError;
use zaino_service::{RawTransactionRead, TransactionLocationRead};
use zaino_source::GetTransaction;

use super::EngineSnapshot;
use crate::chain_view::{ChainTier, ChainViewSnapshot};
use crate::passthrough::PassthroughProvider;
use crate::routing::{Local, Passthrough, Routing, Withheld};

/// How a transaction-location placement answers the raw transaction read over
/// the providers `(F, N, Src)`.
pub trait RawTransactionPlacement<F, N, Src>: Send + Sync + 'static {
    fn raw_transaction(
        local: &ChainViewSnapshot<F, N>,
        passthrough: &PassthroughProvider<Src>,
        id: TransactionId,
    ) -> impl Future<Output = Result<Option<RawTransaction>, TxReadError>> + Send;
}

impl<F, N, Src, R> RawTransactionRead for EngineSnapshot<F, N, Src, R>
where
    F: ChainTier,
    N: ChainTier,
    Src: Send + Sync + 'static,
    R: Routing,
    R::TransactionLocation: RawTransactionPlacement<F, N, Src>,
{
    async fn raw_transaction(
        &self,
        id: TransactionId,
    ) -> Result<Option<RawTransaction>, TxReadError> {
        static READ: crate::emit::Read = crate::emit::Read::new("raw_transaction");
        crate::emit::measured(
            &READ,
            crate::emit::Source::alongside_validator(
                <R::TransactionLocation as crate::routing::Placement>::KIND,
            ),
            async move {
                R::TransactionLocation::raw_transaction(self.local(), self.passthrough(), id).await
            },
        )
        .await
    }
}

impl<F, N, Src> RawTransactionPlacement<F, N, Src> for Local
where
    F: ChainTier + TransactionLocationRead,
    N: ChainTier + TransactionLocationRead,
    Src: GetTransaction + Send + Sync + 'static,
{
    async fn raw_transaction(
        local: &ChainViewSnapshot<F, N>,
        passthrough: &PassthroughProvider<Src>,
        id: TransactionId,
    ) -> Result<Option<RawTransaction>, TxReadError> {
        let Some(relayed) = passthrough.raw_transaction(id).await? else {
            return Ok(None);
        };
        let head = local.non_finalised().transaction_location(id).await?;
        let location = match head {
            Some(location @ TransactionLocation::BestChain(_)) => location,
            // The head has it on a displaced branch only, or not at all: the
            // finalised store's best chain decides before either is believed.
            Some(TransactionLocation::NonBestChain | TransactionLocation::Mempool) | None => {
                match local.finalised().transaction_location(id).await? {
                    Some(location) => location,
                    None => unlocated(head, &relayed.location),
                }
            }
        };
        Ok(Some(RawTransaction {
            data: relayed.data,
            location,
        }))
    }
}

/// Where a transaction neither tier holds on its best chain is, for the pinned
/// view. A displaced-branch sighting, by the head or by the validator, stands.
/// Anything else is unmined here, even when the validator already has it in a
/// block: that block is above or off the pinned chain.
fn unlocated(
    head: Option<TransactionLocation>,
    relayed: &TransactionLocation,
) -> TransactionLocation {
    match (head, relayed) {
        (Some(TransactionLocation::NonBestChain), _) | (_, TransactionLocation::NonBestChain) => {
            TransactionLocation::NonBestChain
        }
        (_, TransactionLocation::BestChain(_) | TransactionLocation::Mempool) => {
            TransactionLocation::Mempool
        }
    }
}

/// The validator's whole answer, relayed as is.
async fn relayed<Src>(
    passthrough: &PassthroughProvider<Src>,
    id: TransactionId,
) -> Result<Option<RawTransaction>, TxReadError>
where
    Src: GetTransaction + Send + Sync + 'static,
{
    passthrough.raw_transaction(id).await
}

impl<F, N, Src> RawTransactionPlacement<F, N, Src> for Passthrough
where
    F: ChainTier,
    N: ChainTier,
    Src: GetTransaction + Send + Sync + 'static,
{
    async fn raw_transaction(
        _local: &ChainViewSnapshot<F, N>,
        passthrough: &PassthroughProvider<Src>,
        id: TransactionId,
    ) -> Result<Option<RawTransaction>, TxReadError> {
        relayed(passthrough, id).await
    }
}

/// A deployment that withholds transaction location builds no local lookup, so
/// the location that rides on the raw transaction is the validator's.
impl<F, N, Src> RawTransactionPlacement<F, N, Src> for Withheld
where
    F: ChainTier,
    N: ChainTier,
    Src: GetTransaction + Send + Sync + 'static,
{
    async fn raw_transaction(
        _local: &ChainViewSnapshot<F, N>,
        passthrough: &PassthroughProvider<Src>,
        id: TransactionId,
    ) -> Result<Option<RawTransaction>, TxReadError> {
        relayed(passthrough, id).await
    }
}
