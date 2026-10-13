//! Where a transaction was mined, over the finalised range.
//!
//! One keyed lookup in the `txid_location` index. The finalised store is a
//! single chain by construction, so every transaction it locates is on the best
//! chain.

use zaino_indexes::capabilities::local::{self, Backs};
use zaino_indexes::indexes::txid_location::{self, TxidLocationIndex};
use zaino_persistence::Backend;
use zaino_primitives::types::{Height, TransactionId, TransactionLocation};
use zaino_service::error::TxReadError;
use zaino_service::{Capability, TransactionLocationRead};

use crate::{read_keyed, serviceability_gate, StoreSnapshot};

/// The store locates transactions wherever its index set builds the
/// `txid_location` index.
impl<B, M> TransactionLocationRead for StoreSnapshot<B, M>
where
    B: Backend + 'static,
    M: Backs<local::TransactionLocation>,
{
    async fn transaction_location(
        &self,
        id: TransactionId,
    ) -> Result<Option<TransactionLocation>, TxReadError> {
        let reader = self
            .backend
            .reader()
            .map_err(|e| TxReadError::Fatal(format!("open reader: {e}")))?;
        // The index is scattered-key, so a bulk load leaves it unreadable until
        // it finishes: answering "not here" then would be a false miss.
        if let Some(capability) = serviceability_gate::<B>(
            &reader,
            &[txid_location::ID.into()],
            Capability::TransactionLocation,
        )
        .map_err(|e| TxReadError::Transient(format!("transaction-location readiness: {e}")))?
        {
            return Err(TxReadError::NotServiceable(capability));
        }

        let Some(location) =
            read_keyed::<TxidLocationIndex, B>(&reader, txid_location::ID.into(), &id)
                .map_err(|t| TxReadError::Transient(t.0))?
        else {
            return Ok(None);
        };
        let height = u32::try_from(location.height.value())
            .ok()
            .and_then(|height| Height::try_from(height).ok())
            .ok_or_else(|| {
                TxReadError::Fatal(format!(
                    "txid_location holds an out-of-range height {}",
                    location.height.value()
                ))
            })?;
        // The index is written in the same batch as the watermark, but this view
        // was pinned earlier: an entry above the pinned watermark is not part of
        // it yet.
        match self.watermark {
            Some(watermark) if height <= watermark => {
                Ok(Some(TransactionLocation::BestChain(height)))
            }
            _ => Ok(None),
        }
    }
}
