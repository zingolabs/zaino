//! Pool-decomposed transaction reads — always passthrough.
//!
//! The decoded transaction can only come from the validator: the store holds no
//! transaction bytes, and the pool decomposition (transparent, sapling, orchard,
//! ironwood) needs the validator's chain library, which this crate must never
//! depend on. The source port returns the decoded [`Transaction`] already, so
//! this routes straight to it.
//!
//! Within one [`transaction_status`](TransactionRead::transaction_status) call the
//! status is read off the [`location`](zaino_source::DecodedTransaction::location)
//! of the single fetch that also yields the transaction. A mempool transaction is
//! [`TxStatus::Unknown`], never [`TxStatus::Orphaned`]: it is not mined and it has
//! not been reorged out, and collapsing those two is how a consumer would wrongly
//! conclude a pending transaction had failed. An absent transaction is
//! [`TxStatus::Unknown`] too.

use crate::chain_view::ChainTier;
use crate::routing::Routing;
use zaino_primitives::types::{Transaction, TransactionId, TransactionLocation};
use zaino_service::error::TxReadError;
use zaino_service::{TransactionRead, TxStatus};
use zaino_source::GetTransactionVerbose;

use super::EngineSnapshot;

impl<F, N, Src, R> TransactionRead for EngineSnapshot<F, N, Src, R>
where
    F: ChainTier,
    N: ChainTier,
    Src: GetTransactionVerbose + Send + Sync + 'static,
    R: Routing,
{
    async fn transaction(&self, id: TransactionId) -> Result<Option<Transaction>, TxReadError> {
        // Passthrough: the decoded form, dropping the location the status read
        // keeps. Placement never varies, so there is no placement-trait dispatch.
        Ok(self
            .passthrough()
            .transaction(id)
            .await?
            .map(|decoded| decoded.transaction))
    }

    async fn transaction_status(&self, id: TransactionId) -> Result<TxStatus, TxReadError> {
        let status = match self.passthrough().transaction(id).await? {
            None => TxStatus::Unknown,
            Some(decoded) => match decoded.location {
                TransactionLocation::BestChain(height) => TxStatus::Mined(height),
                TransactionLocation::NonBestChain => TxStatus::Orphaned,
                TransactionLocation::Mempool => TxStatus::Unknown,
            },
        };
        Ok(status)
    }
}
